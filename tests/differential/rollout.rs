//! Rollout differential scenarios (Task 27): the Rust runner's rollout
//! recorder output compared, per turn, to the rollout JSONL the pinned Go
//! 0.13.1 binary wrote for the same scenarios.
//!
//! `session_meta` is written by the server in Go, not by the recorder, so the
//! test constructs that one line at the call site (exactly as Go's
//! `internal/server/server.go` does; the capture cwds are not git repos, so
//! no `git` object is present). All other lines come from the runner with the
//! recorder attached, as in production. The server-written `session_meta` is
//! additionally compared end-to-end by the MCP replays in
//! `tests/differential/agent.rs`, which read the rollout file the Rust server
//! actually wrote, so this fabricated line is belt-and-braces for the runner
//! level only.
//!
//! Volatile fields (timestamps, `current_date`, `started_at`/`completed_at`
//! epoch seconds, `cli_version`, durations, and cwd paths) are normalized to
//! placeholders before the parsed lines are compared.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use subagent_mcp::VERSION;
use subagent_mcp::agent::{
    BuiltinToolExecutor, DEFAULT_SYSTEM_PROMPT, Emitter, RunHooks, Runner, SessionManager,
    SessionOptions,
};
use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::rollout::RolloutRecorder;
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;

use crate::support::EnvGuard;
use crate::support::golden;

const TEXT_STOP: &str = include_str!("../fixtures/sse/chat/text_stop_usage.sse");
const EMPTY_STOP: &str = include_str!("../fixtures/sse/chat/empty_stop_usage.sse");
const TOOL_CALLS: &str = include_str!("../fixtures/sse/chat/tool_calls_indexed.sse");

const ENV_KEY: &str = "SUBAGENT_DIFFERENTIAL_MOCK_KEY";
const KEY_VALUE: &str = "mock-key";

static SCENARIO_LOCK: Mutex<()> = Mutex::new(());

fn set_mock_key() {
    // SAFETY: under SCENARIO_LOCK; constant value.
    unsafe { std::env::set_var(ENV_KEY, KEY_VALUE) };
}

fn run_ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
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

fn provider_config(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        api: Api::ChatCompletions,
        base_url: base_url.to_string(),
        env_key: Some(ENV_KEY.to_string()),
        key: None,
        command: None,
        default_model: "mock-model".to_string(),
        models: vec![subagent_mcp::config::ModelConfig {
            id: "mock-model".to_string(),
            description: String::new(),
        }],
        effort_map: Default::default(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    }
}

/// Run one full loop with a rollout recorder attached, in a fresh home and
/// cwd (both temp dirs). Returns them with the thread id and the parsed
/// rollout lines (recorder lines only; `session_meta` is added by the
/// caller, as the Go server is the one that writes it).
fn run_with_rollout(
    responses: &[String],
    prompt: &str,
    max_nudges: usize,
    sandbox: Sandbox,
    seed_files: &[(&str, &str)],
) -> (tempfile::TempDir, tempfile::TempDir, String, Vec<Value>) {
    set_mock_key();
    let mock = crate::support::mock_http::MockHttp::start();
    mock.route_all(responses);
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    for (rel, content) in seed_files {
        let path = cwd.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    // The recorder resolves the rollout root from CODEX_HOME, exactly like
    // Go's rollout.Open. SAFETY: env mutation is serialized under
    // SCENARIO_LOCK for the whole test, and the guard restores the previous
    // value so no ambient CODEX_HOME leaks to other scenarios (review
    // finding 5).
    let _codex_home = EnvGuard::set("CODEX_HOME", home.path());

    let thread_id = uuid::Uuid::new_v4().to_string();
    let recorder = RolloutRecorder::create(
        &thread_id,
        cwd.path(),
        DEFAULT_SYSTEM_PROMPT,
        "mock",
        "mock-model",
        "high",
    )
    .expect("rollout file creates");

    let provider = Arc::new(
        subagent_mcp::provider::ChatCompletions::new("mock", &provider_config(&mock.url()))
            .expect("adapter builds"),
    );
    let opts = SessionOptions {
        provider,
        thread: None,
        model: "mock-model".to_string(),
        reasoning_effort: "high".to_string(),
        effort_sent: None,
        cwd: cwd.path().to_path_buf(),
        sandbox,
        approval: ApprovalPolicy::Never,
        system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
        max_turns: 0,
        max_nudges,
        writable_roots: vec![],
        max_history_bytes: 0,
        max_total_tokens: 0,
        run_deadline: None,
    };
    let events = Arc::new(Mutex::new(Vec::new()));
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let manager = SessionManager::new();
        let session = manager.create(opts).await.unwrap();
        let id = session.id.clone();
        let mut lease = manager.acquire(&id).await.unwrap();
        lease.rollout = Some(recorder);
        let hooks = RunHooks {
            emitter: Arc::new(CollectingEmitter(events)),
            approver: always_approver(),
        };
        let runner = Runner {
            executor: Arc::new(BuiltinToolExecutor::new(std::path::PathBuf::from(env!(
                "CARGO_BIN_EXE_subagent-mcp"
            )))),
        };
        runner
            .run_locked(run_ctx(), &mut lease, prompt, &hooks)
            .await
            .expect("run succeeds");
    });

    let path = golden::find_rollout(home.path(), &thread_id);
    (home, cwd, thread_id, golden::read_rollout(&path))
}

/// Build `session_meta` exactly as Go's server.go does for a non-auto
/// selection in a non-repo cwd (no `git` object), and prepend it.
fn with_session_meta(lines: Vec<Value>, thread_id: &str, cwd: &Path) -> Vec<Value> {
    let meta = json!({
        "session_id": thread_id,
        "id": thread_id,
        "timestamp": "2026-11-12T00:00:00Z",
        "cwd": cwd,
        "originator": "subagent-mcp",
        "cli_version": VERSION,
        "source": "mcp",
        "model_provider": "mock",
        "base_instructions": {"text": DEFAULT_SYSTEM_PROMPT},
    });
    let mut out = vec![json!({
        // Distinct values for the line and the payload timestamp: Go writes
        // two separate now() calls, and the placeholder stream must align.
        "timestamp": "2026-11-12T00:00:00.000000001Z",
        "type": "session_meta",
        "payload": meta,
    })];
    out.extend(lines);
    out
}

#[test]
fn differential_rollout_chat_turn_matches_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, cwd, thread_id, lines) =
        run_with_rollout(&[TEXT_STOP.to_string()], "hi", 0, Sandbox::ReadOnly, &[]);
    golden::assert_rollout_matches(
        home.path(),
        cwd.path(),
        with_session_meta(lines, &thread_id, cwd.path()),
        "rollout/chat-turn.jsonl",
    );
}

#[test]
fn differential_rollout_chat_nudge_matches_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, cwd, thread_id, lines) = run_with_rollout(
        &[EMPTY_STOP.to_string(), TEXT_STOP.to_string()],
        "tell me something",
        2,
        Sandbox::ReadOnly,
        &[],
    );
    golden::assert_rollout_matches(
        home.path(),
        cwd.path(),
        with_session_meta(lines, &thread_id, cwd.path()),
        "rollout/chat-nudge.jsonl",
    );
}

#[test]
fn differential_rollout_chat_tool_turn_matches_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (home, cwd, thread_id, lines) = run_with_rollout(
        &[TOOL_CALLS.to_string(), TEXT_STOP.to_string()],
        "list the files",
        0,
        Sandbox::WorkspaceWrite,
        &[("a.txt", "alpha\n"), ("src/b.txt", "beta\n")],
    );
    golden::assert_rollout_matches(
        home.path(),
        cwd.path(),
        with_session_meta(lines, &thread_id, cwd.path()),
        "rollout/chat-tool-turn.jsonl",
    );
}
