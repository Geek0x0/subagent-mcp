//! Provider wire differential scenarios (Task 27).
//!
//! The Rust adapter runs against a local recording mock that answers with the
//! checked-in SSE fixtures, and the recorded request plus the parsed turn are
//! compared to goldens captured from the pinned Go 0.13.1 binary
//! (`tests/fixtures/wire/`, `tests/fixtures/mcp/`, `tests/fixtures/rollout/`).
//! The goldens stand alone: nothing here reads Go source or spawns Go code.
//!
//! Compared per the global constraints: request method/path/headers/body,
//! result text, usage, and per-turn events with consecutive
//! `agent_message_delta` collapsed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use subagent_mcp::agent::{
    BuiltinToolExecutor, DEFAULT_SYSTEM_PROMPT, Emitter, RunHooks, Runner, SessionManager,
    SessionOptions, ToolExecutor,
};
use subagent_mcp::config::{Api, AutoConfig, Config, ModelConfig, ProviderConfig};
use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::provider::{Agent, DeltaSink, Message, Role, ToolCall, TurnRequest};
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;

use crate::support::golden::{self, read_jsonl};
use crate::support::mock_http::MockHttp;
use crate::support::normalize::NormalizeContext;

const TEXT_STOP: &str = include_str!("../fixtures/sse/chat/text_stop_usage.sse");
const TOOL_CALLS: &str = include_str!("../fixtures/sse/chat/tool_calls_indexed.sse");
const PATCH_CALL: &str = include_str!("../fixtures/sse/chat/apply_patch_add.sse");
const RESPONSES_TEXT: &str = include_str!("../fixtures/sse/responses/text_completed.sse");
const MESSAGES_TEXT: &str =
    include_str!("../fixtures/sse/messages/stop_reason_without_message_stop.sse");

const ENV_KEY: &str = "SUBAGENT_DIFFERENTIAL_MOCK_KEY";
const AUTO_ENV_KEY: &str = "SUBAGENT_DIFFERENTIAL_AUTO_KEY";
const KEY_VALUE: &str = "mock-key";

/// Serialize scenarios that mutate process state (env vars, the shared codex
/// record file).
static SCENARIO_LOCK: Mutex<()> = Mutex::new(());

fn set_mock_key() {
    // SAFETY: env mutation is serialized under SCENARIO_LOCK for the whole
    // test; the values are constants, so nothing needs restoring.
    unsafe { std::env::set_var(ENV_KEY, KEY_VALUE) };
}

/// Build the adapter for `api` directly: the factory registry is populated
/// by `SubagentTools::new`, which these scenarios do not construct.
fn build_adapter(api: Api, base_url: &str) -> Arc<dyn subagent_mcp::provider::Provider> {
    let cfg = provider_config(api, base_url);
    let name = "mock";
    match api {
        Api::ChatCompletions => Arc::new(
            subagent_mcp::provider::ChatCompletions::new(name, &cfg).expect("chat adapter"),
        ),
        Api::Responses => {
            Arc::new(subagent_mcp::provider::Responses::new(name, &cfg).expect("responses adapter"))
        }
        Api::Messages => {
            Arc::new(subagent_mcp::provider::Messages::new(name, &cfg).expect("messages adapter"))
        }
        Api::CodexAppServer => Arc::new(
            subagent_mcp::provider::codex::CodexAdapter::new(name, &cfg).expect("codex adapter"),
        ),
    }
}

fn run_ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn outer_block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

fn provider_config(api: Api, base_url: &str) -> ProviderConfig {
    // `Config` load applies the messages-api default before validation (Go
    // Load does the same); mirror it so the adapter sees what production sees.
    let max_output_tokens = (api == Api::Messages).then_some(64000);
    ProviderConfig {
        api,
        base_url: base_url.to_string(),
        env_key: Some(ENV_KEY.to_string()),
        key: None,
        command: None,
        default_model: "mock-model".to_string(),
        models: vec![ModelConfig {
            id: "mock-model".to_string(),
            description: String::new(),
        }],
        effort_map: Default::default(),
        max_output_tokens,
        max_nudges: None,
        extra_body: None,
    }
}

/// Build the turn request the golden `01` wire capture was made with: the
/// default system prompt, one user message, and the four built-in tools.
fn first_turn_request() -> TurnRequest {
    TurnRequest {
        model: "mock-model".to_string(),
        effort: Some("high".to_string()),
        system: DEFAULT_SYSTEM_PROMPT.to_string(),
        messages: vec![Message {
            role: Role::User,
            text: "hi".to_string(),
            ..Default::default()
        }],
        tools: subagent_mcp::agent::builtin_tools(),
    }
}

struct RecordedDeltas(Arc<Mutex<Vec<String>>>);

fn delta_sink(log: &RecordedDeltas) -> DeltaSink {
    let log = Arc::clone(&log.0);
    Box::new(move |delta| log.lock().unwrap().push(delta))
}

/// Assert one recorded request equals its wire golden. One shared context
/// carries the live temp root plus every root extracted from the golden, so
/// both sides normalize to `<tmp>`.
fn assert_request_matches(golden_rel: &str, recorded: &Value, rust_context: &NormalizeContext) {
    let want = golden::read_json(golden_rel);
    let mut roots = Vec::new();
    golden::golden_temp_roots(&want, &mut roots);
    let mut golden_context = NormalizeContext::new();
    for root in roots {
        golden_context = golden_context.with_temp_root(root);
    }
    assert_eq!(
        golden::normalize_wire(recorded, rust_context),
        golden::normalize_wire(&want, &golden_context),
        "wire request differs from {golden_rel}\nrecorded: {recorded}"
    );
}

/// The usage captured in the golden rollout (`info.last_token_usage`).
fn golden_usage(rollout_rel: &str) -> (i64, i64, i64, i64, i64) {
    for line in read_jsonl(rollout_rel) {
        let payload = &line["payload"];
        if payload["type"] == "token_count" {
            let last = &payload["info"]["last_token_usage"];
            return (
                last["input_tokens"].as_i64().unwrap(),
                last["output_tokens"].as_i64().unwrap(),
                last["cached_input_tokens"].as_i64().unwrap(),
                last["reasoning_output_tokens"].as_i64().unwrap(),
                last["total_tokens"].as_i64().unwrap(),
            );
        }
    }
    panic!("no token_count line in {rollout_rel}")
}

fn golden_agent_message(transcript_rel: &str) -> String {
    golden::golden_events(&read_jsonl(transcript_rel))
        .into_iter()
        .find_map(|event| {
            (event["type"] == "agent_message")
                .then(|| event["message"].as_str().unwrap().to_string())
        })
        .unwrap_or_else(|| panic!("no agent_message event in {transcript_rel}"))
}

// ---------------------------------------------------------------------------
// Single-text-turn wire scenarios (chat-completions, responses, messages).
// ---------------------------------------------------------------------------

fn assert_text_turn(
    api: Api,
    sse: &str,
    wire_golden: &str,
    transcript_golden: &str,
    rollout_golden: &str,
) {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_mock_key();
    let mock = MockHttp::start();
    mock.route_all(&[sse.to_string()]);

    let adapter = build_adapter(api, &mock.url());
    let deltas = RecordedDeltas(Arc::new(Mutex::new(Vec::new())));
    let result = outer_block_on(adapter.turn(run_ctx(), first_turn_request(), delta_sink(&deltas)))
        .expect("turn succeeds");

    let requests = mock.recorded();
    assert_eq!(requests.len(), 1, "exactly one provider request");
    let context = NormalizeContext::new();
    assert_request_matches(wire_golden, &requests[0].to_golden(), &context);

    // The turn result matches the golden transcript's structured content.
    let want = golden::golden_result(&read_jsonl(transcript_golden), 2);
    assert_eq!(result.text, want["content"].as_str().unwrap());

    // Deltas collapse into the golden agent message.
    let collapsed = golden::collapse_deltas(
        deltas
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|delta| json!({"type": "agent_message_delta", "delta": delta}))
            .collect(),
    );
    assert_eq!(
        collapsed.len(),
        1,
        "one collapsed delta event: {collapsed:?}"
    );
    assert_eq!(
        collapsed[0]["delta"],
        golden_agent_message(transcript_golden)
    );

    // Usage matches the golden rollout's last_token_usage line.
    let (input, output, cached, reasoning, total) = golden_usage(rollout_golden);
    let usage = result.usage.expect("usage present");
    assert_eq!(
        (
            usage.input,
            usage.output,
            usage.cached,
            usage.reasoning,
            usage.total
        ),
        (input, output, cached, reasoning, total),
        "usage differs from {rollout_golden}"
    );
}

#[test]
fn differential_wire_chat_text_turn_matches_golden() {
    assert_text_turn(
        Api::ChatCompletions,
        TEXT_STOP,
        "wire/chat-turn/01-chat_completions.json",
        "mcp/chat-turn.jsonl",
        "rollout/chat-turn.jsonl",
    );
}

#[test]
fn differential_wire_responses_text_turn_matches_golden() {
    assert_text_turn(
        Api::Responses,
        RESPONSES_TEXT,
        "wire/responses-turn/01-responses.json",
        "mcp/responses-turn.jsonl",
        "rollout/responses-turn.jsonl",
    );
}

#[test]
fn differential_wire_messages_text_turn_matches_golden() {
    assert_text_turn(
        Api::Messages,
        MESSAGES_TEXT,
        "wire/messages-turn/01-v1_messages.json",
        "mcp/messages-turn.jsonl",
        "rollout/messages-turn.jsonl",
    );
}

// ---------------------------------------------------------------------------
// Tool-call and patch turns: the full Rust runner loop (adapter + built-in
// executor) against the mock, compared to the Go wire and event goldens per
// request/thread/turn.
// ---------------------------------------------------------------------------

struct CollectingEmitter(Arc<Mutex<Vec<Value>>>);

impl Emitter for CollectingEmitter {
    fn emit(&self, _thread_id: &str, event: Value) {
        self.0.lock().unwrap().push(event);
    }
}

fn always_approver() -> Arc<dyn Approver> {
    struct Always;
    #[async_trait::async_trait]
    impl Approver for Always {
        async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
            true
        }
    }
    Arc::new(Always)
}

/// The executor whose Landlock helper is the real binary, like production and
/// `tests/agent_tools.rs`; the libtest harness itself cannot dispatch
/// `__sandbox-exec`, so shell results would otherwise be helper errors.
fn real_executor() -> BuiltinToolExecutor {
    BuiltinToolExecutor::new(PathBuf::from(env!("CARGO_BIN_EXE_subagent-mcp")))
}

fn session_options(
    provider: Arc<dyn subagent_mcp::provider::Provider>,
    cwd: &Path,
) -> SessionOptions {
    // The captures ran with `sandbox = "workspace-write"` (the shell may run,
    // apply_patch may write), matching the Go capture's tool arguments.
    SessionOptions {
        provider,
        thread: None,
        model: "mock-model".to_string(),
        reasoning_effort: "high".to_string(),
        effort_sent: None,
        cwd: cwd.to_path_buf(),
        sandbox: Sandbox::WorkspaceWrite,
        approval: ApprovalPolicy::Never,
        system_prompt: String::new(),
        max_turns: 0,
        max_nudges: 0,
        writable_roots: vec![],
        max_history_bytes: 0,
        max_total_tokens: 0,
        run_deadline: None,
    }
}

/// One full agent-loop run. Returns the final text, the emitted events, and
/// the recorded provider requests in order.
async fn run_agent_loop_with_events(
    responses: &[String],
    prompt: &str,
    cwd: &Path,
    events: &Arc<Mutex<Vec<Value>>>,
) -> (Result<String, subagent_mcp::agent::AgentError>, Vec<Value>) {
    set_mock_key();
    let mock = MockHttp::start();
    mock.route_all(responses);
    let provider = build_adapter(Api::ChatCompletions, &mock.url());
    let manager = SessionManager::new();
    let session = manager
        .create(session_options(provider, cwd))
        .await
        .unwrap();
    let id = session.id.clone();
    let mut lease = manager.acquire(&id).await.unwrap();
    let hooks = RunHooks {
        emitter: Arc::new(CollectingEmitter(Arc::clone(events))),
        approver: always_approver(),
    };
    let runner = Runner {
        executor: Arc::new(real_executor()),
    };
    let result = runner
        .run_locked(run_ctx(), &mut lease, prompt, &hooks)
        .await;
    let requests = mock
        .recorded()
        .iter()
        .map(|request| request.to_golden())
        .collect();
    (result, requests)
}

fn assert_agent_loop_scenario(
    responses: &[&str],
    prompt: &str,
    seed_files: &[(&str, &str)],
    wire_goldens: &[&str],
    transcript_golden: &str,
) {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let cwd = tempfile::tempdir().unwrap();
    for (rel, content) in seed_files {
        let path = cwd.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    let responses: Vec<String> = responses.iter().map(|s| s.to_string()).collect();
    let events = Arc::new(Mutex::new(Vec::new()));
    let (result, requests) = outer_block_on(run_agent_loop_with_events(
        &responses,
        prompt,
        cwd.path(),
        &events,
    ));
    let text = result.expect("run succeeds");

    let rust_context = NormalizeContext::new().with_temp_root(cwd.path());
    let transcript = read_jsonl(transcript_golden);

    assert_eq!(requests.len(), wire_goldens.len(), "provider request count");
    for (golden_rel, request) in wire_goldens.iter().zip(&requests) {
        assert_request_matches(golden_rel, request, &rust_context);
    }

    let want = golden::golden_result(&transcript, 2);
    assert_eq!(text, want["content"].as_str().unwrap(), "final text");

    // Event comparison per turn, with consecutive deltas collapsed. The
    // golden side gets its own context seeded with the temp roots the capture
    // left inside the golden events (tool output lines embed the old cwd).
    let mut roots = Vec::new();
    let golden_events = golden::collapse_deltas(golden::golden_events(&transcript));
    for event in &golden_events {
        golden::golden_temp_roots(event, &mut roots);
    }
    let mut golden_context = NormalizeContext::new();
    for root in &roots {
        golden_context = golden_context.with_temp_root(root);
    }
    let got: Vec<Value> = golden::collapse_deltas(events.lock().unwrap().clone())
        .into_iter()
        .map(|event| golden::normalize_value(golden::blank_timing(event), &rust_context))
        .collect();
    let want_events: Vec<Value> = golden_events
        .into_iter()
        .map(|event| golden::normalize_value(golden::blank_timing(event), &golden_context))
        .collect();
    assert_eq!(
        got, want_events,
        "collapsed events differ from {transcript_golden}"
    );
}

#[test]
fn differential_wire_chat_tool_turn_matches_golden() {
    assert_agent_loop_scenario(
        &[TOOL_CALLS, TEXT_STOP],
        "list the files",
        &[("a.txt", "alpha\n"), ("src/b.txt", "beta\n")],
        &[
            "wire/chat-tool-turn/01-chat_completions.json",
            "wire/chat-tool-turn/02-chat_completions.json",
        ],
        "mcp/chat-tool-turn.jsonl",
    );
}

#[test]
fn differential_wire_chat_patch_turn_matches_golden() {
    assert_agent_loop_scenario(
        &[PATCH_CALL, TEXT_STOP],
        "add hello.txt",
        &[("a.txt", "alpha\n"), ("src/b.txt", "beta\n")],
        &[
            "wire/chat-patch-turn/01-chat_completions.json",
            "wire/chat-patch-turn/02-chat_completions.json",
        ],
        "mcp/chat-patch-turn.jsonl",
    );

    // The filesystem effect matches the captured tree, modes and content.
    let cwd = apply_patch_case();
    assert_eq!(
        tree_of(cwd.path()),
        golden::read_text("patch/expected-tree.txt")
    );
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("hello.txt")).unwrap(),
        golden::read_text("patch/hello.txt")
    );
}

/// Apply the checked-in patch case with the Rust built-in executor in a fresh
/// seeded directory (the exact state the Go capture started from).
fn apply_patch_case() -> tempfile::TempDir {
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::create_dir_all(cwd.path().join("src")).unwrap();
    std::fs::write(cwd.path().join("src/b.txt"), "beta\n").unwrap();

    outer_block_on(async {
        let executor = real_executor();
        let manager = SessionManager::new();
        let provider = build_adapter(Api::ChatCompletions, "http://127.0.0.1:1");
        let mut opts = session_options(provider, cwd.path());
        opts.sandbox = Sandbox::WorkspaceWrite;
        let session = manager.create(opts).await.unwrap();
        let id = session.id.clone();
        let mut lease = manager.acquire(&id).await.unwrap();
        let hooks = RunHooks {
            emitter: Arc::new(CollectingEmitter(Arc::new(Mutex::new(Vec::new())))),
            approver: always_approver(),
        };
        let call = ToolCall {
            id: "call_p".to_string(),
            name: "apply_patch".to_string(),
            arguments: json!({"patch": golden::read_text("patch/case.patch")}).to_string(),
        };
        let result = executor
            .execute(&run_ctx(), &mut lease, &call, &hooks, "turn-diff")
            .await;
        assert!(!result.is_error, "patch applies: {:?}", result.output);
    });
    cwd
}

/// Deterministic pre-order walk with modes, matching the capture driver.
fn tree_of(root: &Path) -> String {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                format!(
                    "0o{:03o}",
                    path.metadata().unwrap().permissions().mode() & 0o777
                )
            };
            if path.is_dir() {
                out.push(format!("d {mode} {rel}"));
                walk(&path, root, out);
            } else {
                out.push(format!("f {mode} {rel}"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.join("\n") + "\n"
}

// ---------------------------------------------------------------------------
// Auto (Jev) selection with a mock decision endpoint.
// ---------------------------------------------------------------------------

#[test]
fn differential_auto_selection_matches_jev_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_mock_key();
    // SAFETY: under SCENARIO_LOCK, constant value.
    unsafe { std::env::set_var(AUTO_ENV_KEY, "auto-key") };

    let mock = MockHttp::start();
    let jev = r#"{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":"c2","confidence":0.9,"probabilities":{"c1":0.1,"c2":0.9}}},"usage":{"input_tokens":42,"output_tokens":3}}"#;
    mock.route("/v1/systemone", &[jev.to_string()]);

    let mut providers = BTreeMap::new();
    let mut alpha = provider_config(Api::ChatCompletions, &mock.url());
    alpha.default_model = "m-fast".to_string();
    alpha.models = vec![ModelConfig {
        id: "m-fast".to_string(),
        description: String::new(),
    }];
    providers.insert("alpha".to_string(), alpha);
    let mut bravo = provider_config(Api::ChatCompletions, &mock.url());
    bravo.default_model = "m-pro".to_string();
    bravo.models = vec![ModelConfig {
        id: "m-pro".to_string(),
        description: String::new(),
    }];
    providers.insert("bravo".to_string(), bravo);
    let config = Config {
        path: Path::new("/tmp/differential-auto.toml").to_path_buf(),
        providers,
        auto: Some(AutoConfig {
            base_url: mock.url(),
            url: String::new(),
            env_key: Some(AUTO_ENV_KEY.to_string()),
            key: None,
            model: "jev-latest".to_string(),
            timeout_seconds: 5,
        }),
    };

    let cwd = tempfile::tempdir().unwrap();
    let manager = SessionManager::new();
    let selection = outer_block_on(subagent_mcp::mcp::auto::resolve_auto(
        &run_ctx(),
        &config,
        &manager,
        "fix the parser",
        cwd.path(),
    ))
    .expect("auto selection succeeds");

    // The Jev request matches the wire golden; no provider turn ran.
    let requests = mock.recorded();
    assert_eq!(requests.len(), 1, "one Jev request, no provider turn");
    let context = NormalizeContext::new().with_temp_root(cwd.path());
    assert_request_matches(
        "wire/auto-turn/01-v1_systemone.json",
        &requests[0].to_golden(),
        &context,
    );

    // Selection metadata matches the golden transcript's `auto` object.
    let want = golden::golden_result(&read_jsonl("mcp/auto-turn.jsonl"), 2)["auto"].clone();
    assert_eq!(selection.provider, "bravo");
    assert_eq!(selection.model, "m-pro");
    let meta = selection.meta();
    assert_eq!(meta["judge_model"], want["judge_model"]);
    assert_eq!(meta["confidence"], want["confidence"]);
    assert_eq!(meta["input_tokens"], want["input_tokens"]);
    assert_eq!(meta["output_tokens"], want["output_tokens"]);
    assert_eq!(meta["probabilities"], want["probabilities"]);
    assert!(meta.get("skipped").is_none());
    assert!(meta.get("omitted").is_none());
}

// ---------------------------------------------------------------------------
// Codex fake app-server: the Rust client against the checked-in scripted peer,
// compared to the request record the pinned Go binary produced.
// ---------------------------------------------------------------------------

#[test]
fn differential_codex_turn_matches_record_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let script = golden::fixture("codex/app-server-turn.sh");
    // Per-run record path (review finding 4): the script prefers
    // FAKE_CODEX_RECORD and only falls back to its fixed /tmp file when the
    // environment is scrubbed, so concurrent runs can never share a record.
    // The dialer passes the parent environment through minus the scrub table,
    // which never contains FAKE_CODEX_RECORD. The guard restores the previous
    // value so no ambient variable outlives the scenario.
    let record_dir = tempfile::tempdir().unwrap();
    let record_path = record_dir.path().join("record.jsonl");
    let _record_env = crate::support::EnvGuard::set("FAKE_CODEX_RECORD", record_path.as_os_str());

    let mut config = provider_config(Api::CodexAppServer, "");
    config.command = Some(script.to_str().unwrap().to_string());
    config.default_model = "gpt-6-astra".to_string();
    config.models = vec![ModelConfig {
        id: "gpt-6-astra".to_string(),
        description: String::new(),
    }];
    let adapter =
        subagent_mcp::provider::codex::CodexAdapter::new("codex", &config).expect("adapter builds");

    let cwd = tempfile::tempdir().unwrap();
    struct Always;
    #[async_trait::async_trait]
    impl Approver for Always {
        async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
            true
        }
    }
    let text = outer_block_on(async {
        let thread = adapter
            .start_thread(
                &run_ctx(),
                subagent_mcp::provider::ThreadOptions {
                    model: "gpt-6-astra".to_string(),
                    cwd: cwd.path().to_path_buf(),
                    sandbox: Sandbox::ReadOnly,
                    approval_policy: ApprovalPolicy::Never,
                    base_instructions: None,
                    developer_instructions: None,
                    writable_roots: vec![],
                    ephemeral: false,
                },
            )
            .await
            .expect("thread starts");
        let callbacks = subagent_mcp::provider::ThreadCallbacks {
            emit: Arc::new(|_, _| {}),
            approve: Arc::new(Always),
        };
        let text = thread
            .run(
                run_ctx(),
                "hi".to_string(),
                Some("high".to_string()),
                callbacks,
            )
            .await
            .expect("turn completes");
        thread.close().await.unwrap();
        text
    });

    assert_eq!(text, "hello from codex", "final text");

    // The recorded client-to-peer requests match the Go golden line for line.
    let record = std::fs::read_to_string(&record_path).expect("codex record exists");
    let got: Vec<Value> = record
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("record line parses"))
        .collect();
    let want = read_jsonl("codex/turn-record.jsonl");
    assert_eq!(got.len(), want.len(), "record line count: {record}");
    // Each side carries its own (deleted) temp cwd; collect the golden's
    // roots from the record itself so both normalize to `<tmp>`.
    let rust_context = NormalizeContext::new().with_temp_root(cwd.path());
    let mut roots = Vec::new();
    for line in &want {
        golden::golden_temp_roots(line, &mut roots);
    }
    let mut golden_context = NormalizeContext::new();
    for root in roots {
        golden_context = golden_context.with_temp_root(root);
    }
    for (index, (got, want)) in got.iter().zip(&want).enumerate() {
        assert_eq!(
            golden::normalize_value(got.clone(), &rust_context),
            golden::normalize_value(want.clone(), &golden_context),
            "codex record line {index} differs"
        );
    }
}
