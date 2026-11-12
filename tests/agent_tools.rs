//! Integration tests for Task 23B: the built-in tool executor.
//!
//! Ports the tool half of Go `internal/agent/runner_test.go` and
//! `integration_test.go` (`TestBuiltinTools`, `TestRunnerShellToolCallThenText`,
//! `TestRunnerApprovalDenied`, `TestRunnerNeverPolicyDenial`,
//! `TestRunnerRecoversToolExecutionPanicAndCompletesHistory`,
//! `TestRunnerShellSandboxing`, `TestRunnerApplyPatch*`,
//! `TestApprovalPromptNamesTheSandboxEscalation`,
//! `TestExecToolCallRejectsInvalidModelOutput`, and the empty-output /
//! truncated-arguments cases). The in-repo Go tree is normative for exact
//! errors, events and tool schemas.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use subagent_mcp::agent::{
    AgentError, BuiltinToolExecutor, Emitter, RunHooks, Runner, SessionLease, SessionManager,
    SessionOptions, ToolExecutor, ToolResult, builtin_tools,
};
use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::provider::{
    DeltaSink, Provider, ProviderError, Role, ToolCall, ToolSpec, TurnRequest, TurnResult,
};
use subagent_mcp::rollout::RolloutRecorder;
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Shared recording sinks
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<(String, Value)>>>);

impl Events {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    fn push(&self, thread_id: &str, event: Value) {
        self.0.lock().unwrap().push((thread_id.to_string(), event));
    }
    fn all(&self) -> Vec<(String, Value)> {
        self.0.lock().unwrap().clone()
    }
    fn types(&self) -> Vec<String> {
        self.all()
            .iter()
            .map(|(_, e)| {
                e.get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }
    fn count(&self, t: &str) -> usize {
        self.types().iter().filter(|x| x.as_str() == t).count()
    }
    /// The n-th (0-based) event of a given type, or None.
    fn nth(&self, t: &str, n: usize) -> Option<Value> {
        self.all()
            .into_iter()
            .filter(|(_, e)| e.get("type").and_then(|v| v.as_str()) == Some(t))
            .nth(n)
            .map(|(_, e)| e)
    }
}

struct RecordingEmitter {
    events: Events,
}

impl Emitter for RecordingEmitter {
    fn emit(&self, thread_id: &str, event: Value) {
        self.events.push(thread_id, event);
    }
}

fn recording_emitter(events: &Events) -> Arc<dyn Emitter> {
    Arc::new(RecordingEmitter {
        events: events.clone(),
    })
}

/// Records the event (like Go's `recEmitter`) and panics the FIRST time an
/// event of `panic_type` is emitted (Go `panicOnceEmitter`).
struct PanicOnceEmitter {
    events: Events,
    panic_type: &'static str,
    fired: AtomicBool,
}

impl Emitter for PanicOnceEmitter {
    fn emit(&self, thread_id: &str, event: Value) {
        self.events.push(thread_id, event.clone());
        if event.get("type").and_then(|v| v.as_str()) == Some(self.panic_type)
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            panic!("emitter boom");
        }
    }
}

/// Cancels a run the first time `exec_command_begin` arrives, so a later tool
/// call in the same batch is skipped with `cancelled: not executed`.
struct CancelOnBeginEmitter {
    events: Events,
    cancel: CancellationToken,
    fired: AtomicBool,
}

impl Emitter for CancelOnBeginEmitter {
    fn emit(&self, thread_id: &str, event: Value) {
        self.events.push(thread_id, event.clone());
        if event.get("type").and_then(|v| v.as_str()) == Some("exec_command_begin")
            && !self.fired.swap(true, Ordering::SeqCst)
        {
            self.cancel.cancel();
        }
    }
}

// ---------------------------------------------------------------------------
// Approvers
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Approvals(Arc<Mutex<Vec<RecordedApproval>>>);

#[derive(Clone)]
struct RecordedApproval {
    tool: String,
    command: Option<String>,
    path: Option<String>,
    reason: String,
}

struct DecideApprover {
    decision: bool,
    approvals: Approvals,
}

#[async_trait]
impl Approver for DecideApprover {
    async fn approve(&self, _ctx: &RunContext, req: ApprovalRequest) -> bool {
        self.approvals.0.lock().unwrap().push(RecordedApproval {
            tool: req.tool,
            command: req.command,
            path: req.path,
            reason: req.reason,
        });
        self.decision
    }
}

fn approver(decision: bool) -> (Arc<dyn Approver>, Approvals) {
    let approvals = Approvals::default();
    (
        Arc::new(DecideApprover {
            decision,
            approvals: approvals.clone(),
        }),
        approvals,
    )
}

/// Panics AFTER an `.await` boundary: a `catch_unwind` around only future
/// creation would miss this, while one around the polling boundary catches it.
struct PanicApprover;

#[async_trait]
impl Approver for PanicApprover {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        tokio::task::yield_now().await;
        panic!("approver boom");
    }
}

fn hooks(emitter: Arc<dyn Emitter>, approver: Arc<dyn Approver>) -> RunHooks {
    RunHooks { emitter, approver }
}

// ---------------------------------------------------------------------------
// Provider fake (queued turns)
// ---------------------------------------------------------------------------

struct StubProvider {
    turns: Mutex<std::collections::VecDeque<TurnResult>>,
}

impl StubProvider {
    fn new(turns: Vec<TurnResult>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
        }
    }
}

#[async_trait]
impl Provider for StubProvider {
    fn name(&self) -> &str {
        "stub"
    }
    async fn turn(
        &self,
        _ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        Ok(self.turns.lock().unwrap().pop_front().unwrap_or_default())
    }
}

fn provider(turns: Vec<TurnResult>) -> Arc<dyn Provider> {
    Arc::new(StubProvider::new(turns))
}

fn tool_turn(calls: Vec<ToolCall>) -> TurnResult {
    TurnResult {
        tool_calls: calls,
        ..Default::default()
    }
}

fn text_turn(text: &str) -> TurnResult {
    TurnResult {
        text: text.to_string(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Session / execution helpers
// ---------------------------------------------------------------------------

fn options(
    p: Arc<dyn Provider>,
    cwd: &Path,
    sandbox: Sandbox,
    approval: ApprovalPolicy,
) -> SessionOptions {
    SessionOptions {
        provider: p,
        thread: None,
        model: String::new(),
        reasoning_effort: String::new(),
        effort_sent: None,
        cwd: cwd.to_path_buf(),
        sandbox,
        approval,
        system_prompt: String::new(),
        max_turns: 0,
        max_nudges: 0,
        writable_roots: vec![],
        max_history_bytes: 0,
        max_total_tokens: 0,
        run_deadline: None,
    }
}

async fn lease(opts: SessionOptions) -> SessionLease {
    let m = SessionManager::new();
    let s = m.create(opts).await.unwrap();
    let id = s.id.clone();
    m.acquire(&id).await.unwrap()
}

fn live_ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn tool_call(id: &str, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}

/// Call the executor directly for one tool call.
async fn exec(opts: SessionOptions, ctx: RunContext, call: ToolCall, h: &RunHooks) -> ToolResult {
    let mut l = lease(opts).await;
    BuiltinToolExecutor.execute(&ctx, &mut l, &call, h).await
}

/// Run several tool calls against the SAME session lease.
async fn exec_on(
    l: &mut SessionLease,
    ctx: RunContext,
    call: ToolCall,
    h: &RunHooks,
) -> ToolResult {
    BuiltinToolExecutor.execute(&ctx, l, &call, h).await
}

/// Drive the full runner loop with the real built-in executor.
async fn run_loop(
    opts: SessionOptions,
    ctx: RunContext,
    h: &RunHooks,
) -> (Result<String, AgentError>, SessionLease) {
    let m = SessionManager::new();
    let s = m.create(opts).await.unwrap();
    let id = s.id.clone();
    let mut l = m.acquire(&id).await.unwrap();
    let runner = Runner {
        executor: Arc::new(BuiltinToolExecutor),
    };
    let r = runner.run_locked(ctx, &mut l, "go", h).await;
    (r, l)
}

fn tool_message<'a>(l: &'a SessionLease, call_id: &str) -> &'a subagent_mcp::provider::Message {
    l.messages
        .iter()
        .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(call_id))
        .unwrap_or_else(|| panic!("no tool message for {call_id}"))
}

fn write(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

// ===========================================================================
// Step 1 tests
// ===========================================================================

/// Port of Go `TestBuiltinTools`: exact names, descriptions and JSON schemas.
#[test]
fn builtin_tools_exact_schemas() {
    let tools: Vec<ToolSpec> = builtin_tools();
    assert_eq!(tools.len(), 4, "builtin tool count");

    let descriptions: Vec<(&str, &str)> = vec![
        (
            "shell",
            "Run a bash command in the working directory. The sandbox policy may deny the call; providing justification helps if approval is required.",
        ),
        (
            "read_file",
            "Read a file, resolving relative paths against the working directory. The sandbox policy may deny the call; providing justification helps if approval is required.",
        ),
        (
            "write_file",
            "Create or overwrite a whole file, creating parent directories as needed. The sandbox policy may deny the call; providing justification helps if approval is required.",
        ),
        (
            "apply_patch",
            "Edit files with a patch: '*** Begin Patch', then '*** Add File: <path>' (+lines), '*** Delete File: <path>', or '*** Update File: <path>' (optional '*** Move to: <path>') with '@@' chunks of ' ' context, '-' removed, and '+' added lines, then '*** End Patch'. Prefer this for editing existing files. The sandbox policy may deny the call; providing justification helps if approval is required.",
        ),
    ];
    let wanted: Vec<Value> = vec![
        json!({"type":"object","properties":{
            "command":{"type":"string","description":"Bash command to run."},
            "timeout_seconds":{"type":"integer","description":"Optional timeout in seconds (clamped, max 600)."},
            "justification":{"type":"string","description":"Why this call is needed if approval is required."}},
            "required":["command"]}),
        json!({"type":"object","properties":{
            "path":{"type":"string","description":"File path to read."},
            "offset":{"type":"integer","description":"1-based line number to start reading from; defaults to 1"},
            "limit":{"type":"integer","description":"maximum number of lines to return; defaults to all that fit in the 16 KiB output cap"},
            "justification":{"type":"string","description":"Why this call is needed if approval is required."}},
            "required":["path"]}),
        json!({"type":"object","properties":{
            "path":{"type":"string","description":"File path to write."},
            "content":{"type":"string","description":"Complete file content."},
            "justification":{"type":"string","description":"Why this call is needed if approval is required."}},
            "required":["path","content"]}),
        json!({"type":"object","properties":{
            "patch":{"type":"string","description":"Complete patch text from *** Begin Patch to *** End Patch."},
            "justification":{"type":"string","description":"Why this call is needed if approval is required."}},
            "required":["patch"]}),
    ];

    for (i, (name, desc)) in descriptions.iter().enumerate() {
        let t = &tools[i];
        assert_eq!(&t.name, name, "tool {i} name");
        assert_eq!(&t.description, desc, "{name} description");
        assert!(
            t.description.to_lowercase().contains("sandbox policy")
                && t.description.to_lowercase().contains("justification"),
            "{name} description must explain sandbox/justification"
        );
        let got: Value = serde_json::from_str(t.parameters.get()).unwrap();
        assert_eq!(got, wanted[i], "{name} schema mismatch");
    }
}

/// Port of `TestRunnerShellToolCallThenText` (tool half): exact shell result
/// and the begin/end exec events.
#[tokio::test]
async fn shell_success_and_exec_events() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let call = tool_call("call-shell", "shell", json!({"command": "echo hi"}));
    let r = exec(
        options(
            provider(vec![]),
            cwd.path(),
            Sandbox::DangerFullAccess,
            ApprovalPolicy::Never,
        ),
        live_ctx(),
        call,
        &h,
    )
    .await;

    assert!(!r.is_error, "err: {:?}", r.output);
    assert!(r.output.starts_with("exit code: 0\n"), "{:?}", r.output);
    assert!(r.output.contains("hi"), "{:?}", r.output);

    assert_eq!(
        events.types(),
        vec!["exec_command_begin", "exec_command_end"],
        "event types"
    );
    let begin = events.nth("exec_command_begin", 0).unwrap();
    assert_eq!(begin["call_id"], "call-shell");
    assert_eq!(begin["tool"], "shell");
    assert_eq!(begin["command"], "echo hi");
    let end = events.nth("exec_command_end", 0).unwrap();
    assert_eq!(end["call_id"], "call-shell");
    assert_eq!(end["tool"], "shell");
    assert_eq!(end["exit_code"], 0);
    assert!(
        end.get("error").is_none(),
        "successful shell end has no error"
    );
}

/// Port of `TestRunnerShellNonZeroExitIsNotToolError`.
#[tokio::test]
async fn shell_nonzero_exit_is_not_tool_error() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let call = tool_call("c1", "shell", json!({"command": "exit 3"}));
    let r = exec(
        options(
            provider(vec![]),
            cwd.path(),
            Sandbox::DangerFullAccess,
            ApprovalPolicy::Never,
        ),
        live_ctx(),
        call,
        &h,
    )
    .await;
    assert!(!r.is_error, "{:?}", r.output);
    assert!(r.output.starts_with("exit code: 3"), "{:?}", r.output);
}

/// Port of `TestRunnerEmptyFileResultHasNonEmptyToolContent`: the runner turns
/// an empty tool output into the `(empty output)` placeholder.
#[tokio::test]
async fn empty_output_placeholder() {
    let cwd = tempfile::tempdir().unwrap();
    write(&cwd.path().join("empty.txt"), b"");
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let call = tool_call("c1", "read_file", json!({"path": "empty.txt"}));
    let opts = options(
        provider(vec![tool_turn(vec![call]), text_turn("done")]),
        cwd.path(),
        Sandbox::WorkspaceWrite,
        ApprovalPolicy::Never,
    );
    let (result, l) = run_loop(opts, live_ctx(), &h).await;
    assert_eq!(result.unwrap(), "done");
    assert_eq!(tool_message(&l, "c1").text, "(empty output)");
}

/// Go `execToolCall`'s `ctx.Err()` check: an already-finished context returns
/// `cancelled: not executed` and never emits exec events.
#[tokio::test]
async fn cancelled_not_executed() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let ctx = live_ctx();
    ctx.cancel.cancel();
    let call = tool_call("c1", "shell", json!({"command": "echo should-not-run"}));
    let r = exec(
        options(
            provider(vec![]),
            cwd.path(),
            Sandbox::DangerFullAccess,
            ApprovalPolicy::Never,
        ),
        ctx,
        call,
        &h,
    )
    .await;
    assert!(r.is_error);
    assert_eq!(r.output, "cancelled: not executed");
    assert_eq!(events.count("exec_command_begin"), 0, "no exec events");
}

/// Ports `TestExecToolCallRejectsInvalidModelOutput` and
/// `TestRunnerRejectsTruncatedToolArguments`.
#[tokio::test]
async fn invalid_arguments_unknown_tool_and_utf8_truncation() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let opts = options(
        provider(vec![]),
        cwd.path(),
        Sandbox::DangerFullAccess,
        ApprovalPolicy::Never,
    );

    let unknown = tool_call("c1", "not_a_real_tool", json!({}));
    let r = exec(opts.clone_for(), live_ctx(), unknown, &h).await;
    assert!(r.is_error);
    assert_eq!(r.output, "unknown tool: not_a_real_tool");

    let malformed = ToolCall {
        id: "c2".into(),
        name: "shell".into(),
        arguments: "not-json".to_string(),
    };
    let r = exec(opts.clone_for(), live_ctx(), malformed, &h).await;
    assert!(r.is_error);
    assert!(
        r.output.starts_with("invalid tool arguments:"),
        "{:?}",
        r.output
    );
    assert!(r.output.contains("received: not-json"), "{:?}", r.output);

    // 500-byte argument string whose 200-byte cut lands inside the 63rd euro.
    let mut long_args = String::from("{\"command\":\"");
    long_args.push_str(&"€".repeat(162));
    long_args.push('é');
    let long_call = ToolCall {
        id: "c3".into(),
        name: "shell".into(),
        arguments: long_args.clone(),
    };
    let r = exec(opts.clone_for(), live_ctx(), long_call, &h).await;
    assert!(r.is_error && r.output.starts_with("invalid tool arguments:"));
    let mut want_received = String::from("received: {\"command\":\"");
    want_received.push_str(&"€".repeat(62));
    want_received.push('…');
    assert!(r.output.contains(&want_received), "{:?}", r.output);
    assert!(
        !r.output.contains(&long_args),
        "untruncated arguments leaked"
    );
    assert!(r.output.is_char_boundary(r.output.len()));
    assert_eq!(
        events.count("exec_command_begin"),
        0,
        "invalid calls never execute"
    );
}

/// Port of `TestRunnerApprovalDenied`: denial text, isError, no exec events,
/// and the recorded approval request (reason + justification + path).
#[tokio::test]
async fn approval_denied() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let (ap, approvals) = approver(false);
    let h = hooks(recording_emitter(&events), ap);
    let justification = "the requested change needs a file write";
    let call = tool_call(
        "call-write",
        "write_file",
        json!({"path": "out.txt", "content": "hello", "justification": justification}),
    );
    let r = exec(
        options(
            provider(vec![]),
            cwd.path(),
            Sandbox::ReadOnly,
            ApprovalPolicy::OnRequest,
        ),
        live_ctx(),
        call,
        &h,
    )
    .await;
    assert!(r.is_error);
    assert_eq!(r.output, "operation denied: approval was not granted");
    assert_eq!(events.count("exec_command_begin"), 0, "no exec events");
    assert!(
        !cwd.path().join("out.txt").exists(),
        "denied write not executed"
    );

    let reqs = approvals.0.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0].reason.contains("read-only sandbox"),
        "{:?}",
        reqs[0].reason
    );
    assert!(
        reqs[0].reason.contains(justification),
        "{:?}",
        reqs[0].reason
    );
    assert_eq!(reqs[0].tool, "write_file");
    assert_eq!(reqs[0].path.as_deref(), Some("out.txt"));
}

/// "Denied path is not executed" + "policy per call": a denied write is
/// rejected (no file, no exec events) and a following read on the SAME session
/// is still allowed.
#[tokio::test]
async fn denied_path_not_executed_and_policy_per_call() {
    let cwd = tempfile::tempdir().unwrap();
    write(&cwd.path().join("a.txt"), b"readable");
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let mut l = lease(options(
        provider(vec![]),
        cwd.path(),
        Sandbox::WorkspaceWrite,
        ApprovalPolicy::Never,
    ))
    .await;

    let denied = tool_call(
        "c-deny",
        "write_file",
        json!({"path": "../outside.txt", "content": "nope"}),
    );
    let r1 = exec_on(&mut l, live_ctx(), denied, &h).await;
    assert!(r1.is_error);
    assert!(
        r1.output.starts_with("operation denied by sandbox policy:"),
        "{:?}",
        r1.output
    );
    assert!(!cwd.path().parent().unwrap().join("outside.txt").exists());

    let allowed = tool_call("c-read", "read_file", json!({"path": "a.txt"}));
    let r2 = exec_on(&mut l, live_ctx(), allowed, &h).await;
    assert!(!r2.is_error, "{:?}", r2.output);
    assert_eq!(r2.output, "readable");
    // Only the allowed read produced a begin/end pair.
    assert_eq!(
        events.count("exec_command_begin"),
        1,
        "only the allowed call executed"
    );
}

/// Port of `TestRunnerApplyPatchSingleApprovalListsPaths`: all approval-required
/// paths are combined into ONE request with `; `-joined reasons.
#[tokio::test]
async fn combined_approval_reasons_and_paths() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let (ap, approvals) = approver(true);
    let h = hooks(recording_emitter(&events), ap);
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+x\n*** Add File: b.txt\n+y\n*** End Patch";
    let call = tool_call(
        "c1",
        "apply_patch",
        json!({"patch": patch, "justification": "need files"}),
    );
    let r = exec(
        options(
            provider(vec![]),
            cwd.path(),
            Sandbox::ReadOnly,
            ApprovalPolicy::OnRequest,
        ),
        live_ctx(),
        call,
        &h,
    )
    .await;
    assert!(!r.is_error, "{:?}", r.output);
    assert!(
        r.output
            .starts_with("Success. Updated the following files:"),
        "{:?}",
        r.output
    );
    assert_eq!(fs::read(cwd.path().join("a.txt")).unwrap(), b"x\n");
    assert_eq!(fs::read(cwd.path().join("b.txt")).unwrap(), b"y\n");

    let reqs = approvals.0.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1, "one combined approval");
    assert_eq!(reqs[0].tool, "apply_patch");
    assert_eq!(reqs[0].path.as_deref(), Some("a.txt, b.txt"));
    assert!(
        reqs[0].reason.contains(
            "operation is not permitted by the read-only sandbox; operation is not permitted by the read-only sandbox"
        ),
        "{:?}", reqs[0].reason
    );
    assert!(
        reqs[0].reason.contains("model justification: need files"),
        "{:?}",
        reqs[0].reason
    );
}

/// Port of `TestApprovalPromptNamesTheSandboxEscalation`.
#[tokio::test]
async fn approval_prompt_names_the_sandbox_escalation() {
    for (sandbox, want) in [
        (Sandbox::ReadOnly, true),
        (Sandbox::WorkspaceWrite, true),
        (Sandbox::DangerFullAccess, false),
    ] {
        let cwd = tempfile::tempdir().unwrap();
        let events = Events::new();
        let (ap, approvals) = approver(true);
        let h = hooks(recording_emitter(&events), ap);
        // `echo hi > out.txt` is not allowlisted (redirect), so untrusted asks.
        let call = tool_call("c1", "shell", json!({"command": "echo hi > out.txt"}));
        let r = exec(
            options(
                provider(vec![]),
                cwd.path(),
                sandbox,
                ApprovalPolicy::Untrusted,
            ),
            live_ctx(),
            call,
            &h,
        )
        .await;
        assert!(!r.is_error, "{:?}", r.output);
        let reqs = approvals.0.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1, "one approval for {sandbox:?}");
        assert_eq!(reqs[0].tool, "shell");
        assert_eq!(reqs[0].command.as_deref(), Some("echo hi > out.txt"));
        let has_note = reqs[0].reason.contains("without the kernel sandbox");
        assert_eq!(
            has_note, want,
            "reason for {sandbox:?} = {:?}",
            reqs[0].reason
        );
    }
}

/// "Approved shell escalation omitting the Landlock launcher": a human-approved
/// command runs unwrapped (bash executes, marker appears); the same command in
/// an unapproved, sandboxed session goes through the launcher and bash never
/// runs (no marker).
#[tokio::test]
async fn approved_shell_escalation_omits_launcher() {
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("approved-marker");

    // Approved (untrusted + granted) -> no launcher -> command runs directly.
    let cwd1 = tempfile::tempdir().unwrap();
    let events1 = Events::new();
    let h1 = hooks(recording_emitter(&events1), approver(true).0);
    let cmd =
        json!({"command": format!("echo RUN_MARKER_123 && touch {}", outside_file.display())});
    let r1 = exec(
        options(
            provider(vec![]),
            cwd1.path(),
            Sandbox::WorkspaceWrite,
            ApprovalPolicy::Untrusted,
        ),
        live_ctx(),
        tool_call("c1", "shell", cmd.clone()),
        &h1,
    )
    .await;
    assert!(r1.output.starts_with("exit code: 0"), "{:?}", r1.output);
    assert!(r1.output.contains("RUN_MARKER_123"), "{:?}", r1.output);
    assert!(outside_file.exists(), "approved command ran unwrapped");

    // Unapproved (never) -> sandboxed -> launcher wraps -> bash never runs.
    let outside_file2 = outside.path().join("never-marker");
    let cmd2 =
        json!({"command": format!("echo RUN_MARKER_456 && touch {}", outside_file2.display())});
    let cwd2 = tempfile::tempdir().unwrap();
    let events2 = Events::new();
    let h2 = hooks(recording_emitter(&events2), approver(true).0);
    let r2 = exec(
        options(
            provider(vec![]),
            cwd2.path(),
            Sandbox::WorkspaceWrite,
            ApprovalPolicy::Never,
        ),
        live_ctx(),
        tool_call("c2", "shell", cmd2),
        &h2,
    )
    .await;
    assert!(
        !r2.output.contains("RUN_MARKER_456"),
        "launcher-wrapped command must not run bash: {:?}",
        r2.output
    );
    assert!(
        !outside_file2.exists(),
        "wrapped command must not write outside roots"
    );
}

/// "workspace-write shell/read/write/patch": confined writes/patches go
/// through the BoundDirectory FS (no EXDEV) and read/write/patch all succeed.
#[tokio::test]
async fn workspace_write_read_write_patch_confined() {
    let cwd = tempfile::tempdir().unwrap();
    write(&cwd.path().join("a.txt"), b"old\n");
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let mut l = lease(options(
        provider(vec![]),
        cwd.path(),
        Sandbox::WorkspaceWrite,
        ApprovalPolicy::Never,
    ))
    .await;

    let r1 = exec_on(
        &mut l,
        live_ctx(),
        tool_call("c1", "read_file", json!({"path": "a.txt"})),
        &h,
    )
    .await;
    assert!(!r1.is_error, "{:?}", r1.output);
    assert_eq!(r1.output, "old\n");

    let content = "written by the agent\n";
    let r2 = exec_on(
        &mut l,
        live_ctx(),
        tool_call(
            "c2",
            "write_file",
            json!({"path": "nested/output.txt", "content": content}),
        ),
        &h,
    )
    .await;
    assert!(!r2.is_error, "{:?}", r2.output);
    assert_eq!(
        r2.output,
        format!("wrote {} bytes to nested/output.txt", content.len())
    );
    assert_eq!(
        fs::read(cwd.path().join("nested/output.txt")).unwrap(),
        content.as_bytes()
    );

    let patch = "*** Begin Patch\n*** Update File: a.txt\n-old\n+new\n*** End Patch";
    let r3 = exec_on(
        &mut l,
        live_ctx(),
        tool_call("c3", "apply_patch", json!({"patch": patch})),
        &h,
    )
    .await;
    assert!(!r3.is_error, "{:?}", r3.output);
    assert!(
        r3.output
            .starts_with("Success. Updated the following files:"),
        "{:?}",
        r3.output
    );
    // Confined paths are rewritten back to display paths, not the held
    // `/proc/self/fd/<fd>` spelling (Go `displayPath`).
    assert!(!r3.output.contains("/proc/self/fd/"), "{:?}", r3.output);
    assert_eq!(fs::read(cwd.path().join("a.txt")).unwrap(), b"new\n");
}

/// "patch transaction failure" and rollout: `record_patch_applied` produces a
/// single `patch_apply_end` per apply_patch (success AND failure), with exact
/// stdout/stderr/changes, and no duplicate tool-call/output rollout records.
#[tokio::test]
async fn patch_end_rollout_and_transaction_failure() {
    let home = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::new(home.path());
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);

    let mut l = lease(options(
        provider(vec![]),
        cwd.path(),
        Sandbox::DangerFullAccess,
        ApprovalPolicy::Never,
    ))
    .await;
    l.rollout =
        Some(RolloutRecorder::create("t23b", Path::new(""), "", "stub", "m", "high").unwrap());

    let ok_patch = "*** Begin Patch\n*** Add File: ok.txt\n+hello\n*** End Patch";
    let r_ok = exec_on(
        &mut l,
        live_ctx(),
        tool_call("c-ok", "apply_patch", json!({"patch": ok_patch})),
        &h,
    )
    .await;
    assert!(!r_ok.is_error, "{:?}", r_ok.output);
    assert_eq!(fs::read(cwd.path().join("ok.txt")).unwrap(), b"hello\n");

    // Two Add Files: the second's parent is the file created by the first, so
    // commit fails and rolls the first write back.
    let fail_patch = "*** Begin Patch\n*** Add File: blocker\n+x\n*** Add File: blocker/child\n+y\n*** End Patch";
    let r_fail = exec_on(
        &mut l,
        live_ctx(),
        tool_call("c-fail", "apply_patch", json!({"patch": fail_patch})),
        &h,
    )
    .await;
    assert!(r_fail.is_error, "{:?}", r_fail.output);
    assert!(r_fail.output.starts_with("error:"), "{:?}", r_fail.output);
    assert!(
        !cwd.path().join("blocker").exists(),
        "failed commit rolled back"
    );

    drop(l);

    let mut files = Vec::new();
    collect_jsonl(home.path(), &mut files);
    assert_eq!(files.len(), 1, "one rollout file");
    let lines: Vec<Value> = fs::read_to_string(&files[0])
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // Exactly two records, both patch_apply_end: the executor must not
    // duplicate the runner's tool_call / tool_output records.
    assert_eq!(lines.len(), 2, "only patch_apply_end records: {lines:?}");
    for line in &lines {
        assert_eq!(line["type"], "event_msg");
        assert_eq!(line["payload"]["type"], "patch_apply_end");
    }
    let ok = lines
        .iter()
        .find(|l| l["payload"]["success"] == true)
        .expect("success patch_apply_end");
    assert_eq!(ok["payload"]["call_id"], "c-ok");
    assert_eq!(ok["payload"]["stderr"], "");
    assert!(
        ok["payload"]["stdout"]
            .as_str()
            .unwrap()
            .starts_with("Success.")
    );
    assert!(
        ok["payload"]["changes"]
            .as_object()
            .unwrap()
            .contains_key(&cwd.path().join("ok.txt").display().to_string())
    );
    let fail = lines
        .iter()
        .find(|l| l["payload"]["success"] == false)
        .expect("failure patch_apply_end");
    assert_eq!(fail["payload"]["call_id"], "c-fail");
    assert_eq!(fail["payload"]["stdout"], "");
    assert!(
        fail["payload"]["stderr"]
            .as_str()
            .unwrap()
            .starts_with("error:")
    );
    assert_eq!(fail["payload"]["changes"].as_object().unwrap().len(), 0);
}

/// Panic AFTER an `.await` becomes `internal error: tool execution panicked`.
/// Proves `catch_unwind` wraps the polling boundary, not only future creation.
#[tokio::test]
async fn panic_after_await_recovers() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let h = hooks(recording_emitter(&events), Arc::new(PanicApprover));
    let call = tool_call("c1", "shell", json!({"command": "touch panic-probe"}));
    let r = exec(
        options(
            provider(vec![]),
            cwd.path(),
            Sandbox::DangerFullAccess,
            ApprovalPolicy::Untrusted,
        ),
        live_ctx(),
        call,
        &h,
    )
    .await;
    assert!(r.is_error);
    assert_eq!(
        r.output,
        "internal error: tool execution panicked: approver boom"
    );
    assert_eq!(
        events.count("exec_command_begin"),
        0,
        "panic was before exec events"
    );
    assert!(!cwd.path().join("panic-probe").exists(), "tool did not run");
}

/// Port of `TestRunnerRecoversToolExecutionPanicAndCompletesHistory`: a panic
/// during `exec_command_begin` still emits the deferred `exec_command_end`
/// (exit_code -1, no error), is recovered, and the next call runs normally.
#[tokio::test]
async fn emitter_panic_still_emits_end_events() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let emitter = Arc::new(PanicOnceEmitter {
        events: events.clone(),
        panic_type: "exec_command_begin",
        fired: AtomicBool::new(false),
    });
    let h = hooks(emitter, approver(true).0);
    let mut l = lease(options(
        provider(vec![]),
        cwd.path(),
        Sandbox::DangerFullAccess,
        ApprovalPolicy::Never,
    ))
    .await;

    let r1 = exec_on(
        &mut l,
        live_ctx(),
        tool_call("c1", "shell", json!({"command": "echo should-not-run"})),
        &h,
    )
    .await;
    assert!(r1.is_error);
    assert!(
        r1.output
            .contains("internal error: tool execution panicked: emitter boom"),
        "{:?}",
        r1.output
    );
    let r2 = exec_on(
        &mut l,
        live_ctx(),
        tool_call("c2", "shell", json!({"command": "echo after-panic"})),
        &h,
    )
    .await;
    assert!(
        !r2.is_error && r2.output.contains("after-panic"),
        "{:?}",
        r2.output
    );

    assert_eq!(
        events.types(),
        vec![
            "exec_command_begin",
            "exec_command_end",
            "exec_command_begin",
            "exec_command_end",
        ],
        "begin/end for both calls"
    );
    let end1 = events.nth("exec_command_end", 0).unwrap();
    assert_eq!(end1["call_id"], "c1");
    assert_eq!(end1["tool"], "shell");
    assert_eq!(end1["exit_code"], -1);
    assert!(end1.get("error").is_none(), "panic end event has no error");
}

/// "exact error text": Task 19 ToolError becomes an isError tool result
/// `error: <message>` (not a JSON-RPC error), invalid patch, and shell timeout
/// composition keeps captured output with `\n[error: ...]`.
#[tokio::test]
async fn exact_error_texts() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let opts = options(
        provider(vec![]),
        cwd.path(),
        Sandbox::DangerFullAccess,
        ApprovalPolicy::Never,
    );

    let missing = tool_call("c1", "read_file", json!({"path": "no-such-file.txt"}));
    let r = exec(opts.clone_for(), live_ctx(), missing, &h).await;
    assert!(
        r.is_error && r.output.starts_with("error: "),
        "{:?}",
        r.output
    );

    let bad_patch = tool_call("c2", "apply_patch", json!({"patch": "not a patch"}));
    let r = exec(opts.clone_for(), live_ctx(), bad_patch, &h).await;
    assert!(
        r.is_error && r.output.starts_with("error: invalid patch:"),
        "{:?}",
        r.output
    );

    // Write to a path whose parent is a regular file -> ToolError -> `error:`.
    write(&cwd.path().join("blocker"), b"x");
    let bad_write = tool_call(
        "c3",
        "write_file",
        json!({"path": "blocker/child.txt", "content": "y"}),
    );
    let r = exec(opts.clone_for(), live_ctx(), bad_write, &h).await;
    assert!(
        r.is_error && r.output.starts_with("error: "),
        "{:?}",
        r.output
    );
}

/// Task 23A's parked requirement: a run deadline elapsing DURING an in-flight
/// tool must terminate the tool (killing the process group), not just be caught
/// by the runner's before/after checks.
#[tokio::test]
async fn deadline_during_tool_kills_process() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let marker = "deadline-marker";
    let cmd = format!("sleep 5 && touch {marker}");
    let call = tool_call("c1", "shell", json!({"command": cmd.clone()}));
    let mut opts = options(
        provider(vec![tool_turn(vec![call]), text_turn("done")]),
        cwd.path(),
        Sandbox::DangerFullAccess,
        ApprovalPolicy::Never,
    );
    opts.run_deadline = Some(Duration::from_millis(200));

    let started = Instant::now();
    let (result, _) = run_loop(opts, live_ctx(), &h).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "tool overran the deadline: {elapsed:?}"
    );
    assert!(result.is_err(), "run should end on the deadline");
    assert!(
        result.unwrap_err().message.contains("run deadline"),
        "deadline error text"
    );
    assert!(
        !cwd.path().join(marker).exists(),
        "process group was not killed"
    );
}

/// "shell timeout kills the process group": a short `timeout_seconds` kills the
/// whole group so the trailing `touch` never runs, and Go's
/// `exit code: %d\n%s` + `\n[error: %s]` composition is exact.
#[tokio::test]
async fn shell_timeout_kills_process_group() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let h = hooks(recording_emitter(&events), approver(true).0);
    let call = tool_call(
        "c1",
        "shell",
        json!({"command": "echo before-timeout; sleep 5 && touch timeout-marker", "timeout_seconds": 1}),
    );
    let started = Instant::now();
    let r = exec(
        options(
            provider(vec![]),
            cwd.path(),
            Sandbox::DangerFullAccess,
            ApprovalPolicy::Never,
        ),
        live_ctx(),
        call,
        &h,
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "timeout did not fire promptly"
    );
    assert!(r.is_error, "{:?}", r.output);
    assert!(r.output.starts_with("exit code: -1\n"), "{:?}", r.output);
    assert!(
        r.output.contains("before-timeout"),
        "captured output was lost: {:?}",
        r.output
    );
    assert!(
        r.output.contains("\n[error: command timed out after 1s]"),
        "{:?}",
        r.output
    );
    assert!(
        !cwd.path().join("timeout-marker").exists(),
        "process group was not killed"
    );
}

/// The runner's cancel-during-batch path: the remaining call gets
/// `cancelled: not executed`, and every tool call keeps exactly one result.
#[tokio::test]
async fn cancel_skips_remaining_tool_calls() {
    let cwd = tempfile::tempdir().unwrap();
    let events = Events::new();
    let cancel = CancellationToken::new();
    let emitter = Arc::new(CancelOnBeginEmitter {
        events: events.clone(),
        cancel: cancel.clone(),
        fired: AtomicBool::new(false),
    });
    let h = hooks(emitter, approver(true).0);
    let calls = vec![
        tool_call("c1", "shell", json!({"command": "echo one"})),
        tool_call("c2", "shell", json!({"command": "echo two"})),
    ];
    let opts = options(
        provider(vec![tool_turn(calls.clone()), text_turn("done")]),
        cwd.path(),
        Sandbox::DangerFullAccess,
        ApprovalPolicy::Never,
    );
    let ctx = RunContext {
        cancel,
        deadline: None,
    };
    let (result, l) = run_loop(opts, ctx, &h).await;
    assert!(result.is_err(), "cancelled run reports an error");
    let skipped = tool_message(&l, "c2");
    assert_eq!(skipped.text, "cancelled: not executed");
    assert!(skipped.is_error, "skipped result is isError");
    // Complete history: each tool call has exactly one tool message.
    for c in &calls {
        let n = l
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(c.id.as_str()))
            .count();
        assert_eq!(n, 1, "call {} has {} tool messages", c.id, n);
    }
}

// ---------------------------------------------------------------------------
// Rollout helpers (env guard is process-wide; only this test changes env)
// ---------------------------------------------------------------------------

struct EnvGuard {
    old: Option<String>,
}

impl EnvGuard {
    fn new(home: &Path) -> Self {
        let old = std::env::var("CODEX_HOME").ok();
        // SAFETY: only this test mutates CODEX_HOME, and it restores it on drop.
        unsafe { std::env::set_var("CODEX_HOME", home) };
        EnvGuard { old }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.old {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
    }
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect_jsonl(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(p);
            }
        }
    }
}

/// Small helper so tests can build many sessions from one options value.
trait CloneFor {
    fn clone_for(&self) -> Self;
}
impl CloneFor for SessionOptions {
    fn clone_for(&self) -> Self {
        SessionOptions {
            provider: self.provider.clone(),
            thread: self.thread.clone(),
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            effort_sent: self.effort_sent.clone(),
            cwd: self.cwd.clone(),
            sandbox: self.sandbox,
            approval: self.approval,
            system_prompt: self.system_prompt.clone(),
            max_turns: self.max_turns,
            max_nudges: self.max_nudges,
            writable_roots: self.writable_roots.clone(),
            max_history_bytes: self.max_history_bytes,
            max_total_tokens: self.max_total_tokens,
            run_deadline: self.run_deadline,
        }
    }
}
