//! Tool differential scenarios (Task 27): the Rust built-in executor against
//! the tool results the pinned Go 0.13.1 binary produced for the same calls.
//!
//! The Go tool outputs live inside the checked-in wire goldens (`tests/
//! fixtures/wire/*/02-*.json`, the second turn's request history) and the
//! filesystem effects in `expected-tree.txt`, so everything stands alone
//! after the Go tree is deleted. Compared per call: output text, and tree,
//! modes, and contents for the filesystem effects.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use subagent_mcp::agent::{
    BuiltinToolExecutor, Emitter, RunHooks, SessionManager, SessionOptions, ToolExecutor,
};
use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::provider::ToolCall;
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;

use crate::support::golden;

const ENV_KEY: &str = "SUBAGENT_DIFFERENTIAL_MOCK_KEY";
const KEY_VALUE: &str = "mock-key";

static SCENARIO_LOCK: Mutex<()> = Mutex::new(());

/// Discarding event sink for direct executor calls.
struct NullEmitter;

impl Emitter for NullEmitter {
    fn emit(&self, _thread_id: &str, _event: Value) {}
}

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

/// The executor whose Landlock helper is the real binary, like production.
fn real_executor() -> BuiltinToolExecutor {
    BuiltinToolExecutor::new(PathBuf::from(env!("CARGO_BIN_EXE_subagent-mcp")))
}

/// The `role: "tool"` messages of the second turn's request history: the
/// outputs the pinned Go binary recorded for each call, in call order.
fn golden_tool_outputs(wire_golden: &str) -> Vec<(String, String)> {
    let golden = golden::read_json(wire_golden);
    golden["body"]["messages"]
        .as_array()
        .expect("golden history has messages")
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| {
            (
                message["tool_call_id"].as_str().unwrap().to_string(),
                message["content"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}

/// Execute one tool call against a fresh workspace-write session in `cwd`.
async fn execute(cwd: &Path, tool_call: &ToolCall) -> subagent_mcp::agent::ToolResult {
    set_mock_key();
    let cfg = ProviderConfig {
        api: Api::ChatCompletions,
        base_url: "http://127.0.0.1:1".to_string(),
        env_key: Some(ENV_KEY.to_string()),
        key: None,
        command: None,
        default_model: "mock-model".to_string(),
        models: vec![],
        effort_map: Default::default(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    };
    let provider = Arc::new(
        subagent_mcp::provider::ChatCompletions::new("mock", &cfg).expect("adapter builds"),
    );
    let opts = SessionOptions {
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
    };
    let manager = SessionManager::new();
    let session = manager.create(opts).await.unwrap();
    let id = session.id.clone();
    let mut lease = manager.acquire(&id).await.unwrap();
    let hooks = RunHooks {
        emitter: Arc::new(NullEmitter),
        approver: always_approver(),
    };
    real_executor()
        .execute(&run_ctx(), &mut lease, tool_call, &hooks, "turn-diff")
        .await
}

/// Pre-order tree with modes, matching the capture driver.
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

fn seeded_cwd() -> tempfile::TempDir {
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::create_dir_all(cwd.path().join("src")).unwrap();
    std::fs::write(cwd.path().join("src/b.txt"), "beta\n").unwrap();
    cwd
}

/// shell + unknown tool, compared to the Go outputs in the tool-turn golden.
#[test]
fn differential_tools_shell_and_unknown_tool_match_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let outputs = golden_tool_outputs("wire/chat-tool-turn/02-chat_completions.json");
    assert_eq!(outputs.len(), 2, "golden has two tool results");

    let cwd = seeded_cwd();
    let shell = tokio::runtime::Runtime::new().unwrap().block_on(execute(
        cwd.path(),
        &call("call_a", "shell", json!({"command": "ls"})),
    ));
    assert_eq!(shell.output, outputs[0].1, "shell ls output");
    assert!(!shell.is_error);

    let unknown = tokio::runtime::Runtime::new().unwrap().block_on(execute(
        cwd.path(),
        &call("call_b", "get_weather", json!({"city": "Vancouver"})),
    ));
    assert_eq!(unknown.output, outputs[1].1, "unknown tool output");
    assert!(unknown.is_error, "unknown tool results are marked errors");
}

/// shell + read_file + write_file, compared to the Go outputs and to the
/// captured tree (modes included) and file contents.
#[test]
fn differential_tools_shell_read_write_match_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let outputs = golden_tool_outputs("wire/chat-files-turn/02-chat_completions.json");
    assert_eq!(outputs.len(), 3, "golden has three tool results");

    let cwd = seeded_cwd();
    let shell = tokio::runtime::Runtime::new().unwrap().block_on(execute(
        cwd.path(),
        &call("call_s", "shell", json!({"command": "ls"})),
    ));
    assert_eq!(shell.output, outputs[0].1, "shell ls output");

    let read = tokio::runtime::Runtime::new().unwrap().block_on(execute(
        cwd.path(),
        &call("call_r", "read_file", json!({"path": "a.txt"})),
    ));
    assert_eq!(read.output, outputs[1].1, "read_file output");
    assert!(!read.is_error);

    let write = tokio::runtime::Runtime::new().unwrap().block_on(execute(
        cwd.path(),
        &call(
            "call_w",
            "write_file",
            json!({"path": "out.txt", "content": "written by tool\n"}),
        ),
    ));
    assert_eq!(write.output, outputs[2].1, "write_file output");
    assert!(!write.is_error);

    // Filesystem effects: tree, modes, and contents.
    assert_eq!(
        tree_of(cwd.path()),
        golden::read_text("wire/chat-files-turn/expected-tree.txt")
    );
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("out.txt")).unwrap(),
        "written by tool\n"
    );
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
}

/// apply_patch: the checked-in patch case reproduces the captured tree,
/// modes, and file content through the real executor.
#[test]
fn differential_tools_apply_patch_matches_golden() {
    let _guard = SCENARIO_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let cwd = seeded_cwd();
    let patch = tokio::runtime::Runtime::new().unwrap().block_on(execute(
        cwd.path(),
        &call(
            "call_p",
            "apply_patch",
            json!({"patch": golden::read_text("patch/case.patch")}),
        ),
    ));
    assert!(!patch.is_error, "patch applies: {:?}", patch.output);
    assert_eq!(
        tree_of(cwd.path()),
        golden::read_text("patch/expected-tree.txt")
    );
    assert_eq!(
        std::fs::read_to_string(cwd.path().join("hello.txt")).unwrap(),
        golden::read_text("patch/hello.txt")
    );
}
