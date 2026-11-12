//! Provider-agnostic turn loop (Task 23A): the port of Go
//! `internal/agent/runner.go`'s `RunLocked` built-in loop and its helpers.
//!
//! The runner owns only an [`ToolExecutor`]; every progress event and approval
//! comes from the per-call [`RunHooks`], never from state stored on the
//! [`Runner`]. All session mutation happens through `SessionLease`'s
//! `DerefMut`; there is no direct path to the session-state lock.
//!
//! Each provider turn, tool call and approval wait receives the run context or
//! a child of it. A guarded select around the provider turn makes the run
//! deadline (and explicit cancellation) terminate a blocked model call, so a
//! phase that ignores the context cannot outlast the configured deadline.
//!
//! Tool bodies and policy are NOT implemented here: every tool call is handed
//! to [`ToolExecutor::execute`] and only its [`ToolResult`] is recorded.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use crate::agent::session::{AgentError, Emitter, SessionLease};
use crate::policy::{ApprovalPolicy, Sandbox};
use crate::provider::{
    DeltaSink, Message, ProviderError, Role, ThreadCallbacks, ToolCall, ToolSpec, TurnRequest,
    TurnResult, Usage,
};
use crate::util::approval::{ApprovalRequest, Approver};
use crate::util::duration::format_go_duration;
use crate::util::run::RunContext;

/// Go `continuePrompt`: answers a reply that has no tool call when nudging is
/// enabled, so a narration-only reply does not end an unfinished run.
pub const CONTINUE_PROMPT: &str = "You replied without a tool call, which ends the run. \
If any work remains, continue now by making the tool call. \
If the task is completely done, repeat your final summary.";

/// Go `continueAfterEmptyPrompt`: answers a reply with neither text nor a tool
/// call, asking for a smaller/different next tool call.
pub const CONTINUE_AFTER_EMPTY_PROMPT: &str = "Your last reply was empty: it had no text and no tool call, \
so any tool call you made may not have been delivered. \
If work remains, make the next tool call now, and keep it small and simple \
(for example split a large file write into several smaller calls). \
If the task is completely done, reply with your final summary.";

/// Go `cancelledToolResult`: the tool result recorded for calls skipped after
/// the context was cancelled, so every tool call keeps a matching result.
pub const CANCELLED_TOOL_RESULT: &str = "cancelled: not executed";

/// Go `deltaFlushInterval` / `deltaFlushBytes`.
pub const DELTA_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
pub const DELTA_FLUSH_BYTES: usize = 8 << 10;

/// Placeholder for an empty tool output, so the tool message is never empty
/// (several providers reject an assistant/tool history with empty content).
const EMPTY_TOOL_OUTPUT: &str = "(empty output)";

/// The result of executing one tool call, delegated to [`ToolExecutor`].
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

/// Per-call progress and approval sinks. These are passed into every
/// [`Runner::run_locked`] call and are deliberately NOT stored on [`Runner`]:
/// two concurrent runs must be able to use different sinks.
pub struct RunHooks {
    pub emitter: Arc<dyn Emitter>,
    pub approver: Arc<dyn Approver>,
}

/// The tool surface the runner delegates to (Task 23B owns the real executor).
/// `turn_id` is the per-run id generated in `run_inner`; the executor uses it
/// for `patch_apply_end` rollout records (Go `s.turnID`).
#[async_trait::async_trait]
pub trait ToolExecutor: Send + Sync {
    fn tools(&self) -> Vec<ToolSpec>;
    async fn execute(
        &self,
        ctx: &RunContext,
        lease: &mut SessionLease,
        call: &ToolCall,
        hooks: &RunHooks,
        turn_id: &str,
    ) -> ToolResult;
}

pub struct Runner {
    pub executor: Arc<dyn ToolExecutor>,
}

/// Go `deltaCoalescer`: merges streamed text deltas into fewer events. It is
/// used from the provider's streaming callback and flushed by the caller that
/// owns it, so it emits synchronously and needs no background timer. Uses
/// `tokio::time::Instant` so paused-time tests can control the interval.
pub struct DeltaCoalescer {
    emitter: Arc<dyn Emitter>,
    thread_id: String,
    pending: String,
    last: Instant,
    pub interval: Duration,
    pub started: Instant,
    pub first_delta: Option<Instant>,
}

impl DeltaCoalescer {
    pub fn new(emitter: Arc<dyn Emitter>, thread_id: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            emitter,
            thread_id: thread_id.into(),
            pending: String::new(),
            last: now,
            interval: DELTA_FLUSH_INTERVAL,
            started: now,
            first_delta: None,
        }
    }

    pub fn add(&mut self, delta: &str) {
        if self.first_delta.is_none() {
            self.first_delta = Some(Instant::now());
        }
        self.pending.push_str(delta);
        if self.pending.len() >= DELTA_FLUSH_BYTES
            || Instant::now().saturating_duration_since(self.last) >= self.interval
        {
            self.flush();
        }
    }

    pub fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending);
        self.last = Instant::now();
        (self.emitter).emit(
            &self.thread_id,
            json!({"type": "agent_message_delta", "delta": text}),
        );
    }
}

/// Go `messageSize`: every part the next turn re-sends counts, including the
/// raw opaque replay bytes, the tool-call id, and each tool call's id/name/
/// arguments. The system prompt and tool schemas are fixed for the thread's
/// life, so they are not counted.
pub fn message_size(m: &Message) -> u64 {
    let mut size = m.text.len() as u64
        + m.opaque.as_ref().map(|o| o.get().len() as u64).unwrap_or(0)
        + m.tool_call_id
            .as_deref()
            .map(|s| s.len() as u64)
            .unwrap_or(0);
    for call in &m.tool_calls {
        size += call.id.len() as u64 + call.name.len() as u64 + call.arguments.len() as u64;
    }
    size
}

/// Go `appendMessage`: append one history entry and keep the byte counter in
/// step. All state mutation flows through the lease's `DerefMut`.
pub fn append_message(lease: &mut SessionLease, msg: Message) {
    lease.history_bytes += message_size(&msg);
    lease.messages.push(msg);
}

/// Go `truncateMessages`: drop entries from `n` onwards and roll the byte
/// counter back with them, so a rolled-back prompt cannot count against a
/// later turn.
pub fn truncate_messages(lease: &mut SessionLease, n: usize) {
    let removed: u64 = lease.messages[n..].iter().map(message_size).sum();
    lease.history_bytes -= removed;
    lease.messages.truncate(n);
}

/// Go `budgetError`: report the first spent ceiling before a turn costs
/// anything. History is refused when strictly over the limit; tokens are
/// refused at or over the limit. Exact Go error text is preserved.
pub fn budget_error(
    max_history_bytes: u64,
    history_bytes: u64,
    max_total_tokens: u64,
    total_tokens: i64,
) -> Option<String> {
    if max_history_bytes > 0 && history_bytes > max_history_bytes {
        return Some(format!(
            "thread history reached {} bytes (max_history_bytes {}); start a new thread",
            history_bytes, max_history_bytes
        ));
    }
    if max_total_tokens > 0 && total_tokens >= max_total_tokens as i64 {
        return Some(format!(
            "thread spent {} tokens (max_total_tokens {})",
            total_tokens, max_total_tokens
        ));
    }
    None
}

/// Go `shellWritableRoots`, expressed as the `writable_roots` argument of
/// `RolloutRecorder::record_turn_start`: `None` serializes as JSON `null`
/// (danger-full-access, or workspace-write with zero roots), `Some(&[])` as
/// `[]` (read-only), and `Some(roots)` as an array (workspace-write with roots).
pub fn rollout_writable_roots(sandbox: Sandbox, root_paths: &[PathBuf]) -> Option<Vec<PathBuf>> {
    match sandbox {
        Sandbox::ReadOnly => Some(Vec::new()),
        Sandbox::WorkspaceWrite => {
            if root_paths.is_empty() {
                None
            } else {
                Some(root_paths.to_vec())
            }
        }
        Sandbox::DangerFullAccess => None,
    }
}

fn sandbox_str(s: Sandbox) -> &'static str {
    match s {
        Sandbox::ReadOnly => "read-only",
        Sandbox::WorkspaceWrite => "workspace-write",
        Sandbox::DangerFullAccess => "danger-full-access",
    }
}

fn approval_str(a: ApprovalPolicy) -> &'static str {
    match a {
        ApprovalPolicy::Untrusted => "untrusted",
        ApprovalPolicy::OnRequest => "on-request",
        ApprovalPolicy::OnFailure => "on-failure",
        ApprovalPolicy::Never => "never",
    }
}

/// Combine the caller's absolute deadline with the session's configured
/// `run_deadline` duration (Go `context.WithTimeout` inherits an earlier
/// parent deadline, so the effective deadline is the earlier of the two).
fn combine_deadline(caller: Option<Instant>, run: Option<Duration>) -> Option<Instant> {
    let now = Instant::now();
    match (caller, run) {
        (Some(a), Some(d)) => Some(a.min(now + d)),
        (Some(a), None) => Some(a),
        (None, Some(d)) => Some(now + d),
        (None, None) => None,
    }
}

/// Go `runCause`: the most informative error for a finished run context. A
/// deadline that has elapsed names the full configured deadline; a plain
/// cancellation is reported as "context canceled".
fn stop_cause(ctx: &RunContext, deadline_text: Option<&str>) -> Option<String> {
    if ctx.deadline_reason().is_some() {
        return Some(deadline_text.unwrap_or("run deadline reached").to_string());
    }
    if ctx.cancel.is_cancelled() {
        return Some("context canceled".to_string());
    }
    None
}

/// Go `recordThreadEvent`'s numeric reader: a token_count event from a
/// delegated thread carries JSON numbers, never a typed [`Usage`].
fn int_field(event: &Value, key: &str) -> i64 {
    event.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// The delegated-thread event sink. Named to keep the closure type out of the
/// run path (the same shape as `ThreadCallbacks::emit`).
type ThreadEmit = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// Replace the provider's thread id on an approval with the session id, which
/// is what the MCP client knows as `threadId`. Go's provider approval type has
/// no thread id; the runner supplies `s.ID` at this boundary.
struct ThreadApprover {
    inner: Arc<dyn Approver>,
    thread_id: String,
}

#[async_trait::async_trait]
impl Approver for ThreadApprover {
    async fn approve(&self, ctx: &RunContext, mut req: ApprovalRequest) -> bool {
        req.thread_id.clone_from(&self.thread_id);
        self.inner.approve(ctx, req).await
    }
}

impl Runner {
    /// Go `RunLocked` for a session the caller already holds through
    /// `Manager.Acquire`. Sets `last_used` at the end of the run.
    pub async fn run_locked(
        &self,
        ctx: RunContext,
        lease: &mut SessionLease,
        prompt: &str,
        hooks: &RunHooks,
    ) -> Result<String, AgentError> {
        let result = self.run_inner(ctx, lease, prompt, hooks).await;
        lease.last_used = Instant::now();
        result
    }

    async fn run_inner(
        &self,
        ctx: RunContext,
        lease: &mut SessionLease,
        prompt: &str,
        hooks: &RunHooks,
    ) -> Result<String, AgentError> {
        // Gather owned configuration up front so we can freely mutate the
        // session state through the lease without holding a borrow of it.
        let opts = lease.options();
        let thread_id = lease.id().to_string();
        let provider = opts.provider.clone();
        let model = opts.model.clone();
        let effort_sent = opts.effort_sent.clone();
        let system = opts.system_prompt.clone();
        let max_turns = opts.max_turns;
        let max_nudges = opts.max_nudges;
        let max_history_bytes = opts.max_history_bytes;
        let max_total_tokens = opts.max_total_tokens;
        let run_deadline = opts.run_deadline;
        let cwd = opts.cwd.clone();
        let reasoning_effort = opts.reasoning_effort.clone();
        let sandbox = opts.sandbox;
        let approval = opts.approval;
        let thread = opts.thread.clone();
        let root_paths = lease.root_paths.clone();

        let RunContext {
            cancel,
            deadline: caller_deadline,
        } = ctx;
        let deadline_text: Option<String> =
            run_deadline.map(|d| format!("run deadline of {} reached", format_go_duration(d)));
        let deadline = combine_deadline(caller_deadline, run_deadline);
        let ctx = RunContext { cancel, deadline };

        if let Some(thread) = thread {
            // A delegated thread spends the same budget as the built-in loop,
            // so the check runs before the thread is handed the prompt. Both
            // ceilings are cumulative, which is why a thread that reaches one
            // stays stopped.
            if let Some(msg) = budget_error(
                max_history_bytes,
                lease.history_bytes,
                max_total_tokens,
                lease.total_usage.total,
            ) {
                hooks
                    .emitter
                    .emit(&thread_id, json!({"type": "error", "message": &msg}));
                if let Some(r) = &mut lease.rollout {
                    r.record_error(&msg);
                }
                return Err(AgentError { message: msg });
            }

            // A delegated thread reports usage through token_count events and
            // has no TurnResult to take it from. Collect them while it runs,
            // then account them (even on failure) so the token budget and any
            // attached rollout stay complete.
            let usage_events = Arc::new(Mutex::new(Vec::new()));
            let recorded = usage_events.clone();
            let emitter = hooks.emitter.clone();
            let emit_thread_id = thread_id.clone();
            let emit: ThreadEmit = Arc::new(move |_kind, event| {
                if event.get("type").and_then(Value::as_str) == Some("token_count") {
                    recorded.lock().unwrap().push(event.clone());
                }
                emitter.emit(&emit_thread_id, event);
            });
            let approver: Arc<dyn Approver> = Arc::new(ThreadApprover {
                inner: hooks.approver.clone(),
                thread_id: thread_id.clone(),
            });
            let outcome = thread
                .run(
                    ctx.clone(),
                    prompt.to_string(),
                    effort_sent.clone(),
                    ThreadCallbacks {
                        emit,
                        approve: approver,
                    },
                )
                .await;

            for event in usage_events.lock().unwrap().iter() {
                let last = Usage {
                    input: int_field(event, "prompt_tokens"),
                    cached: 0,
                    output: int_field(event, "completion_tokens"),
                    reasoning: 0,
                    total: int_field(event, "total_tokens"),
                };
                lease.total_usage.input += last.input;
                lease.total_usage.cached += last.cached;
                lease.total_usage.output += last.output;
                lease.total_usage.reasoning += last.reasoning;
                lease.total_usage.total += last.total;
                let total = lease.total_usage.clone();
                if let Some(r) = &mut lease.rollout {
                    r.record_thread_token_count(&last, &total);
                }
            }

            return match outcome {
                Ok(text) => Ok(text),
                Err(error) => {
                    let message = if ctx.cancel.is_cancelled() || ctx.deadline_reason().is_some() {
                        stop_cause(&ctx, deadline_text.as_deref()).unwrap_or(error.message)
                    } else {
                        error.message
                    };
                    hooks
                        .emitter
                        .emit(&thread_id, json!({"type": "error", "message": &message}));
                    if let Some(r) = &mut lease.rollout {
                        r.record_error(&message);
                    }
                    Err(AgentError { message })
                }
            };
        }

        hooks
            .emitter
            .emit(&thread_id, json!({"type": "task_started"}));

        let started = SystemTime::now();
        let turn_id = Uuid::new_v4().to_string();
        let roots = rollout_writable_roots(sandbox, &root_paths);
        if let Some(r) = &mut lease.rollout {
            r.record_turn_start(
                &turn_id,
                &cwd,
                prompt,
                approval_str(approval),
                sandbox_str(sandbox),
                roots.as_deref(),
                &model,
                &reasoning_effort,
                effort_sent.as_deref(),
            );
        }

        // `entry_messages` is captured BEFORE the prompt is appended (Go
        // line 186), so an unanswered prompt is rolled back on failure.
        let entry_messages = lease.messages.len();
        append_message(
            lease,
            Message {
                role: Role::User,
                text: prompt.to_string(),
                ..Default::default()
            },
        );

        let mut assistant_appended = false;
        let mut nudges = 0usize;
        let mut nudged_since_tool = false;
        let mut last_text = String::new();

        for _ in 0..max_turns {
            if let Some(msg) = budget_error(
                max_history_bytes,
                lease.history_bytes,
                max_total_tokens,
                lease.total_usage.total,
            ) {
                hooks
                    .emitter
                    .emit(&thread_id, json!({"type": "error", "message": &msg}));
                if let Some(r) = &mut lease.rollout {
                    r.record_error(&msg);
                }
                return Err(AgentError { message: msg });
            }

            let coalescer = Arc::new(Mutex::new(DeltaCoalescer::new(
                hooks.emitter.clone(),
                thread_id.clone(),
            )));
            let request_started = coalescer.lock().unwrap().started;

            let req = TurnRequest {
                model: model.clone(),
                effort: effort_sent.clone(),
                system: system.clone(),
                messages: lease.messages.clone(),
                tools: self.executor.tools(),
            };
            let sink = Arc::clone(&coalescer);
            let on_delta: DeltaSink = Box::new(move |delta| {
                sink.lock().unwrap().add(&delta);
            });

            // One guarded select covers the model request: a deadline or an
            // explicit cancellation terminates a phase that ignores the
            // context, and the resulting error names the cause (Go runCause).
            let turn_fut = provider.turn(ctx.child(), req, on_delta);
            let raw: Result<TurnResult, ProviderError> = if let Some(dl) = ctx.deadline {
                tokio::select! {
                    biased;
                    _ = ctx.cancel.cancelled() => Err(ProviderError {
                        message: "context canceled".into(),
                    }),
                    _ = tokio::time::sleep_until(dl) => Err(ProviderError {
                        message: deadline_text
                            .clone()
                            .unwrap_or_else(|| "run deadline reached".into()),
                    }),
                    r = turn_fut => r,
                }
            } else {
                tokio::select! {
                    biased;
                    _ = ctx.cancel.cancelled() => Err(ProviderError {
                        message: "context canceled".into(),
                    }),
                    r = turn_fut => r,
                }
            };

            // Tail flush: emit any buffered deltas BEFORE the turn's summary
            // events (provider_request, token_count, tool events, ...).
            coalescer.lock().unwrap().flush();
            let first_delta = coalescer.lock().unwrap().first_delta;
            let request_finished = Instant::now();
            let duration_ms = request_finished
                .saturating_duration_since(request_started)
                .as_millis() as i64;
            let ttft_ms = first_delta
                .map(|fd| fd.saturating_duration_since(request_started).as_millis() as i64);

            let err_msg = raw.as_ref().err().map(|e| e.message.clone());
            let upstream = match &raw {
                Ok(res) => res
                    .upstream
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(String::from),
                Err(_) => None,
            };
            let mut request_event = json!({
                "type": "provider_request",
                "provider": provider.name(),
                "model": &model,
                "duration_ms": duration_ms,
            });
            if let Some(ttft) = ttft_ms {
                request_event["ttft_ms"] = json!(ttft);
            }
            if let Some(e) = &err_msg {
                request_event["error"] = json!(e);
            } else if let Some(u) = &upstream {
                request_event["upstream"] = json!(u);
            }

            if let Some(r) = &mut lease.rollout {
                r.record_provider_request(
                    provider.name(),
                    &model,
                    duration_ms,
                    ttft_ms,
                    upstream.as_deref(),
                    err_msg.as_deref(),
                );
            }
            hooks.emitter.emit(&thread_id, request_event);

            let res = match raw {
                Err(e) => {
                    if !assistant_appended {
                        truncate_messages(lease, entry_messages);
                    }
                    hooks
                        .emitter
                        .emit(&thread_id, json!({"type": "error", "message": &e.message}));
                    if let Some(r) = &mut lease.rollout {
                        r.record_error(&e.message);
                    }
                    return Err(AgentError { message: e.message });
                }
                Ok(res) => res,
            };

            // Client token_count uses the last turn's usage; cumulative usage
            // is what the token budget and the rollout token_count record use.
            if let Some(usage) = &res.usage {
                hooks.emitter.emit(
                    &thread_id,
                    json!({
                        "type": "token_count",
                        "prompt_tokens": usage.input,
                        "completion_tokens": usage.output,
                        "total_tokens": usage.total,
                    }),
                );
                lease.total_usage.input += usage.input;
                lease.total_usage.cached += usage.cached;
                lease.total_usage.output += usage.output;
                lease.total_usage.reasoning += usage.reasoning;
                lease.total_usage.total += usage.total;
            }
            let total_snapshot = lease.total_usage.clone();
            if let Some(r) = &mut lease.rollout {
                r.record_model_turn(
                    &turn_id,
                    &res.text,
                    res.reasoning.as_deref(),
                    res.usage.as_ref(),
                    &total_snapshot,
                );
            }

            // An empty reply (no text, no tool call) is not replayed as an
            // empty assistant message: several APIs reject such an entry.
            let empty_reply = res.tool_calls.is_empty() && res.text.is_empty();
            if !empty_reply {
                append_message(
                    lease,
                    Message {
                        role: Role::Assistant,
                        text: res.text.clone(),
                        tool_calls: res.tool_calls.clone(),
                        tool_call_id: None,
                        is_error: false,
                        opaque: res.opaque.clone(),
                    },
                );
            }

            if res.tool_calls.is_empty() {
                if !res.text.is_empty() {
                    last_text = res.text.clone();
                }
                if nudges < max_nudges && (!nudged_since_tool || res.text.is_empty()) {
                    nudges += 1;
                    nudged_since_tool = true;
                    assistant_appended = true;
                    let nudge = if res.text.is_empty() {
                        CONTINUE_AFTER_EMPTY_PROMPT
                    } else {
                        CONTINUE_PROMPT
                    };
                    append_message(
                        lease,
                        Message {
                            role: Role::User,
                            text: nudge.to_string(),
                            ..Default::default()
                        },
                    );
                    hooks
                        .emitter
                        .emit(&thread_id, json!({"type": "agent_nudge", "message": nudge}));
                    continue;
                }
                let text = if res.text.is_empty() {
                    last_text.clone()
                } else {
                    res.text.clone()
                };
                hooks.emitter.emit(
                    &thread_id,
                    json!({"type": "agent_message", "message": &text}),
                );
                hooks
                    .emitter
                    .emit(&thread_id, json!({"type": "task_complete"}));
                if let Some(r) = &mut lease.rollout {
                    r.record_task_complete(&turn_id, &text, started, SystemTime::now());
                }
                return Ok(text);
            }

            assistant_appended = true;
            nudged_since_tool = false;

            let calls = res.tool_calls.clone();
            for call in &calls {
                if stop_cause(&ctx, deadline_text.as_deref()).is_some() {
                    if let Some(r) = &mut lease.rollout {
                        r.record_tool_call(&turn_id, call, &call.name);
                        r.record_tool_output(&turn_id, call, CANCELLED_TOOL_RESULT, true);
                    }
                    append_message(
                        lease,
                        Message {
                            role: Role::Tool,
                            tool_call_id: Some(call.id.clone()),
                            text: CANCELLED_TOOL_RESULT.into(),
                            is_error: true,
                            ..Default::default()
                        },
                    );
                    continue;
                }
                if let Some(r) = &mut lease.rollout {
                    r.record_tool_call(&turn_id, call, &call.name);
                }
                let child = ctx.child();
                let result = self
                    .executor
                    .execute(&child, lease, call, hooks, &turn_id)
                    .await;
                let mut content = result.output;
                if content.is_empty() {
                    content = EMPTY_TOOL_OUTPUT.into();
                }
                if let Some(r) = &mut lease.rollout {
                    r.record_tool_output(&turn_id, call, &content, result.is_error);
                }
                append_message(
                    lease,
                    Message {
                        role: Role::Tool,
                        tool_call_id: Some(call.id.clone()),
                        text: content,
                        is_error: result.is_error,
                        ..Default::default()
                    },
                );
            }
            if let Some(cause) = stop_cause(&ctx, deadline_text.as_deref()) {
                hooks
                    .emitter
                    .emit(&thread_id, json!({"type": "error", "message": &cause}));
                if let Some(r) = &mut lease.rollout {
                    r.record_error(&cause);
                }
                return Err(AgentError { message: cause });
            }
        }

        let msg = format!("turn limit reached ({}) without a final answer", max_turns);
        hooks
            .emitter
            .emit(&thread_id, json!({"type": "error", "message": &msg}));
        if let Some(r) = &mut lease.rollout {
            r.record_error(&msg);
        }
        Err(AgentError { message: msg })
    }
}
