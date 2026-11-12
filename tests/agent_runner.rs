//! Integration tests for Task 23A: the provider-agnostic turn loop.
//!
//! Ports the turn-loop half of Go `internal/agent/runner.go` (`RunLocked`),
//! `runner_test.go`, `runner_nudge_test.go`, and `session_budget_test.go`.
//! The in-repo Go tree is normative for exact prompts, errors, events and
//! ordering. Tool execution is delegated to a fake `ToolExecutor`; this suite
//! never implements tool bodies or policy.

#![allow(clippy::await_holding_lock)] // process-global env lock, intentionally held in current-thread tests

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use subagent_mcp::agent::{
    CANCELLED_TOOL_RESULT, CONTINUE_AFTER_EMPTY_PROMPT, CONTINUE_PROMPT, DELTA_FLUSH_BYTES,
    DELTA_FLUSH_INTERVAL, DeltaCoalescer, Emitter, RunHooks, Runner, SessionLease, SessionManager,
    SessionOptions, ToolExecutor, ToolResult, append_message, budget_error, message_size,
    rollout_writable_roots, truncate_messages,
};
use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::provider::{
    DeltaSink, Message, Provider, ProviderError, Role, Thread, ThreadCallbacks, ToolCall, ToolSpec,
    TurnRequest, TurnResult, Usage,
};
use subagent_mcp::rollout::RolloutRecorder;
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;
use tokio::sync::{Barrier, Notify};
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
    fn first(&self, t: &str) -> Option<Value> {
        self.all()
            .into_iter()
            .find(|(_, e)| e.get("type").and_then(|v| v.as_str()) == Some(t))
            .map(|(_, e)| e)
    }
    fn deltas_text(&self) -> String {
        self.emitted_deltas().concat()
    }
    fn emitted_deltas(&self) -> Vec<String> {
        self.all()
            .into_iter()
            .filter(|(_, e)| e.get("type").and_then(|v| v.as_str()) == Some("agent_message_delta"))
            .filter_map(|(_, e)| e.get("delta").and_then(|v| v.as_str()).map(String::from))
            .collect()
    }
    fn indices_of(&self, t: &str) -> Vec<usize> {
        self.types()
            .iter()
            .enumerate()
            .filter(|(_, x)| x.as_str() == t)
            .map(|(i, _)| i)
            .collect()
    }
    fn has_message_containing(&self, needle: &str) -> bool {
        self.all().iter().any(|(_, e)| {
            e.get("message")
                .and_then(|v| v.as_str())
                .map(|m| m.contains(needle))
                .unwrap_or(false)
        })
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

fn emitter(events: &Events) -> Arc<dyn Emitter> {
    Arc::new(RecordingEmitter {
        events: events.clone(),
    })
}

#[derive(Clone, Default)]
struct Approvals(Arc<Mutex<Vec<String>>>);

struct RecordingApprover {
    approvals: Approvals,
    decision: bool,
}

#[async_trait]
impl Approver for RecordingApprover {
    async fn approve(&self, _ctx: &RunContext, req: ApprovalRequest) -> bool {
        self.approvals.0.lock().unwrap().push(req.thread_id.clone());
        self.decision
    }
}

fn approver(approvals: &Approvals) -> Arc<dyn Approver> {
    Arc::new(RecordingApprover {
        approvals: approvals.clone(),
        decision: true,
    })
}

fn hooks(events: &Events) -> RunHooks {
    RunHooks {
        emitter: emitter(events),
        approver: approver(&Approvals::default()),
    }
}

// ---------------------------------------------------------------------------
// Provider fakes
// ---------------------------------------------------------------------------

struct StubTurn {
    result: Result<TurnResult, String>,
    deltas: Vec<String>,
}

fn text_turn(text: &str) -> StubTurn {
    StubTurn {
        result: Ok(TurnResult {
            text: text.into(),
            ..Default::default()
        }),
        deltas: vec![],
    }
}

fn err_turn(msg: &str) -> StubTurn {
    StubTurn {
        result: Err(msg.into()),
        deltas: vec![],
    }
}

fn tool_turn(calls: Vec<ToolCall>) -> StubTurn {
    StubTurn {
        result: Ok(TurnResult {
            tool_calls: calls,
            ..Default::default()
        }),
        deltas: vec![],
    }
}

struct StubState {
    turns: VecDeque<StubTurn>,
    requests: Vec<TurnRequest>,
}

struct StubProvider {
    state: Arc<Mutex<StubState>>,
}

impl StubProvider {
    fn new(turns: Vec<StubTurn>) -> Self {
        Self {
            state: Arc::new(Mutex::new(StubState {
                turns: turns.into(),
                requests: vec![],
            })),
        }
    }
    fn requests(&self) -> Vec<TurnRequest> {
        self.state.lock().unwrap().requests.clone()
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
        req: TurnRequest,
        mut on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        let turn = {
            let mut st = self.state.lock().unwrap();
            st.requests.push(req);
            st.turns.pop_front()
        };
        let Some(turn) = turn else {
            return Err(ProviderError {
                message: "stub provider has no queued turn".into(),
            });
        };
        for d in &turn.deltas {
            on_delta(d.clone());
        }
        match turn.result {
            Ok(r) => Ok(r),
            Err(e) => Err(ProviderError { message: e }),
        }
    }
}

/// Blocks forever; used to prove the run deadline fires during a model call.
struct ForeverProvider;

#[async_trait]
impl Provider for ForeverProvider {
    fn name(&self) -> &str {
        "stub"
    }
    async fn turn(
        &self,
        _ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        std::future::pending::<Result<TurnResult, ProviderError>>().await
    }
}

/// Signals entry and then blocks on the run context's cancellation; the runner
/// must observe the cancellation in its guarded select and roll back history.
struct CancelBlockingProvider {
    entered: Arc<Notify>,
}

#[async_trait]
impl Provider for CancelBlockingProvider {
    fn name(&self) -> &str {
        "stub"
    }
    async fn turn(
        &self,
        ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        self.entered.notify_one();
        ctx.cancel.cancelled().await;
        Err(ProviderError {
            message: "context canceled".into(),
        })
    }
}

/// Two runs share a barrier so both are genuinely in flight simultaneously;
/// this is the regression condition for per-call progress hooks.
struct ConcurrentProvider {
    counter: Mutex<usize>,
    barrier: Arc<Barrier>,
    text: String,
}

#[async_trait]
impl Provider for ConcurrentProvider {
    fn name(&self) -> &str {
        "stub"
    }
    async fn turn(
        &self,
        _ctx: RunContext,
        _req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        let n = {
            let mut c = self.counter.lock().unwrap();
            let n = *c;
            *c += 1;
            n
        };
        if n == 0 {
            self.barrier.wait().await;
            Ok(TurnResult {
                tool_calls: vec![ToolCall {
                    id: format!("c-{}", self.text),
                    name: "shell".into(),
                    arguments: "{}".into(),
                }],
                ..Default::default()
            })
        } else {
            Ok(TurnResult {
                text: self.text.clone(),
                ..Default::default()
            })
        }
    }
}

// ---------------------------------------------------------------------------
// ToolExecutor fakes
// ---------------------------------------------------------------------------

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: format!("{name} tool"),
        parameters: serde_json::value::RawValue::from_string("{}".to_string()).unwrap(),
    }
}

struct StubExecutor {
    tools: Vec<ToolSpec>,
    outputs: Mutex<VecDeque<ToolResult>>,
    calls: Mutex<Vec<ToolCall>>,
    emit_events: bool,
    use_approver: bool,
}

impl StubExecutor {
    fn new(tools: Vec<ToolSpec>, outputs: Vec<ToolResult>) -> Self {
        Self {
            tools,
            outputs: Mutex::new(outputs.into()),
            calls: Mutex::new(vec![]),
            emit_events: false,
            use_approver: false,
        }
    }
    fn with_events(mut self) -> Self {
        self.emit_events = true;
        self
    }
    fn with_approver(mut self) -> Self {
        self.use_approver = true;
        self
    }
    fn calls(&self) -> Vec<ToolCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ToolExecutor for StubExecutor {
    fn tools(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }
    async fn execute(
        &self,
        ctx: &RunContext,
        lease: &mut SessionLease,
        call: &ToolCall,
        hooks: &RunHooks,
        _turn_id: &str,
    ) -> ToolResult {
        self.calls.lock().unwrap().push(call.clone());
        if self.use_approver {
            let _ = hooks
                .approver
                .approve(
                    ctx,
                    ApprovalRequest {
                        thread_id: lease.id().to_string(),
                        tool: call.name.clone(),
                        command: None,
                        path: None,
                        reason: "test".into(),
                    },
                )
                .await;
        }
        if self.emit_events {
            hooks.emitter.emit(
                lease.id(),
                json!({"type":"exec_command_begin","call_id":call.id,"tool":call.name}),
            );
        }
        let result = self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ToolResult {
                output: "ok".into(),
                is_error: false,
            });
        if self.emit_events {
            hooks.emitter.emit(
                lease.id(),
                json!({"type":"exec_command_end","call_id":call.id,"tool":call.name}),
            );
        }
        result
    }
}

/// On the gated call, signals entry and blocks until the run context is
/// cancelled, then returns a real result. Remaining calls must be skipped.
struct GateExecutor {
    entered: Arc<Notify>,
    outputs: Mutex<VecDeque<ToolResult>>,
    calls: Mutex<Vec<ToolCall>>,
    gate_id: String,
}

#[async_trait]
impl ToolExecutor for GateExecutor {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![spec("shell"), spec("write_file")]
    }
    async fn execute(
        &self,
        ctx: &RunContext,
        _lease: &mut SessionLease,
        call: &ToolCall,
        _hooks: &RunHooks,
        _turn_id: &str,
    ) -> ToolResult {
        self.calls.lock().unwrap().push(call.clone());
        let result = self
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ToolResult {
                output: "ok".into(),
                is_error: false,
            });
        if call.id == self.gate_id {
            self.entered.notify_one();
            ctx.cancel.cancelled().await;
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn options(provider: Arc<dyn Provider>) -> SessionOptions {
    SessionOptions {
        provider,
        thread: None,
        model: String::new(),
        reasoning_effort: String::new(),
        effort_sent: None,
        cwd: PathBuf::new(), // empty cwd -> binding skipped, fast
        sandbox: Sandbox::DangerFullAccess,
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

fn run_ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

async fn lease(opts: SessionOptions) -> SessionLease {
    let m = SessionManager::new();
    let s = m.create(opts).await.unwrap();
    let id = s.id.clone();
    m.acquire(&id).await.unwrap()
}

fn tool_call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

fn last_role_text(req: &TurnRequest) -> (Role, String) {
    let m = req.messages.last().expect("request has messages");
    (m.role, m.text.clone())
}

fn stub_executor() -> Arc<dyn ToolExecutor> {
    Arc::new(StubExecutor::new(vec![spec("shell")], vec![]))
}

fn event_types_without(types: &[String], drop: &str) -> Vec<String> {
    types
        .iter()
        .filter(|t| t.as_str() != drop)
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Turn loop basics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pure_text_one_turn() {
    let provider = Arc::new(StubProvider::new(vec![text_turn("done")]));
    let events = Events::new();
    let mut l = lease(options(provider.clone())).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };

    let got = runner
        .run_locked(run_ctx(), &mut l, "finish the task", &hooks)
        .await
        .unwrap();
    assert_eq!(got, "done");
    assert_eq!(
        events.types(),
        vec![
            "task_started",
            "provider_request",
            "agent_message",
            "task_complete"
        ]
    );
    // Absent usage emits no token_count.
    assert_eq!(events.count("token_count"), 0);
    let last = l.messages.last().unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert_eq!(last.text, "done");

    let req = events.first("provider_request").unwrap();
    assert_eq!(req["provider"], "stub");
    assert!(req.get("duration_ms").unwrap().is_number());
    assert!(req.get("ttft_ms").is_none());
    assert!(req.get("error").is_none());
}

#[tokio::test]
async fn includes_reasoning_effort_in_request() {
    for (effort, want) in [(None, "high"), (Some("low"), "low")] {
        let provider = Arc::new(StubProvider::new(vec![text_turn("done")]));
        let recorded = provider.clone();
        let events = Events::new();
        let mut opts = options(provider);
        if let Some(e) = effort {
            opts.reasoning_effort = e.into();
        }
        let mut l = lease(opts).await;
        let hooks = hooks(&events);
        let runner = Runner {
            executor: stub_executor(),
        };
        runner
            .run_locked(run_ctx(), &mut l, "task", &hooks)
            .await
            .unwrap();
        let reqs = recorded.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].effort.as_deref(), Some(want));
    }
}

#[tokio::test]
async fn tool_specs_come_from_executor() {
    let provider = Arc::new(StubProvider::new(vec![text_turn("done")]));
    let recorded = provider.clone();
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let tools = vec![spec("shell"), spec("read_file"), spec("write_file")];
    let runner = Runner {
        executor: Arc::new(StubExecutor::new(tools.clone(), vec![])),
    };
    runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    let reqs = recorded.requests();
    assert_eq!(reqs.len(), 1);
    let got_names: Vec<&str> = reqs[0].tools.iter().map(|t| t.name.as_str()).collect();
    let want_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        got_names, want_names,
        "tools must come from executor.tools()"
    );
}

#[tokio::test]
async fn tool_call_then_text() {
    let call = tool_call("call-shell", "shell", r#"{"command":"echo hi"}"#);
    let provider = Arc::new(StubProvider::new(vec![
        tool_turn(vec![call.clone()]),
        text_turn("ok"),
    ]));
    let events = Events::new();
    let mut l = lease(options(provider.clone())).await;
    let hooks = hooks(&events);
    let executor = Arc::new(StubExecutor::new(
        vec![spec("shell")],
        vec![ToolResult {
            output: "hi there".into(),
            is_error: false,
        }],
    ));
    let runner = Runner {
        executor: executor.clone(),
    };

    let got = runner
        .run_locked(run_ctx(), &mut l, "say hi", &hooks)
        .await
        .unwrap();
    assert_eq!(got, "ok");
    assert_eq!(provider.requests().len(), 2);
    // The tool result is appended with the matching call id.
    let tool_msg = l
        .messages
        .iter()
        .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some("call-shell"))
        .unwrap();
    assert!(tool_msg.text.contains("hi"));
    assert_eq!(executor.calls(), vec![call]);
    // The runner itself emits only loop events; exec events belong to the
    // executor (Task 23B), so none appear here.
    assert_eq!(
        events.types(),
        vec![
            "task_started",
            "provider_request",
            "provider_request",
            "agent_message",
            "task_complete",
        ]
    );
}

#[tokio::test]
async fn empty_tool_output_gets_placeholder() {
    let call = tool_call("call-empty", "read_file", r#"{"path":"empty.txt"}"#);
    let provider = Arc::new(StubProvider::new(vec![
        tool_turn(vec![call]),
        text_turn("handled"),
    ]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: Arc::new(StubExecutor::new(
            vec![spec("read_file")],
            vec![ToolResult {
                output: String::new(),
                is_error: false,
            }],
        )),
    };
    runner
        .run_locked(run_ctx(), &mut l, "read", &hooks)
        .await
        .unwrap();
    let tool_msg = l.messages.iter().find(|m| m.role == Role::Tool).unwrap();
    assert_eq!(tool_msg.text, "(empty output)");
}

#[tokio::test]
async fn tool_call_batching_handoff() {
    let c1 = tool_call("c1", "shell", "{}");
    let c2 = tool_call("c2", "shell", "{}");
    let provider = Arc::new(StubProvider::new(vec![
        tool_turn(vec![c1.clone(), c2.clone()]),
        text_turn("done"),
    ]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let executor = Arc::new(StubExecutor::new(
        vec![spec("shell")],
        vec![
            ToolResult {
                output: "one".into(),
                is_error: false,
            },
            ToolResult {
                output: "two".into(),
                is_error: true,
            },
        ],
    ));
    let runner = Runner {
        executor: executor.clone(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "batch", &hooks)
        .await
        .unwrap();
    assert_eq!(executor.calls(), vec![c1, c2]);
    let tools: Vec<&Message> = l.messages.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(tools[0].text, "one");
    assert!(!tools[0].is_error);
    assert_eq!(tools[1].tool_call_id.as_deref(), Some("c2"));
    assert_eq!(tools[1].text, "two");
    assert!(tools[1].is_error);
}

#[tokio::test]
async fn passes_opaque_back_in_history() {
    let opaque = serde_json::value::RawValue::from_string(r#"{"k":1}"#.to_string()).unwrap();
    let call = tool_call("c1", "shell", "{}");
    let provider = Arc::new(StubProvider::new(vec![
        StubTurn {
            result: Ok(TurnResult {
                tool_calls: vec![call],
                opaque: Some(opaque),
                ..Default::default()
            }),
            deltas: vec![],
        },
        text_turn("done"),
    ]));
    let events = Events::new();
    let mut l = lease(options(provider.clone())).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "go", &hooks)
        .await
        .unwrap();
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 2);
    let assistant = reqs[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
        .expect("assistant message carrying the tool call");
    assert_eq!(
        assistant.opaque.as_ref().map(|o| o.get()),
        Some(r#"{"k":1}"#)
    );
}

#[tokio::test]
async fn turn_limit_reached() {
    let c1 = tool_call("c1", "shell", "{}");
    let c2 = tool_call("c2", "shell", "{}");
    let provider = Arc::new(StubProvider::new(vec![
        tool_turn(vec![c1]),
        tool_turn(vec![c2]),
        text_turn("never reached"),
    ]));
    let events = Events::new();
    let mut opts = options(provider);
    opts.max_turns = 2;
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let err = runner
        .run_locked(run_ctx(), &mut l, "keep going", &hooks)
        .await
        .unwrap_err();
    assert_eq!(err.message, "turn limit reached (2) without a final answer");
    assert_eq!(events.count("error"), 1);
}

#[tokio::test]
async fn client_error_restores_history_and_unlocks() {
    let provider = Arc::new(StubProvider::new(vec![
        err_turn("upstream failed"),
        text_turn("recovered"),
    ]));
    let events = Events::new();
    let mut l = lease(options(provider.clone())).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };

    let err = runner
        .run_locked(run_ctx(), &mut l, "first prompt", &hooks)
        .await
        .unwrap_err();
    assert_eq!(err.message, "upstream failed");
    assert_eq!(l.messages.len(), 0, "failed prompt must be dropped");
    assert_eq!(l.history_bytes, 0, "history byte counter must roll back");
    assert_eq!(
        events.types(),
        vec!["task_started", "provider_request", "error"]
    );

    let got = runner
        .run_locked(run_ctx(), &mut l, "second prompt", &hooks)
        .await
        .unwrap();
    assert_eq!(got, "recovered");
    assert_eq!(l.messages.len(), 2);
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn failed_model_call_restores_history_to_entry_length() {
    let provider = Arc::new(StubProvider::new(vec![
        text_turn("warmup"),
        err_turn("upstream failed"),
        text_turn("recovered"),
    ]));
    let events = Events::new();
    let mut l = lease(options(provider.clone())).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };

    runner
        .run_locked(run_ctx(), &mut l, "first prompt", &hooks)
        .await
        .unwrap();
    let entry = l.messages.len();
    let entry_bytes = l.history_bytes;
    assert_eq!(entry, 2);

    let err = runner
        .run_locked(run_ctx(), &mut l, "abandoned prompt", &hooks)
        .await
        .unwrap_err();
    assert_eq!(err.message, "upstream failed");
    assert_eq!(l.messages.len(), entry);
    assert_eq!(l.history_bytes, entry_bytes);

    let got = runner
        .run_locked(run_ctx(), &mut l, "third prompt", &hooks)
        .await
        .unwrap();
    assert_eq!(got, "recovered");
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);
    let user_texts: Vec<String> = reqs[2]
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .map(|m| m.text.clone())
        .collect();
    assert_eq!(user_texts, vec!["first prompt", "third prompt"]);
}

#[tokio::test]
async fn provider_failure_emits_request_before_error() {
    let provider = Arc::new(StubProvider::new(vec![err_turn("upstream failed")]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "fail", &hooks)
        .await
        .unwrap_err();
    assert_eq!(
        events.types(),
        vec!["task_started", "provider_request", "error"]
    );
    let req = events.first("provider_request").unwrap();
    assert_eq!(req["error"], "upstream failed");
    assert!(req.get("duration_ms").unwrap().is_number());
}

#[tokio::test]
async fn absent_usage_emits_no_token_count() {
    let provider = Arc::new(StubProvider::new(vec![text_turn("done")]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    assert_eq!(events.count("token_count"), 0);
}

#[tokio::test]
async fn provider_request_carries_the_upstream() {
    let provider = Arc::new(StubProvider::new(vec![StubTurn {
        result: Ok(TurnResult {
            text: "one".into(),
            upstream: Some("relace".into()),
            ..Default::default()
        }),
        deltas: vec![],
    }]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    let req = events.first("provider_request").unwrap();
    assert_eq!(req["upstream"], "relace");
    assert!(req.get("error").is_none());
}

// ---------------------------------------------------------------------------
// Nudges (Go runner_nudge_test.go)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nudge_continues_after_narration_only_reply() {
    let provider = Arc::new(StubProvider::new(vec![
        tool_turn(vec![tool_call("c1", "shell", "{}")]),
        text_turn("Step 2: write the failing test first."),
        tool_turn(vec![tool_call("c2", "shell", "{}")]),
        text_turn("All done: changed a.go."),
        text_turn("All done: changed a.go."),
    ]));
    let events = Events::new();
    let mut opts = options(provider.clone());
    opts.max_nudges = 2;
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let text = runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    assert_eq!(text, "All done: changed a.go.");
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 5);
    assert_eq!(
        last_role_text(&reqs[2]),
        (Role::User, CONTINUE_PROMPT.into())
    );
}

#[tokio::test]
async fn nudge_disabled_by_default() {
    let provider = Arc::new(StubProvider::new(vec![text_turn("Step 2: next")]));
    let events = Events::new();
    let mut l = lease(options(provider.clone())).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let text = runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    assert_eq!(text, "Step 2: next");
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(events.count("agent_nudge"), 0);
}

#[tokio::test]
async fn nudge_accepts_second_text_only_reply_as_final() {
    let provider = Arc::new(StubProvider::new(vec![
        text_turn("Finished."),
        text_turn("Finished: nothing else to do."),
    ]));
    let events = Events::new();
    let mut opts = options(provider.clone());
    opts.max_nudges = 5;
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let text = runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    assert_eq!(text, "Finished: nothing else to do.");
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn nudge_budget_is_per_run() {
    let provider = Arc::new(StubProvider::new(vec![
        text_turn("Step 1."),
        tool_turn(vec![tool_call("c1", "shell", "{}")]),
        text_turn("Step 2."),
    ]));
    let events = Events::new();
    let mut opts = options(provider.clone());
    opts.max_nudges = 1;
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let text = runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    assert_eq!(text, "Step 2.");
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn nudge_handles_empty_replies() {
    let provider = Arc::new(StubProvider::new(vec![
        text_turn("Summary of the work."),
        text_turn(""),
    ]));
    let events = Events::new();
    let mut opts = options(provider);
    opts.max_nudges = 1;
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let text = runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    assert_eq!(text, "Summary of the work.");
    assert!(
        !l.messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.text.is_empty() && m.tool_calls.is_empty())
    );

    let provider2 = Arc::new(StubProvider::new(vec![
        text_turn(""),
        tool_turn(vec![tool_call("c1", "shell", "{}")]),
        text_turn("Done."),
        text_turn("Done."),
    ]));
    let mut opts2 = options(provider2);
    opts2.max_nudges = 2;
    let mut l2 = lease(opts2).await;
    let runner2 = Runner {
        executor: stub_executor(),
    };
    let text2 = runner2
        .run_locked(run_ctx(), &mut l2, "task", &hooks)
        .await
        .unwrap();
    assert_eq!(text2, "Done.");
    assert!(
        !l2.messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.text.is_empty() && m.tool_calls.is_empty())
    );
}

#[tokio::test]
async fn nudge_emits_an_event() {
    let provider = Arc::new(StubProvider::new(vec![
        text_turn("Step 1."),
        text_turn("Step 1 done."),
    ]));
    let events = Events::new();
    let mut opts = options(provider);
    opts.max_nudges = 1;
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    assert_eq!(events.count("agent_nudge"), 1);
    assert!(events.has_message_containing("without a tool call"));
}

#[tokio::test]
async fn nudge_repeats_on_empty_replies() {
    let provider = Arc::new(StubProvider::new(vec![
        text_turn("Step 2: write the failing test first."),
        text_turn(""),
        text_turn(""),
        tool_turn(vec![tool_call("c1", "shell", "{}")]),
        text_turn("Finished."),
        text_turn("Finished."),
    ]));
    let events = Events::new();
    let mut opts = options(provider.clone());
    opts.max_nudges = 4;
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let text = runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap();
    assert_eq!(text, "Finished.");
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 6);
    assert_eq!(
        last_role_text(&reqs[1]),
        (Role::User, CONTINUE_PROMPT.into())
    );
    for i in [2, 3] {
        assert_eq!(
            last_role_text(&reqs[i]),
            (Role::User, CONTINUE_AFTER_EMPTY_PROMPT.into())
        );
    }
}

// ---------------------------------------------------------------------------
// Deltas, usage and tail-flush ordering
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn emits_deltas_and_usage_in_order() {
    let provider = Arc::new(StubProvider::new(vec![StubTurn {
        result: Ok(TurnResult {
            text: "done".into(),
            usage: Some(Usage {
                input: 7,
                output: 5,
                total: 12,
                ..Default::default()
            }),
            ..Default::default()
        }),
        deltas: vec!["do".into(), "ne".into()],
    }]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "stream", &hooks)
        .await
        .unwrap();
    assert_eq!(events.deltas_text(), "done");
    assert_eq!(
        event_types_without(&events.types(), "agent_message_delta"),
        vec![
            "task_started",
            "provider_request",
            "token_count",
            "agent_message",
            "task_complete",
        ]
    );
    let req_index = events.indices_of("provider_request")[0];
    for i in events.indices_of("agent_message_delta") {
        assert!(i < req_index, "delta after provider_request");
    }
    let req = events.first("provider_request").unwrap();
    assert_eq!(req["provider"], "stub");
    assert!(req.get("ttft_ms").unwrap().is_number());
    assert!(req.get("duration_ms").unwrap().is_number());
    let tok = events.first("token_count").unwrap();
    assert_eq!(tok["prompt_tokens"], 7);
    assert_eq!(tok["completion_tokens"], 5);
    assert_eq!(tok["total_tokens"], 12);
}

#[tokio::test]
async fn provider_request_without_deltas_omits_ttft() {
    let provider = Arc::new(StubProvider::new(vec![text_turn("done")]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "finish", &hooks)
        .await
        .unwrap();
    let req = events.first("provider_request").unwrap();
    assert!(req.get("ttft_ms").is_none());
    assert!(req.get("duration_ms").unwrap().is_number());
}

#[tokio::test(start_paused = true)]
async fn paused_stream_tail_is_flushed() {
    // With paused time the 50 ms interval never elapses and the small stream
    // never reaches 8 KiB, so every delta is held until the tail flush.
    let provider = Arc::new(StubProvider::new(vec![StubTurn {
        result: Ok(TurnResult {
            text: "Hello world".into(),
            ..Default::default()
        }),
        deltas: vec!["Hel".into(), "lo ".into(), "wor".into(), "ld".into()],
    }]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "stream", &hooks)
        .await
        .unwrap();
    assert_eq!(events.count("agent_message_delta"), 1);
    assert_eq!(events.deltas_text(), "Hello world");
    let req_index = events.indices_of("provider_request")[0];
    assert!(events.indices_of("agent_message_delta")[0] < req_index);
}

#[tokio::test(start_paused = true)]
async fn tail_flush_ordering_before_all_events() {
    let provider = Arc::new(StubProvider::new(vec![
        StubTurn {
            result: Ok(TurnResult {
                text: "step".into(),
                usage: Some(Usage {
                    input: 1,
                    output: 2,
                    total: 3,
                    ..Default::default()
                }),
                tool_calls: vec![tool_call("c1", "shell", "{}")],
                ..Default::default()
            }),
            deltas: vec!["st".into(), "ep".into()],
        },
        text_turn("done"),
    ]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: Arc::new(StubExecutor::new(vec![spec("shell")], vec![]).with_events()),
    };
    runner
        .run_locked(run_ctx(), &mut l, "stream", &hooks)
        .await
        .unwrap();
    assert_eq!(events.deltas_text(), "step");
    let delta_idx = events.indices_of("agent_message_delta");
    assert!(!delta_idx.is_empty());
    let last_delta = *delta_idx.last().unwrap();
    for t in [
        "provider_request",
        "token_count",
        "exec_command_begin",
        "agent_message",
        "task_complete",
    ] {
        let first = events.indices_of(t)[0];
        assert!(last_delta < first, "a delta came after {t}");
    }
}

// ---------------------------------------------------------------------------
// Budgets (Go session_budget_test.go, turn-loop half)
// ---------------------------------------------------------------------------

#[test]
fn message_size_counts_everything_the_next_turn_resends() {
    assert_eq!(message_size(&Message::default()), 0);
    assert_eq!(
        message_size(&Message {
            text: "hello".into(),
            ..Default::default()
        }),
        5
    );
    assert_eq!(
        message_size(&Message {
            tool_calls: vec![ToolCall {
                id: "ab".into(),
                name: "cde".into(),
                arguments: "fghi".into(),
            }],
            ..Default::default()
        }),
        9
    );
    assert_eq!(
        message_size(&Message {
            opaque: Some(
                serde_json::value::RawValue::from_string(r#"{"a":1}"#.to_string()).unwrap()
            ),
            ..Default::default()
        }),
        7,
        "raw opaque bytes must be counted"
    );
    assert_eq!(
        message_size(&Message {
            tool_call_id: Some("xy".into()),
            ..Default::default()
        }),
        2
    );
}

#[tokio::test]
async fn history_bytes_follow_append_and_truncate() {
    let provider = Arc::new(StubProvider::new(vec![]));
    let mut l = lease(options(provider)).await;
    append_message(
        &mut l,
        Message {
            role: Role::User,
            text: "first".into(),
            ..Default::default()
        },
    );
    assert_eq!(l.history_bytes, 5);
    let entry = l.messages.len();
    append_message(
        &mut l,
        Message {
            role: Role::User,
            text: "second".into(),
            ..Default::default()
        },
    );
    assert_eq!(l.history_bytes, 11);
    truncate_messages(&mut l, entry);
    assert_eq!(l.history_bytes, 5);
    assert_eq!(l.messages.len(), 1);
    truncate_messages(&mut l, 0);
    assert_eq!(l.history_bytes, 0);
}

#[test]
fn budget_error_reports_the_spent_limit() {
    assert!(budget_error(0, 0, 0, 0).is_none());
    let h = budget_error(10, 11, 0, 0).unwrap();
    assert!(h.contains("max_history_bytes"), "{h}");
    assert!(h.contains("thread history reached 11 bytes"), "{h}");
    assert!(budget_error(10, 10, 0, 0).is_none());
    let t = budget_error(0, 0, 100, 100).unwrap();
    assert!(t.contains("max_total_tokens"), "{t}");
    assert!(t.contains("thread spent 100 tokens"), "{t}");
    assert!(budget_error(0, 0, 100, 99).is_none());
}

#[tokio::test]
async fn total_token_budget_refuses_next_turn() {
    let provider = Arc::new(StubProvider::new(vec![
        StubTurn {
            result: Ok(TurnResult {
                tool_calls: vec![tool_call("c1", "shell", "{}")],
                usage: Some(Usage {
                    input: 6,
                    output: 4,
                    total: 10,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            deltas: vec![],
        },
        text_turn("never reached"),
    ]));
    let events = Events::new();
    let mut opts = options(provider.clone());
    opts.max_total_tokens = 10;
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let err = runner
        .run_locked(run_ctx(), &mut l, "task", &hooks)
        .await
        .unwrap_err();
    assert!(err.message.contains("max_total_tokens"), "{err}");
    assert_eq!(l.total_usage.total, 10);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(events.count("error"), 1);
}

#[test]
fn rollout_writable_roots_variants() {
    let roots = vec![PathBuf::from("/a"), PathBuf::from("/b")];
    assert!(rollout_writable_roots(Sandbox::DangerFullAccess, &roots).is_none());
    assert!(rollout_writable_roots(Sandbox::WorkspaceWrite, &[]).is_none());
    assert_eq!(
        rollout_writable_roots(Sandbox::WorkspaceWrite, &roots),
        Some(roots.clone())
    );
    assert_eq!(
        rollout_writable_roots(Sandbox::ReadOnly, &roots),
        Some(vec![])
    );
}

// ---------------------------------------------------------------------------
// DeltaCoalescer unit tests (Go TestDeltaCoalescer*, paused time)
// ---------------------------------------------------------------------------

fn coalescer(events: &Events) -> DeltaCoalescer {
    DeltaCoalescer::new(emitter(events), "t")
}

#[tokio::test(start_paused = true)]
async fn delta_coalescer_merges_without_losing_text() {
    let events = Events::new();
    let mut c = coalescer(&events);
    c.interval = Duration::from_secs(3600); // Go: hold the time flush open
    for d in ["Hel", "lo ", "wor", "ld"] {
        c.add(d);
    }
    assert_eq!(events.count("agent_message_delta"), 0);
    c.flush();
    assert_eq!(events.count("agent_message_delta"), 1);
    assert_eq!(events.deltas_text(), "Hello world");
    c.flush();
    assert_eq!(
        events.count("agent_message_delta"),
        1,
        "empty flush emits nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn delta_coalescer_flushes_on_size() {
    let events = Events::new();
    let mut c = coalescer(&events);
    c.interval = Duration::from_secs(3600);
    let chunk = "x".repeat(DELTA_FLUSH_BYTES / 2);
    for _ in 0..4 {
        c.add(&chunk);
    }
    assert!(
        events.count("agent_message_delta") >= 2,
        "size bound must flush mid-stream"
    );
    c.flush();
    assert_eq!(events.deltas_text().len(), 4 * chunk.len());
}

#[tokio::test(start_paused = true)]
async fn delta_coalescer_flushes_on_interval() {
    let events = Events::new();
    let mut c = coalescer(&events);
    c.add("a");
    tokio::time::advance(Duration::from_millis(51)).await;
    c.add("b"); // elapsed since last >= 50 ms -> flush "ab"
    c.add("c"); // no time passes -> stays buffered
    tokio::time::advance(Duration::from_millis(51)).await;
    c.add("d"); // elapsed since last >= 50 ms -> flush "cd"
    c.flush();
    assert_eq!(events.count("agent_message_delta"), 2);
    assert_eq!(events.deltas_text(), "abcd");
    assert_eq!(events.emitted_deltas(), vec!["ab", "cd"]);
    assert_eq!(DELTA_FLUSH_INTERVAL, Duration::from_millis(50));
}

// ---------------------------------------------------------------------------
// Cancellation, run deadline, and cancellation of a tool batch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancelled_first_model_call_restores_history() {
    let entered = Arc::new(Notify::new());
    let provider = Arc::new(CancelBlockingProvider {
        entered: entered.clone(),
    });
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let cancel = CancellationToken::new();
    let ctx = RunContext {
        cancel: cancel.clone(),
        deadline: None,
    };
    let canceller = async move {
        entered.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(
        runner.run_locked(ctx, &mut l, "cancelled prompt", &hooks),
        canceller
    );
    assert_eq!(result.unwrap_err().message, "context canceled");
    assert_eq!(l.messages.len(), 0, "cancelled prompt must be dropped");
    assert_eq!(l.history_bytes, 0);
}

#[tokio::test]
async fn cancel_stops_remaining_tool_batch() {
    let entered = Arc::new(Notify::new());
    let c1 = tool_call("c1", "shell", "{}");
    let c2 = tool_call("c2", "write_file", "{}");
    let provider = Arc::new(StubProvider::new(vec![
        tool_turn(vec![c1.clone(), c2.clone()]),
        text_turn("recovered"),
    ]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let hooks = hooks(&events);
    let executor = Arc::new(GateExecutor {
        entered: entered.clone(),
        outputs: Mutex::new(
            vec![
                ToolResult {
                    output: "exit code: -1\ninterrupted".into(),
                    is_error: true,
                },
                ToolResult {
                    output: "should not run".into(),
                    is_error: false,
                },
            ]
            .into(),
        ),
        calls: Mutex::new(vec![]),
        gate_id: "c1".into(),
    });
    let runner = Runner {
        executor: executor.clone(),
    };
    let cancel = CancellationToken::new();
    let ctx = RunContext {
        cancel: cancel.clone(),
        deadline: None,
    };
    let canceller = async move {
        entered.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(
        runner.run_locked(ctx, &mut l, "run the batch", &hooks),
        canceller
    );
    assert_eq!(result.unwrap_err().message, "context canceled");
    let tools: Vec<&Message> = l.messages.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(tools[0].text, "exit code: -1\ninterrupted");
    assert!(tools[0].is_error);
    assert_eq!(tools[1].tool_call_id.as_deref(), Some("c2"));
    assert_eq!(tools[1].text, CANCELLED_TOOL_RESULT);
    assert!(tools[1].is_error);
    // Only the in-flight call executed; the rest were skipped.
    assert_eq!(
        executor
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>(),
        vec!["c1"]
    );
}

#[tokio::test]
async fn run_deadline_names_itself() {
    let provider = Arc::new(ForeverProvider);
    let events = Events::new();
    let mut opts = options(provider);
    opts.run_deadline = Some(Duration::from_millis(30));
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    let err = runner
        .run_locked(run_ctx(), &mut l, "prompt", &hooks)
        .await
        .unwrap_err();
    assert!(err.message.contains("run deadline"), "{err}");
    assert!(err.message.contains("30ms"), "{err}");
    assert_eq!(events.count("error"), 1);
}

// ---------------------------------------------------------------------------
// Per-call progress: two concurrent runs emit only to their own sink
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_concurrent_runs_with_different_hooks_emit_to_own_sinks() {
    let barrier = Arc::new(Barrier::new(2));
    let prov_a = Arc::new(ConcurrentProvider {
        counter: Mutex::new(0),
        barrier: barrier.clone(),
        text: "alpha".into(),
    });
    let prov_b = Arc::new(ConcurrentProvider {
        counter: Mutex::new(0),
        barrier,
        text: "beta".into(),
    });
    let events_a = Events::new();
    let events_b = Events::new();
    let approvals_a = Approvals::default();
    let approvals_b = Approvals::default();
    let ma = SessionManager::new();
    let mb = SessionManager::new();
    let sa = ma.create(options(prov_a)).await.unwrap();
    let sb = mb.create(options(prov_b)).await.unwrap();
    let id_a = sa.id.clone();
    let id_b = sb.id.clone();
    let mut la = ma.acquire(&id_a).await.unwrap();
    let mut lb = mb.acquire(&id_b).await.unwrap();
    let hooks_a = RunHooks {
        emitter: emitter(&events_a),
        approver: approver(&approvals_a),
    };
    let hooks_b = RunHooks {
        emitter: emitter(&events_b),
        approver: approver(&approvals_b),
    };
    let executor: Arc<dyn ToolExecutor> = Arc::new(
        StubExecutor::new(vec![spec("shell")], vec![])
            .with_events()
            .with_approver(),
    );
    let runner = Runner { executor };

    let (ra, rb) = tokio::join!(
        runner.run_locked(run_ctx(), &mut la, "task", &hooks_a),
        runner.run_locked(run_ctx(), &mut lb, "task", &hooks_b),
    );
    assert_eq!(ra.unwrap(), "alpha");
    assert_eq!(rb.unwrap(), "beta");

    assert!(
        events_a.all().iter().all(|(id, _)| id == &id_a),
        "run A leaked into another sink"
    );
    assert!(
        events_b.all().iter().all(|(id, _)| id == &id_b),
        "run B leaked into another sink"
    );
    assert!(events_a.has_message_containing("alpha"));
    assert!(events_b.has_message_containing("beta"));
    assert!(!events_a.has_message_containing("beta"));
    assert!(!events_b.has_message_containing("alpha"));
    assert!(
        approvals_a.0.lock().unwrap().iter().all(|id| id == &id_a),
        "approver A saw another thread"
    );
    assert!(
        approvals_b.0.lock().unwrap().iter().all(|id| id == &id_b),
        "approver B saw another thread"
    );
    assert!(!approvals_a.0.lock().unwrap().is_empty());
    assert!(!approvals_b.0.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Rollout: sandbox policy, user message, usage and task records survive
// ---------------------------------------------------------------------------

/// Serializes the tests that mutate process-global environment variables.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    old: Option<String>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn new(home: &Path) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("CODEX_HOME").ok();
        // SAFETY: the ENV_LOCK guard above serializes every env-mutating test
        // in this binary; the guard restores the value on drop.
        unsafe { std::env::set_var("CODEX_HOME", home) };
        EnvGuard { old, _lock: lock }
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

#[tokio::test]
async fn rollout_records_turn_context_usage_and_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::new(tmp.path());

    let provider = Arc::new(StubProvider::new(vec![StubTurn {
        result: Ok(TurnResult {
            text: "done".into(),
            usage: Some(Usage {
                input: 7,
                output: 5,
                total: 12,
                ..Default::default()
            }),
            ..Default::default()
        }),
        deltas: vec![],
    }]));
    let events = Events::new();
    let mut l = lease(options(provider)).await;
    let recorder = RolloutRecorder::create("t", Path::new(""), "", "stub", "m", "high").unwrap();
    l.rollout = Some(recorder);
    let hooks = hooks(&events);
    let runner = Runner {
        executor: stub_executor(),
    };
    runner
        .run_locked(run_ctx(), &mut l, "the prompt", &hooks)
        .await
        .unwrap();

    let mut files = vec![];
    collect_jsonl(tmp.path(), &mut files);
    assert_eq!(files.len(), 1, "one rollout file");
    let raw = fs::read_to_string(&files[0]).unwrap();
    let lines: Vec<Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect();
    // A line's outer "type" is "turn_context"/"response_item"/"event_msg";
    // event_msg payloads carry their own inner "type" (task_started, etc.).
    let find = |t: &str| {
        lines
            .iter()
            .find(|line| {
                line.get("type").and_then(|v| v.as_str()) == Some(t)
                    || line
                        .get("payload")
                        .and_then(|p| p.get("type"))
                        .and_then(|v| v.as_str())
                        == Some(t)
            })
            .map(|line| line.get("payload").cloned().unwrap_or(Value::Null))
    };

    let ctx = find("turn_context").expect("turn_context record");
    assert_eq!(ctx["sandbox_policy"]["type"], "danger-full-access");
    assert!(ctx["sandbox_policy"]["writable_roots"].is_null());
    assert_eq!(ctx["approval_policy"], "never");
    assert_eq!(ctx["model"], "deepseek-v4-pro");
    assert_eq!(ctx["effort"], "high");
    assert_eq!(ctx["effort_sent"], "high");
    assert!(ctx["turn_id"].as_str().unwrap().len() >= 30);

    assert_eq!(find("user_message").unwrap()["message"], "the prompt");
    let tokens = find("token_count").expect("token_count rollout event");
    assert_eq!(tokens["info"]["total_token_usage"]["total_tokens"], 12);
    assert_eq!(tokens["info"]["last_token_usage"]["total_tokens"], 12);
    assert_eq!(find("agent_message").unwrap()["message"], "done");
    assert!(find("provider_request").is_some());
    assert!(find("task_complete").is_some());
}

// ---------------------------------------------------------------------------
// Delegated threads (Go `session_budget_test.go`)
// ---------------------------------------------------------------------------

fn token_count(input: i64, output: i64, total: i64) -> Value {
    json!({
        "type": "token_count",
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": total,
    })
}

fn rollout_lines(dir: &Path) -> Vec<Value> {
    let mut files = vec![];
    collect_jsonl(dir, &mut files);
    assert_eq!(files.len(), 1, "one rollout file: {files:?}");
    fs::read_to_string(&files[0])
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect()
}

fn rollout_event(lines: &[Value], kind: &str) -> Vec<Value> {
    lines
        .iter()
        .filter(|line| {
            line.get("type").and_then(Value::as_str) == Some("event_msg")
                && line
                    .get("payload")
                    .and_then(|p| p.get("type"))
                    .and_then(Value::as_str)
                    == Some(kind)
        })
        .map(|line| line["payload"].clone())
        .collect()
}

/// A delegated thread that emits a fixed event script, then returns text.
struct ScriptedThread {
    runs: Arc<AtomicUsize>,
    events: Vec<Value>,
    text: String,
}

#[async_trait]
impl Thread for ScriptedThread {
    async fn run(
        &self,
        _ctx: RunContext,
        _prompt: String,
        _effort: Option<String>,
        callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        for event in &self.events {
            (callbacks.emit)("thread", event.clone());
        }
        Ok(self.text.clone())
    }

    async fn close(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// A delegated thread that fails with a fixed provider message.
struct FailingThread {
    message: &'static str,
}

#[async_trait]
impl Thread for FailingThread {
    async fn run(
        &self,
        _ctx: RunContext,
        _prompt: String,
        _effort: Option<String>,
        _callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        Err(ProviderError {
            message: self.message.to_string(),
        })
    }

    async fn close(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// A delegated thread that blocks until the run deadline fires, then returns
/// the context error, like Go's stub in `TestThreadRunNamesTheRunDeadline`.
struct DeadlineThread;

#[async_trait]
impl Thread for DeadlineThread {
    async fn run(
        &self,
        ctx: RunContext,
        _prompt: String,
        _effort: Option<String>,
        _callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        match ctx.deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => ctx.cancel.cancelled().await,
        }
        Err(ProviderError {
            message: "context deadline exceeded".into(),
        })
    }

    async fn close(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Emits a usage event, reads the rollout file before returning, then emits a
/// second one. Used to prove usage is recorded in the callback, not after the
/// run, and that rollout lines keep event order.
struct MidRunRolloutThread {
    runs: Arc<AtomicUsize>,
    rollout_path: PathBuf,
    seen: Arc<Mutex<usize>>,
}

#[async_trait]
impl Thread for MidRunRolloutThread {
    async fn run(
        &self,
        _ctx: RunContext,
        _prompt: String,
        _effort: Option<String>,
        callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        (callbacks.emit)("thread", token_count(10, 0, 10));
        let raw = fs::read_to_string(&self.rollout_path).unwrap();
        *self.seen.lock().unwrap() = raw
            .lines()
            .filter(|l| l.contains("\"token_count\""))
            .count();
        (callbacks.emit)("thread", token_count(5, 0, 5));
        Ok("done".into())
    }

    async fn close(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Keeps its callbacks so a test can emit an event after `run` returns, as a
/// provider's background task could.
struct RetainingThread {
    runs: Arc<AtomicUsize>,
    callbacks: Arc<Mutex<Option<ThreadCallbacks>>>,
    events: Vec<Value>,
}

#[async_trait]
impl Thread for RetainingThread {
    async fn run(
        &self,
        _ctx: RunContext,
        _prompt: String,
        _effort: Option<String>,
        callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        for event in &self.events {
            (callbacks.emit)("thread", event.clone());
        }
        *self.callbacks.lock().unwrap() = Some(callbacks);
        Ok("done".into())
    }

    async fn close(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}

fn thread_options(thread: Arc<dyn Thread>) -> SessionOptions {
    let mut opts = options(Arc::new(StubProvider::new(vec![])));
    opts.thread = Some(thread);
    opts
}

fn runner() -> Runner {
    Runner {
        executor: stub_executor(),
    }
}

#[tokio::test]
async fn delegated_thread_honors_the_token_budget() {
    let runs = Arc::new(AtomicUsize::new(0));
    let thread = Arc::new(ScriptedThread {
        runs: runs.clone(),
        events: vec![token_count(4000, 1000, 5000)],
        text: "done".into(),
    });
    let events = Events::new();
    let mut opts = thread_options(thread);
    opts.max_total_tokens = 100;
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = runner();

    runner
        .run_locked(run_ctx(), &mut l, "first", &hooks)
        .await
        .unwrap();
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(l.total_usage.total, 5000);

    let err = runner
        .run_locked(run_ctx(), &mut l, "second", &hooks)
        .await
        .unwrap_err();
    assert!(err.message.contains("max_total_tokens"), "{}", err.message);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn delegated_thread_honors_the_history_budget() {
    let runs = Arc::new(AtomicUsize::new(0));
    let thread = Arc::new(ScriptedThread {
        runs: runs.clone(),
        events: vec![],
        text: "done".into(),
    });
    let events = Events::new();
    let mut opts = thread_options(thread);
    opts.max_history_bytes = 10;
    let mut l = lease(opts).await;
    l.history_bytes = 11;
    let hooks = hooks(&events);
    let runner = runner();

    let err = runner
        .run_locked(run_ctx(), &mut l, "prompt", &hooks)
        .await
        .unwrap_err();
    assert!(err.message.contains("max_history_bytes"), "{}", err.message);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn delegated_thread_names_the_run_deadline() {
    let events = Events::new();
    let mut opts = thread_options(Arc::new(DeadlineThread));
    opts.run_deadline = Some(Duration::from_millis(20));
    let mut l = lease(opts).await;
    let hooks = hooks(&events);
    let runner = runner();

    let err = runner
        .run_locked(run_ctx(), &mut l, "prompt", &hooks)
        .await
        .unwrap_err();
    assert!(err.message.contains("run deadline"), "{}", err.message);
}

#[tokio::test]
async fn delegated_thread_usage_reaches_the_rollout() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::new(tmp.path());

    let runs = Arc::new(AtomicUsize::new(0));
    let thread = Arc::new(ScriptedThread {
        runs: runs.clone(),
        events: vec![token_count(400, 100, 500)],
        text: "done".into(),
    });
    let events = Events::new();
    let mut l = lease(thread_options(thread)).await;
    let recorder =
        RolloutRecorder::create("thread-usage", Path::new(""), "", "codex", "m", "high").unwrap();
    l.rollout = Some(recorder);
    let hooks = hooks(&events);
    let runner = runner();

    runner
        .run_locked(run_ctx(), &mut l, "prompt", &hooks)
        .await
        .unwrap();
    assert_eq!(l.total_usage.total, 500);

    let lines = rollout_lines(tmp.path());
    let counts = rollout_event(&lines, "token_count");
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0]["info"]["last_token_usage"]["total_tokens"], 500);
    assert_eq!(counts[0]["info"]["total_token_usage"]["total_tokens"], 500);
}

#[tokio::test]
async fn delegated_thread_maps_a_wrapped_context_error() {
    let events = Events::new();
    let mut l = lease(thread_options(Arc::new(FailingThread {
        message: "turn/start: context canceled",
    })))
    .await;
    let ctx = run_ctx();
    ctx.cancel.cancel();
    let hooks = hooks(&events);
    let runner = runner();

    let err = runner
        .run_locked(ctx, &mut l, "prompt", &hooks)
        .await
        .unwrap_err();
    assert_eq!(err.message, "context canceled");
}

#[tokio::test]
async fn delegated_thread_preserves_a_real_error_after_cancellation() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::new(tmp.path());

    let events = Events::new();
    let mut l = lease(thread_options(Arc::new(FailingThread {
        message: "codex thread closed; start a new session",
    })))
    .await;
    let recorder =
        RolloutRecorder::create("thread-error", Path::new(""), "", "codex", "m", "high").unwrap();
    l.rollout = Some(recorder);
    let ctx = run_ctx();
    ctx.cancel.cancel();
    let hooks = hooks(&events);
    let runner = runner();

    let err = runner
        .run_locked(ctx, &mut l, "prompt", &hooks)
        .await
        .unwrap_err();
    assert_eq!(err.message, "codex thread closed; start a new session");

    // Go's delegated error path does not record a rollout error line.
    let lines = rollout_lines(tmp.path());
    assert!(
        rollout_event(&lines, "error").is_empty(),
        "rollout = {lines:?}"
    );
}

#[tokio::test]
async fn delegated_thread_records_usage_as_events_arrive() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::new(tmp.path());

    let mut files = vec![];
    let recorder =
        RolloutRecorder::create("thread-mid", Path::new(""), "", "codex", "m", "high").unwrap();
    collect_jsonl(tmp.path(), &mut files);
    assert_eq!(files.len(), 1, "rollout file is created up front");
    let seen = Arc::new(Mutex::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let thread = Arc::new(MidRunRolloutThread {
        runs: runs.clone(),
        rollout_path: files[0].clone(),
        seen: seen.clone(),
    });
    let events = Events::new();
    let mut l = lease(thread_options(thread)).await;
    l.rollout = Some(recorder);
    let hooks = hooks(&events);
    let runner = runner();

    runner
        .run_locked(run_ctx(), &mut l, "prompt", &hooks)
        .await
        .unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        1,
        "usage must be recorded while the thread runs, not after it returns"
    );
    let lines = rollout_lines(tmp.path());
    let counts = rollout_event(&lines, "token_count");
    assert_eq!(counts.len(), 2);
    assert_eq!(counts[0]["info"]["total_token_usage"]["total_tokens"], 10);
    assert_eq!(counts[1]["info"]["total_token_usage"]["total_tokens"], 15);
    assert_eq!(l.total_usage.total, 15);
}

#[tokio::test]
async fn delegated_thread_accounts_usage_emitted_after_the_run_returns() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = EnvGuard::new(tmp.path());

    let callbacks = Arc::new(Mutex::new(None));
    let runs = Arc::new(AtomicUsize::new(0));
    let thread = Arc::new(RetainingThread {
        runs: runs.clone(),
        callbacks: callbacks.clone(),
        events: vec![token_count(10, 0, 10)],
    });
    let events = Events::new();
    let mut opts = thread_options(thread);
    opts.max_total_tokens = 12;
    let mut l = lease(opts).await;
    let recorder =
        RolloutRecorder::create("thread-late", Path::new(""), "", "codex", "m", "high").unwrap();
    l.rollout = Some(recorder);
    let hooks = hooks(&events);
    let runner = runner();

    runner
        .run_locked(run_ctx(), &mut l, "first", &hooks)
        .await
        .unwrap();
    assert_eq!(l.total_usage.total, 10);

    // A background provider task reports usage after the turn returned.
    let retained = callbacks
        .lock()
        .unwrap()
        .clone()
        .expect("the thread keeps its callbacks");
    (retained.emit)("thread", token_count(5, 0, 5));

    // The next budget check sees the late event, as Go's session mutation does.
    let err = runner
        .run_locked(run_ctx(), &mut l, "second", &hooks)
        .await
        .unwrap_err();
    assert!(err.message.contains("max_total_tokens"), "{}", err.message);
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    let lines = rollout_lines(tmp.path());
    let counts = rollout_event(&lines, "token_count");
    assert_eq!(counts.len(), 2, "late usage is still recorded: {lines:?}");
    assert_eq!(counts[1]["info"]["last_token_usage"]["total_tokens"], 5);
    assert_eq!(counts[1]["info"]["total_token_usage"]["total_tokens"], 15);
}
