//! Agent-loop differential scenarios (Task 27): nudges, budgets, and
//! cancellation at the runner level, plus full MCP transcript replays of the
//! captured scenarios against the checked-in goldens.
//!
//! The goldens under `tests/fixtures/mcp/` are complete Go 0.13.1 output
//! streams (responses and `subagent/event` notifications in arrival order)
//! captured from the pinned binary; they stand alone after the Go tree is
//! deleted. The progress replay additionally checks in
//! `mcp/chat-progress.jsonl`, a stream captured with `_meta.progressToken`,
//! so `notifications/progress` frames (token, per-call sequence, mapped
//! message) are compared causally per call (review finding 1). Every replay
//! also compares the rollout JSONL the server wrote under `CODEX_HOME` —
//! server-written `session_meta` line included — to the golden rollout for
//! the same scenario (review findings 2 and 3). Every comparison is per
//! request/thread/turn, collapses consecutive `agent_message_delta` events,
//! and normalizes volatile fields.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use subagent_mcp::agent::{
    BuiltinToolExecutor, Emitter, RunHooks, Runner, SessionManager, SessionOptions,
};
use subagent_mcp::config::{Api, ModelConfig, ProviderConfig};
use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;

use crate::support::golden::{self, read_jsonl};
use crate::support::mock_http::MockHttp;
use crate::support::normalize::NormalizeContext;
use crate::support::{McpProcess, reference_or_skip, rust_binary};

const TEXT_STOP: &str = include_str!("../fixtures/sse/chat/text_stop_usage.sse");
const EMPTY_STOP: &str = include_str!("../fixtures/sse/chat/empty_stop_usage.sse");
const TOOL_CALLS: &str = include_str!("../fixtures/sse/chat/tool_calls_indexed.sse");
const PATCH_CALL: &str = include_str!("../fixtures/sse/chat/apply_patch_add.sse");
const FILES_CALLS: &str = include_str!("../fixtures/sse/chat/tool_calls_files.sse");
const RESPONSES_TEXT: &str = include_str!("../fixtures/sse/responses/text_completed.sse");
const MESSAGES_TEXT: &str =
    include_str!("../fixtures/sse/messages/stop_reason_without_message_stop.sse");

const ENV_KEY: &str = "SUBAGENT_DIFFERENTIAL_MOCK_KEY";
const KEY_VALUE: &str = "mock-key";

/// Serializes every scenario that touches process state (env vars, adapters,
/// the shared codex record file).
static SCENARIO_LOCK: Mutex<()> = Mutex::new(());

fn set_mock_key() {
    // SAFETY: under SCENARIO_LOCK; constant value, nothing to restore.
    unsafe { std::env::set_var(ENV_KEY, KEY_VALUE) };
}

fn run_ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn provider_config(base_url: &str, default_model: &str) -> ProviderConfig {
    ProviderConfig {
        api: Api::ChatCompletions,
        base_url: base_url.to_string(),
        env_key: Some(ENV_KEY.to_string()),
        key: None,
        command: None,
        default_model: default_model.to_string(),
        models: vec![ModelConfig {
            id: default_model.to_string(),
            description: String::new(),
        }],
        effort_map: Default::default(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    }
}

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

/// Build the normalization contexts for a golden-transcript comparison: the
/// live side gets the live temp roots, the golden side every `/tmp` root the
/// capture left inside the golden stream.
fn golden_context_for(golden: &[Value]) -> NormalizeContext {
    let mut roots = Vec::new();
    for frame in golden {
        golden::golden_temp_roots(frame, &mut roots);
    }
    let mut context = NormalizeContext::new();
    for root in roots {
        context = context.with_temp_root(root);
    }
    context
}

fn normalize_frames(frames: Vec<Value>, context: &NormalizeContext) -> Vec<Value> {
    frames
        .into_iter()
        .map(|frame| golden::normalize_value(golden::blank_timing(frame), context))
        .collect()
}

// ---------------------------------------------------------------------------
// Runner-level nudge / budget / cancellation.
// ---------------------------------------------------------------------------

async fn run_with(
    responses: &[String],
    prompt: &str,
    cwd: &Path,
    options: impl FnOnce(&mut SessionOptions),
    events: &Arc<Mutex<Vec<Value>>>,
) -> Result<String, subagent_mcp::agent::AgentError> {
    set_mock_key();
    let mock = MockHttp::start();
    mock.route_all(responses);
    let provider = Arc::new(
        subagent_mcp::provider::ChatCompletions::new(
            "mock",
            &provider_config(&mock.url(), "mock-model"),
        )
        .expect("adapter builds"),
    );
    let mut opts = SessionOptions {
        provider,
        thread: None,
        model: "mock-model".to_string(),
        reasoning_effort: "high".to_string(),
        effort_sent: None,
        cwd: cwd.to_path_buf(),
        sandbox: Sandbox::ReadOnly,
        approval: ApprovalPolicy::Never,
        system_prompt: String::new(),
        max_turns: 0,
        max_nudges: 0,
        writable_roots: vec![],
        max_history_bytes: 0,
        max_total_tokens: 0,
        run_deadline: None,
    };
    options(&mut opts);
    let manager = SessionManager::new();
    let session = manager.create(opts).await.unwrap();
    let id = session.id.clone();
    let mut lease = manager.acquire(&id).await.unwrap();
    let hooks = RunHooks {
        emitter: Arc::new(CollectingEmitter(Arc::clone(events))),
        approver: always_approver(),
    };
    let runner = Runner {
        executor: Arc::new(BuiltinToolExecutor::default()),
    };
    runner
        .run_locked(run_ctx(), &mut lease, prompt, &hooks)
        .await
}

fn assert_events_match_golden(events: &[Value], transcript_golden: &str, live_roots: &[&Path]) {
    let transcript = read_jsonl(transcript_golden);
    assert_events_match_golden_transcript(events, &transcript, transcript_golden, live_roots);
}

/// Compare against the events of the transcript AFTER the first tools/call
/// result (the second run of a two-run scenario).
fn assert_events_match_golden_after_first_result(
    events: &[Value],
    transcript_golden: &str,
    live_roots: &[&Path],
) {
    let transcript = read_jsonl(transcript_golden);
    let split = transcript
        .iter()
        .position(|frame| frame.get("id") == Some(&json!(2)) && frame.get("result").is_some())
        .expect("golden has the first tools/call result");
    let rest = &transcript[split + 1..];
    assert_events_match_golden_transcript(events, rest, transcript_golden, live_roots);
}

fn assert_events_match_golden_transcript(
    events: &[Value],
    transcript: &[Value],
    transcript_golden: &str,
    live_roots: &[&Path],
) {
    let golden_context = golden_context_for(transcript);
    let mut rust_context = NormalizeContext::new();
    for root in live_roots {
        rust_context = rust_context.with_temp_root(root);
    }
    let got: Vec<Value> = golden::collapse_deltas(events.to_vec())
        .into_iter()
        .map(|event| golden::normalize_value(golden::blank_timing(event), &rust_context))
        .collect();
    let want: Vec<Value> = golden::collapse_deltas(golden::golden_events(transcript))
        .into_iter()
        .map(|event| golden::normalize_value(golden::blank_timing(event), &golden_context))
        .collect();
    assert_eq!(
        got, want,
        "collapsed events differ from {transcript_golden}"
    );
}

/// A narration-only reply is answered with the exact continue prompt, the
/// second text-only reply is final, and the wire carries both requests
/// exactly as the pinned Go binary sent them.
#[test]
fn differential_agent_nudge_matches_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let cwd = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let text = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(run_with(
            &[EMPTY_STOP.to_string(), TEXT_STOP.to_string()],
            "tell me something",
            cwd.path(),
            |opts| opts.max_nudges = 2,
            &events,
        ))
        .expect("nudge run succeeds");
    assert_eq!(text, "Let me check.");
    assert_events_match_golden(
        &events.lock().unwrap(),
        "mcp/chat-nudge.jsonl",
        &[cwd.path()],
    );

    // The continue prompt reaches the wire byte-for-byte: replay the loop and
    // compare both request bodies to the Go goldens.
    let mock = MockHttp::start();
    mock.route_all(&[EMPTY_STOP.to_string(), TEXT_STOP.to_string()]);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        set_mock_key();
        let provider = Arc::new(
            subagent_mcp::provider::ChatCompletions::new(
                "mock",
                &provider_config(&mock.url(), "mock-model"),
            )
            .unwrap(),
        );
        let mut opts = SessionOptions {
            provider,
            thread: None,
            model: "mock-model".to_string(),
            reasoning_effort: "high".to_string(),
            effort_sent: None,
            cwd: cwd.path().to_path_buf(),
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::Never,
            system_prompt: String::new(),
            max_turns: 0,
            max_nudges: 2,
            writable_roots: vec![],
            max_history_bytes: 0,
            max_total_tokens: 0,
            run_deadline: None,
        };
        opts.max_nudges = 2;
        let manager = SessionManager::new();
        let session = manager.create(opts).await.unwrap();
        let id = session.id.clone();
        let mut lease = manager.acquire(&id).await.unwrap();
        let hooks = RunHooks {
            emitter: Arc::new(CollectingEmitter(Arc::new(Mutex::new(Vec::new())))),
            approver: always_approver(),
        };
        let runner = Runner {
            executor: Arc::new(BuiltinToolExecutor::default()),
        };
        runner
            .run_locked(run_ctx(), &mut lease, "tell me something", &hooks)
            .await
            .unwrap();
    });
    let requests = mock.recorded();
    assert_eq!(requests.len(), 2);
    let context = NormalizeContext::new().with_temp_root(cwd.path());
    for (golden_rel, request) in [
        "wire/chat-nudge/01-chat_completions.json",
        "wire/chat-nudge/02-chat_completions.json",
    ]
    .iter()
    .zip(&requests)
    {
        let want = golden::read_json(golden_rel);
        let mut golden_roots = Vec::new();
        golden::golden_temp_roots(&want, &mut golden_roots);
        let mut golden_context = NormalizeContext::new();
        for root in golden_roots {
            golden_context = golden_context.with_temp_root(root);
        }
        assert_eq!(
            golden::normalize_wire(&request.to_golden(), &context),
            golden::normalize_wire(&want, &golden_context),
            "wire request differs from {golden_rel}"
        );
    }
    drop(context);
}

/// A spent token budget refuses the next run of the same thread by name
/// before any provider request.
#[test]
fn differential_agent_budget_error_matches_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let cwd = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_second = Arc::clone(&events);

    // Drive both runs on ONE session so the budget carries over, mirroring
    // the captured subagent + subagent-reply sequence.
    let (first, second) = tokio::runtime::Runtime::new().unwrap().block_on(async {
        set_mock_key();
        let mock = MockHttp::start();
        mock.route_all(&[TEXT_STOP.to_string()]);
        let provider = Arc::new(
            subagent_mcp::provider::ChatCompletions::new(
                "mock",
                &provider_config(&mock.url(), "mock-model"),
            )
            .unwrap(),
        );
        let opts = SessionOptions {
            provider,
            thread: None,
            model: "mock-model".to_string(),
            reasoning_effort: "high".to_string(),
            effort_sent: None,
            cwd: cwd.path().to_path_buf(),
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::Never,
            system_prompt: String::new(),
            max_turns: 0,
            max_nudges: 0,
            writable_roots: vec![],
            max_history_bytes: 0,
            max_total_tokens: 5,
            run_deadline: None,
        };
        let manager = SessionManager::new();
        let session = manager.create(opts).await.unwrap();
        let id = session.id.clone();
        let mut lease = manager.acquire(&id).await.unwrap();
        let runner = Runner {
            executor: Arc::new(BuiltinToolExecutor::default()),
        };
        let hooks = RunHooks {
            emitter: Arc::new(CollectingEmitter(Arc::clone(&events_for_second))),
            approver: always_approver(),
        };
        let first = runner.run_locked(run_ctx(), &mut lease, "hi", &hooks).await;
        // The second run's events only.
        events_for_second.lock().unwrap().clear();
        let second = runner
            .run_locked(run_ctx(), &mut lease, "again", &hooks)
            .await;
        (first, second)
    });

    assert_eq!(first.expect("first run succeeds"), "Let me check.");
    let error = second.expect_err("budget refuses the second run");
    // The exact subagent-mcp-authored budget error text.
    assert_eq!(
        error.to_string(),
        "thread spent 12 tokens (max_total_tokens 5)"
    );
    assert_events_match_golden_after_first_result(
        &events.lock().unwrap(),
        "mcp/chat-budget.jsonl",
        &[cwd.path()],
    );
}

/// Cancelling mid-turn surfaces the same error event and provider_request
/// error the pinned binary emitted.
#[test]
fn differential_agent_cancellation_matches_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let cwd = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));

    let result = tokio::runtime::Runtime::new().unwrap().block_on(async {
        set_mock_key();
        let mock = MockHttp::start();
        mock.route_all(&[TEXT_STOP.to_string()]);
        mock.set_delay_ms(2000);
        let provider = Arc::new(
            subagent_mcp::provider::ChatCompletions::new(
                "mock",
                &provider_config(&mock.url(), "mock-model"),
            )
            .unwrap(),
        );
        let opts = SessionOptions {
            provider,
            thread: None,
            model: "mock-model".to_string(),
            reasoning_effort: "high".to_string(),
            effort_sent: None,
            cwd: cwd.path().to_path_buf(),
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::Never,
            system_prompt: String::new(),
            max_turns: 0,
            max_nudges: 0,
            writable_roots: vec![],
            max_history_bytes: 0,
            max_total_tokens: 0,
            run_deadline: None,
        };
        let manager = SessionManager::new();
        let session = manager.create(opts).await.unwrap();
        let id = session.id.clone();
        let mut lease = manager.acquire(&id).await.unwrap();
        let cancel = CancellationToken::new();
        let hooks = RunHooks {
            emitter: Arc::new(CollectingEmitter(Arc::clone(&events))),
            approver: always_approver(),
        };
        let watcher = {
            let events = Arc::clone(&events);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    let has_event = events
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|event| event["type"] == "task_started");
                    if has_event {
                        cancel.cancel();
                        break;
                    }
                }
            })
        };
        let runner = Runner {
            executor: Arc::new(BuiltinToolExecutor::default()),
        };
        let result = runner
            .run_locked(
                RunContext {
                    cancel,
                    deadline: None,
                },
                &mut lease,
                "hi",
                &hooks,
            )
            .await;
        watcher.await.unwrap();
        result
    });

    let error = result.expect_err("cancelled run errors");
    assert_eq!(error.to_string(), "context canceled");
    assert_events_match_golden(
        &events.lock().unwrap(),
        "mcp/chat-cancel.jsonl",
        &[cwd.path()],
    );
}

// ---------------------------------------------------------------------------
// Full MCP transcript replays against the checked-in Go goldens.
// ---------------------------------------------------------------------------

/// The recorded request inputs of one captured scenario, sent to both
/// binaries. `follow_up` builds the (dynamic) reply request from a previous
/// structured result, for the budget scenario's threadId threading.
struct Replay {
    name: &'static str,
    config: String,
    mock_bodies: Vec<String>,
    arguments: Value,
    /// Extra mock routes (prefix, bodies), e.g. the Jev endpoint.
    routes: Vec<(&'static str, Vec<String>)>,
    follow_up: Option<fn(&Value) -> Value>,
    /// Hold replies so a mid-turn `notifications/cancelled` can race.
    cancel_after_first_event: bool,
    delay_ms: u64,
    /// `_meta.progressToken` sent with the `tools/call`, so the replay stream
    /// carries `notifications/progress` frames (task 27 review finding 1).
    progress_token: Option<&'static str>,
    /// Golden rollout JSONL the server writes for this scenario under
    /// `CODEX_HOME` (`None` for Codex, which owns its own rollout). Compared
    /// after the replay, including the server-written `session_meta` line
    /// (task 27 review findings 2 and 3).
    rollout_golden: Option<&'static str>,
    /// Extra environment entries for the replayed process, e.g. the fake
    /// Codex peer's per-run record path.
    extra_env: Vec<(String, String)>,
    /// Files seeded into the run's cwd before the call.
    seed_files: Vec<(&'static str, &'static str)>,
}

fn replay_config(provider_block: &str) -> String {
    format!(
        r#"
{provider_block}
"#
    )
}

fn chat_provider_block() -> String {
    "[providers.mock]\napi = \"chat-completions\"\nbase_url = \"MOCK_URL\"\nenv_key = \"SUBAGENT_DIFFERENTIAL_MOCK_KEY\"\ndefault_model = \"mock-model\"\nmodels = [{ id = \"mock-model\" }]\n".to_string()
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "differential", "version": "1"}
        }
    })
}

fn subagent_call(id: i64, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": "subagent", "arguments": arguments}
    })
}

/// Replay one scenario end-to-end on `binary` and return its ordered output
/// stream plus the temp home/cwd the run used (the rollout file under the
/// home is compared by the caller before they are dropped).
fn replay_stream(
    binary: &Path,
    replay: &Replay,
) -> (Vec<Value>, tempfile::TempDir, tempfile::TempDir) {
    let mock = Arc::new(MockHttp::start());
    mock.route_all(&replay.mock_bodies);
    for (prefix, bodies) in &replay.routes {
        mock.route(prefix, bodies);
    }
    if replay.delay_ms > 0 {
        mock.set_delay_ms(replay.delay_ms);
    }

    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    for (rel, content) in &replay.seed_files {
        let path = cwd.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let config_path = home.path().join("config.toml");
    std::fs::write(&config_path, replay.config.replace("MOCK_URL", &mock.url())).unwrap();

    let mut env: Vec<(&str, &str)> = vec![
        ("HOME", home.path().to_str().unwrap()),
        ("CODEX_HOME", home.path().to_str().unwrap()),
        // Rollout stays enabled so the server writes the rollout file the
        // golden rollout JSONL was captured from.
        ("SUBAGENT_MCP_ROLLOUT", ""),
        ("SUBAGENT_MCP_CONFIG", config_path.to_str().unwrap()),
        ("SUBAGENT_DIFFERENTIAL_MOCK_KEY", KEY_VALUE),
        ("SUBAGENT_DIFFERENTIAL_AUTO_KEY", "auto-key"),
    ];
    env.extend(
        replay
            .extra_env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    );

    let mut process = McpProcess::start(binary, &[], &env)
        .unwrap_or_else(|error| panic!("start {}: {error}", binary.display()))
        .with_timeout(std::time::Duration::from_secs(60));

    let initialized = process.request(initialize_request());
    assert_eq!(initialized["id"], 1);
    let mut stream: Vec<Value> = vec![initialized];
    process
        .notify(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .unwrap();

    let mut call = subagent_call(
        2,
        json!({ "cwd": cwd.path() }).merge(replay.arguments.clone()),
    );
    if let Some(token) = replay.progress_token {
        call["params"]["_meta"] = json!({ "progressToken": token });
    }
    if replay.cancel_after_first_event {
        let cancelled = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 2}
        });
        let response = process.request_with_hook(call, |frame| {
            (frame.get("method")? == "subagent/event"
                && frame["params"]["msg"]["type"] == "task_started")
                .then(|| cancelled.clone())
        });
        stream.extend(process.take_notifications());
        stream.push(response);
    } else {
        let response = process.request(call);
        stream.extend(process.take_notifications());
        let follow_up_request = replay.follow_up.map(|follow_up| follow_up(&response));
        stream.push(response);
        if let Some(second) = follow_up_request {
            let response = process.request(second);
            stream.extend(process.take_notifications());
            stream.push(response);
        }
    }
    process.close();
    (stream, home, cwd)
}

/// The `threadId` the run's `subagent/event` notifications carry.
fn stream_thread_id(stream: &[Value]) -> String {
    stream
        .iter()
        .find_map(|frame| frame["params"]["threadId"].as_str().map(str::to_string))
        .expect("a subagent/event notification carries the threadId")
}

/// Compare the ordered output stream to the checked-in golden transcript,
/// normalized (notifications drained before each response, so the comparison
/// stays causal per call).
fn assert_transcript_matches_golden(stream: &[Value], name: &str, home: &Path, cwd: &Path) {
    let golden = read_jsonl(&format!("mcp/{name}.jsonl"));
    let golden_context = golden_context_for(&golden);
    let live_context = NormalizeContext::new()
        .with_temp_root(home)
        .with_temp_root(cwd);
    assert_eq!(
        normalize_frames(stream.to_vec(), &live_context),
        normalize_frames(golden, &golden_context),
        "replayed MCP transcript differs from golden for {name}"
    );
}

/// Replay one scenario end-to-end on the Rust binary and compare its ordered
/// output stream to the checked-in golden transcript, normalized. When the
/// pinned reference is available it is replayed too and cross-checked against
/// the same golden, guarding fixture drift. Returns the Rust stream so a
/// scenario can assert on frame families (e.g. progress) directly.
fn assert_replay_matches_golden(replay: &Replay) -> Vec<Value> {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_mock_key();

    let (stream, home, cwd) = replay_stream(Path::new(rust_binary()), replay);
    assert_transcript_matches_golden(&stream, replay.name, home.path(), cwd.path());
    if let Some(golden_rel) = replay.rollout_golden {
        // The server wrote this rollout file, `session_meta` line included.
        golden::assert_rollout_for_thread_matches(
            home.path(),
            cwd.path(),
            &stream_thread_id(&stream),
            golden_rel,
        );
    }

    // Guard fixture drift: when the pinned reference is available, replay it
    // against the same golden too.
    if let Some(reference) = reference_or_skip() {
        let (go_stream, go_home, go_cwd) = replay_stream(&reference.path(), replay);
        let golden = read_jsonl(&format!("mcp/{}.jsonl", replay.name));
        let golden_context = golden_context_for(&golden);
        let go_context = NormalizeContext::new()
            .with_temp_root(go_home.path())
            .with_temp_root(go_cwd.path());
        assert_eq!(
            normalize_frames(go_stream.clone(), &go_context),
            normalize_frames(golden, &golden_context),
            "reference transcript drifted from golden for {}",
            replay.name
        );
        if let Some(golden_rel) = replay.rollout_golden {
            golden::assert_rollout_for_thread_matches(
                go_home.path(),
                go_cwd.path(),
                &stream_thread_id(&go_stream),
                golden_rel,
            );
        }
    }
    stream
}

fn budget_follow_up(first: &Value) -> Value {
    let thread_id = first["result"]["structuredContent"]["threadId"]
        .as_str()
        .expect("first reply carries a threadId")
        .to_string();
    json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "subagent-reply", "arguments": {
            "threadId": thread_id, "prompt": "again"
        }}
    })
}

fn chat_replay_arguments(prompt: &str, extra: Value) -> Value {
    let mut arguments = json!({
        "prompt": prompt,
        "provider": "mock",
        "sandbox": "read-only",
        "approval-policy": "never",
    });
    if let (Some(base), Some(extra)) = (arguments.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    arguments
}

#[test]
fn differential_mcp_replay_chat_turn_matches_golden() {
    assert_replay_matches_golden(&Replay {
        name: "chat-turn",
        config: replay_config(&chat_provider_block()),
        mock_bodies: vec![TEXT_STOP.to_string()],
        arguments: chat_replay_arguments("hi", json!({})),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        rollout_golden: Some("rollout/chat-turn.jsonl"),
        extra_env: vec![],
        seed_files: vec![],
    });
}

#[test]
fn differential_mcp_replay_chat_tool_turn_matches_golden() {
    assert_replay_matches_golden(&Replay {
        name: "chat-tool-turn",
        config: replay_config(&chat_provider_block()),
        mock_bodies: vec![TOOL_CALLS.to_string(), TEXT_STOP.to_string()],
        arguments: chat_replay_arguments("list the files", json!({"sandbox": "workspace-write"})),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        rollout_golden: Some("rollout/chat-tool-turn.jsonl"),
        extra_env: vec![],
        seed_files: vec![("a.txt", "alpha\n"), ("src/b.txt", "beta\n")],
    });
}

#[test]
fn differential_mcp_replay_chat_patch_turn_matches_golden() {
    assert_replay_matches_golden(&Replay {
        name: "chat-patch-turn",
        config: replay_config(&chat_provider_block()),
        mock_bodies: vec![PATCH_CALL.to_string(), TEXT_STOP.to_string()],
        arguments: chat_replay_arguments("add hello.txt", json!({"sandbox": "workspace-write"})),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        rollout_golden: Some("rollout/chat-patch-turn.jsonl"),
        extra_env: vec![],
        seed_files: vec![("a.txt", "alpha\n"), ("src/b.txt", "beta\n")],
    });
}

#[test]
fn differential_mcp_replay_chat_files_turn_matches_golden() {
    assert_replay_matches_golden(&Replay {
        name: "chat-files-turn",
        config: replay_config(&chat_provider_block()),
        mock_bodies: vec![FILES_CALLS.to_string(), TEXT_STOP.to_string()],
        arguments: chat_replay_arguments(
            "inspect and write",
            json!({"sandbox": "workspace-write"}),
        ),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        rollout_golden: Some("rollout/chat-files-turn.jsonl"),
        extra_env: vec![],
        seed_files: vec![("a.txt", "alpha\n"), ("src/b.txt", "beta\n")],
    });
}

#[test]
fn differential_mcp_replay_chat_nudge_matches_golden() {
    let mut config = chat_provider_block();
    config.push_str("max_nudges = 2\n");
    assert_replay_matches_golden(&Replay {
        name: "chat-nudge",
        config: replay_config(&config),
        mock_bodies: vec![EMPTY_STOP.to_string(), TEXT_STOP.to_string()],
        arguments: chat_replay_arguments("tell me something", json!({})),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        rollout_golden: Some("rollout/chat-nudge.jsonl"),
        extra_env: vec![],
        seed_files: vec![],
    });
}

#[test]
fn differential_mcp_replay_chat_budget_matches_golden() {
    assert_replay_matches_golden(&Replay {
        name: "chat-budget",
        config: replay_config(&chat_provider_block()),
        mock_bodies: vec![TEXT_STOP.to_string()],
        arguments: chat_replay_arguments("hi", json!({"config": {"max_total_tokens": 5}})),
        routes: vec![],
        follow_up: Some(budget_follow_up),
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        rollout_golden: Some("rollout/chat-budget.jsonl"),
        extra_env: vec![],
        seed_files: vec![],
    });
}

#[test]
fn differential_mcp_replay_chat_cancel_matches_golden() {
    assert_replay_matches_golden(&Replay {
        name: "chat-cancel",
        config: replay_config(&chat_provider_block()),
        mock_bodies: vec![TEXT_STOP.to_string()],
        arguments: chat_replay_arguments("hi", json!({})),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: true,
        delay_ms: 3000,
        progress_token: None,
        rollout_golden: Some("rollout/chat-cancel.jsonl"),
        extra_env: vec![],
        seed_files: vec![],
    });
}

#[test]
fn differential_mcp_replay_responses_turn_matches_golden() {
    let mut config = chat_provider_block();
    config = config.replace("api = \"chat-completions\"", "api = \"responses\"");
    assert_replay_matches_golden(&Replay {
        name: "responses-turn",
        config: replay_config(&config),
        mock_bodies: vec![RESPONSES_TEXT.to_string()],
        arguments: chat_replay_arguments("hi", json!({})),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        rollout_golden: Some("rollout/responses-turn.jsonl"),
        extra_env: vec![],
        seed_files: vec![],
    });
}

#[test]
fn differential_mcp_replay_messages_turn_matches_golden() {
    let mut config = chat_provider_block();
    config = config.replace("api = \"chat-completions\"", "api = \"messages\"");
    assert_replay_matches_golden(&Replay {
        name: "messages-turn",
        config: replay_config(&config),
        mock_bodies: vec![MESSAGES_TEXT.to_string()],
        arguments: chat_replay_arguments("hi", json!({})),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        rollout_golden: Some("rollout/messages-turn.jsonl"),
        extra_env: vec![],
        seed_files: vec![],
    });
}

#[test]
fn differential_mcp_replay_auto_turn_matches_golden() {
    let jev = r#"{"model":"jev-1.14.0","answers":{"model":{"type":"choice","choice":"c2","confidence":0.9,"probabilities":{"c1":0.1,"c2":0.9}}},"usage":{"input_tokens":42,"output_tokens":3}}"#;
    let config = r#"
[auto]
base_url = "MOCK_URL"
env_key = "SUBAGENT_DIFFERENTIAL_AUTO_KEY"
model = "jev-latest"
timeout_seconds = 5

[providers.alpha]
api = "chat-completions"
base_url = "MOCK_URL"
env_key = "SUBAGENT_DIFFERENTIAL_MOCK_KEY"
default_model = "m-fast"
models = [{ id = "m-fast" }]

[providers.bravo]
api = "chat-completions"
base_url = "MOCK_URL"
env_key = "SUBAGENT_DIFFERENTIAL_MOCK_KEY"
default_model = "m-pro"
models = [{ id = "m-pro" }]
"#
    .to_string();
    assert_replay_matches_golden(&Replay {
        name: "auto-turn",
        config: replay_config(&config),
        mock_bodies: vec![TEXT_STOP.to_string()],
        arguments: {
            let mut arguments = chat_replay_arguments("fix the parser", json!({}));
            let object = arguments.as_object_mut().unwrap();
            object.remove("provider");
            object.insert("model".into(), json!("auto"));
            arguments
        },
        routes: vec![("/v1/systemone", vec![jev.to_string()])],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        rollout_golden: Some("rollout/auto-turn.jsonl"),
        extra_env: vec![],
        seed_files: vec![],
    });
}

/// A replay that sends `_meta.progressToken`: the stream must carry
/// `notifications/progress` frames (token, per-call sequence, mapped message)
/// interleaved causally with the `subagent/event` frames, exactly as the
/// pinned Go binary emitted them for the same requests.
#[test]
fn differential_mcp_replay_progress_turn_matches_golden() {
    let stream = assert_replay_matches_golden(&Replay {
        name: "chat-progress",
        config: replay_config(&chat_provider_block()),
        mock_bodies: vec![TEXT_STOP.to_string()],
        arguments: chat_replay_arguments("hi", json!({})),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: Some("prog-1"),
        rollout_golden: Some("rollout/chat-turn.jsonl"),
        extra_env: vec![],
        seed_files: vec![],
    });

    // The progress frames explicitly: one per summarized event, the client's
    // token, a per-call sequence starting at 1, and Go's mapped messages.
    let progress: Vec<&Value> = stream
        .iter()
        .filter(|frame| frame["method"] == "notifications/progress")
        .collect();
    assert_eq!(progress.len(), 3, "progress frames: {progress:?}");
    for (index, frame) in progress.iter().enumerate() {
        assert_eq!(frame["params"]["progressToken"], "prog-1");
        assert_eq!(frame["params"]["progress"], json!(index + 1));
    }
    let messages: Vec<&str> = progress
        .iter()
        .map(|frame| frame["params"]["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        messages,
        vec!["started", "agent: Let me check.", "completed"]
    );
    // Causal per call: each progress frame directly follows the
    // `subagent/event` it summarizes (verified against the Go golden's
    // interleaving by the transcript comparison above).
    for frame in &progress {
        let position = stream
            .iter()
            .position(|candidate| candidate == *frame)
            .unwrap();
        assert_eq!(
            stream[position - 1]["method"],
            "subagent/event",
            "progress frame not preceded by its event: {frame}"
        );
    }
}

/// The fake Codex peer's per-run record env entry; the serve process scrubs
/// `SUBAGENT_MCP_*` and config env keys from its children, and
/// `FAKE_CODEX_RECORD` is neither, so it reaches the peer script.
fn codex_replay() -> (Replay, tempfile::TempDir) {
    let record_dir = tempfile::tempdir().unwrap();
    let record_path = record_dir.path().join("record.jsonl");
    let script = golden::fixture("codex/app-server-turn.sh");
    let config = format!(
        "[providers.codex]\napi = \"codex-app-server\"\ncommand = \"{}\"\ndefault_model = \"gpt-6-astra\"\nmodels = [{{ id = \"gpt-6-astra\" }}]\n",
        script.display()
    );
    let replay = Replay {
        name: "codex-turn",
        config: replay_config(&config),
        mock_bodies: vec![],
        arguments: json!({
            "prompt": "hi",
            "provider": "codex",
            "sandbox": "read-only",
            "approval-policy": "never",
        }),
        routes: vec![],
        follow_up: None,
        cancel_after_first_event: false,
        delay_ms: 0,
        progress_token: None,
        // Codex owns its rollout; the server writes no rollout file for it.
        rollout_golden: None,
        extra_env: vec![(
            "FAKE_CODEX_RECORD".to_string(),
            record_path.to_string_lossy().into_owned(),
        )],
        seed_files: vec![],
    };
    (replay, record_dir)
}

/// The full `mcp/codex-turn.jsonl` golden, at the MCP level: the serve process
/// runs the checked-in scripted fake app-server and the output frames the
/// golden captured — the initialize response and the tools/call result —
/// match the pinned Go capture exactly, and the client-to-peer record matches
/// the Go record golden too.
///
/// Known divergence (verified live against the pinned binary while wiring
/// this golden): Go 0.13.1 emits no `subagent/event` notifications for Codex
/// sessions, while the Rust port forwards the runner events. The comparison
/// below pins the frames the golden contains; the extra Rust notifications
/// are a documented parity divergence, not an asserted behavior.
#[test]
fn differential_mcp_replay_codex_turn_matches_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_mock_key();
    // The Go drift-guard replay below may write to the fixed fallback record
    // path (Go's pool dialer scrubs the environment); keep it clean.
    let fallback = Path::new("/tmp/subagent-mcp-codex-app-server-record.jsonl");
    let _ = std::fs::remove_file(fallback);

    let (replay, record_dir) = codex_replay();
    let (stream, home, cwd) = replay_stream(Path::new(rust_binary()), &replay);
    assert_codex_transcript_frames(&stream, home.path(), cwd.path());

    // The client-to-peer record: same lines the Go client sent at capture
    // time, compared with the same temp-root normalization as the
    // adapter-level codex scenario.
    let record_path = record_dir.path().join("record.jsonl");
    let got = golden::read_rollout(&record_path);
    let want = read_jsonl("codex/turn-record.jsonl");
    assert_eq!(got.len(), want.len(), "record line count");
    let rust_context = NormalizeContext::new()
        .with_temp_root(home.path())
        .with_temp_root(cwd.path());
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

    // Drift guard: the pinned binary must still produce the same frames.
    if let Some(reference) = reference_or_skip() {
        let (go_stream, go_home, go_cwd) = replay_stream(&reference.path(), &replay);
        assert_codex_transcript_frames(&go_stream, go_home.path(), go_cwd.path());
    }
    let _ = std::fs::remove_file(fallback);
}

/// Compare the two frames the codex golden captured (initialize response and
/// tools/call result) positionally.
fn assert_codex_transcript_frames(stream: &[Value], home: &Path, cwd: &Path) {
    let golden = read_jsonl("mcp/codex-turn.jsonl");
    assert_eq!(golden.len(), 2, "golden frame count");
    let live_context = NormalizeContext::new()
        .with_temp_root(home)
        .with_temp_root(cwd);
    let golden_context = golden_context_for(&golden);
    let initialize = stream
        .iter()
        .find(|frame| frame["id"] == json!(1))
        .expect("initialize response");
    let result = stream
        .iter()
        .find(|frame| frame["id"] == json!(2) && frame.get("result").is_some())
        .expect("tools/call result");
    let golden_initialize = golden
        .iter()
        .find(|frame| frame["id"] == json!(1))
        .expect("golden initialize response");
    let golden_result = golden
        .iter()
        .find(|frame| frame["id"] == json!(2) && frame.get("result").is_some())
        .expect("golden tools/call result");
    assert_eq!(
        golden::normalize_value(golden::blank_timing(initialize.clone()), &live_context),
        golden::normalize_value(
            golden::blank_timing(golden_initialize.clone()),
            &golden_context
        ),
        "codex initialize response differs from golden"
    );
    assert_eq!(
        golden::normalize_value(golden::blank_timing(result.clone()), &live_context),
        golden::normalize_value(golden::blank_timing(golden_result.clone()), &golden_context),
        "codex tools/call result differs from golden"
    );
}

/// A JSON merge helper: `base` wins for overlapping keys only when `overlay`
/// is missing them. Named `merge` for readability at the call site.
trait MergeInto {
    fn merge(self, overlay: Value) -> Value;
}

impl MergeInto for Value {
    fn merge(self, overlay: Value) -> Value {
        let mut base = self;
        if let (Some(base), Some(overlay)) = (base.as_object_mut(), overlay.as_object()) {
            for (key, value) in overlay {
                base.insert(key.clone(), value.clone());
            }
        }
        base
    }
}
