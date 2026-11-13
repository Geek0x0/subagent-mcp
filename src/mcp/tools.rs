//! MCP tool handlers for `subagent` and `subagent-reply` (Task 24A).
//!
//! Ports Go `internal/server/server.go` (v0.13.1) for the non-Codex providers:
//! the exact tool declarations, provider/model/effort/cwd/budget resolution,
//! session creation with a rollout recorder and `session_meta`, the per-call
//! progress/approval hooks, and the structured result/error shapes.
//!
//! [`Runner`] owns only the shared executor, so one `Arc<Runner>` serves every
//! call; each call builds its own [`RunHooks`] from its [`CallContext`], which
//! keeps progress and approvals per-call. Codex whole-thread sessions are
//! integrated here: the adapter is registered with the factory and reached
//! through an optional-capability downcast, the HTTP system prompt is skipped,
//! and Codex owns its own rollout.

use std::any::Any;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::agent::executor::BuiltinToolExecutor;
use crate::agent::runner::{RunHooks, Runner};
use crate::agent::session::{Emitter, SessionError, SessionManager, SessionOptions};
use crate::config::{Api, Config, EFFORT_VALUES, MAX_NUDGES_LIMIT, ProviderConfig};
use crate::mcp::auto::{AutoSelection, resolve_auto};
use crate::mcp::progress::ProgressSink;
use crate::mcp::server::{CallContext, ToolDefinition, ToolError, ToolHandler};
use crate::policy::{ApprovalPolicy, Sandbox};
use crate::provider::codex::CodexAdapter;
use crate::provider::{
    Agent, ChatCompletions, Messages, Provider, Responses, Thread, ThreadOptions, factory,
};
use crate::repo;
use crate::rollout::RolloutRecorder;
use crate::util::duration::format_go_duration;
use crate::util::fs::eval_symlinks;
use crate::util::json::go_quote;
use crate::util::run::RunContext;
use crate::util::time::rfc3339_nano;

/// Go `maxConfiguredTurns`: a larger `max_turns` is ignored, not clamped.
const MAX_CONFIGURED_TURNS: i64 = 100_000;
/// Go `agent.DefaultMaxHistoryBytes` (64 MiB).
const DEFAULT_MAX_HISTORY_BYTES: u64 = 64 << 20;
/// Go `maxHistoryBytesLimit` (1 TiB).
const MAX_HISTORY_BYTES_LIMIT: u64 = 1 << 40;
/// Go `maxTotalTokensLimit`.
const MAX_TOTAL_TOKENS_LIMIT: u64 = 1_000_000_000;
/// Go `maxRunDeadlineSeconds` (7 days).
const MAX_RUN_DEADLINE_SECONDS: u64 = 7 * 24 * 60 * 60;

const START_DESCRIPTION: &str = "Start a new subagent coding-agent thread.";
const REPLY_DESCRIPTION: &str = "Continue an existing subagent coding-agent thread.";
const PROMPT_DESCRIPTION: &str = "Task prompt to send to the new subagent thread.";
const REPLY_PROMPT_DESCRIPTION: &str = "Follow-up prompt to send to the existing subagent thread.";
const THREAD_ID_DESCRIPTION: &str = "ID of the subagent thread that produced this result.";
const CONTENT_DESCRIPTION: &str = "Human-readable report text from the subagent.";
const PROVIDER_OUTPUT_DESCRIPTION: &str = "Configured provider the session uses.";
const MODEL_OUTPUT_DESCRIPTION: &str = "Model id the session uses.";
const AUTO_DESCRIPTION: &str =
    "Auto selection details, present when the model was selected by auto mode.";
const CONFIG_DESCRIPTION: &str = "loose config map; recognized keys: max_turns (number from 1 to 100000), model_reasoning_effort (same values as reasoning-effort; the top-level argument wins), writable_roots (array of absolute directory paths the shell may also write under workspace-write), max_nudges (integer 0 to 10; how many times a reply without a tool call is answered with a continue prompt, overriding the provider default), max_history_bytes (integer ceiling on the history the thread replays; default 67108864, 0 disables), max_total_tokens (integer ceiling on the tokens the thread reports across all turns; default 0, off), run_deadline_seconds (integer wall-clock ceiling on one call; default 0, off); an unusable budget value is an error, other unknown keys are silently ignored";

/// Whether this build can run Codex whole-thread sessions. The adapter is
/// registered by [`register_adapters`]; a test dispatcher may own the slot, in
/// which case the registered constructor is still the availability signal.
pub fn codex_available() -> bool {
    factory::lookup(Api::CodexAppServer.as_str()).is_some()
}

/// Register the adapter constructors once. The factory's `Constructor` takes a
/// resolved API key, while each adapter resolves its own key from
/// `ProviderConfig`/env; the third argument is therefore ignored deliberately.
fn register_adapters() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        register_if_absent(
            Api::ChatCompletions.as_str(),
            Arc::new(|name, cfg, _key| {
                Ok(Arc::new(ChatCompletions::new(name, cfg)?) as Arc<dyn Provider>)
            }),
        );
        register_if_absent(
            Api::Responses.as_str(),
            Arc::new(|name, cfg, _key| {
                Ok(Arc::new(Responses::new(name, cfg)?) as Arc<dyn Provider>)
            }),
        );
        register_if_absent(
            Api::Messages.as_str(),
            Arc::new(
                |name, cfg, _key| Ok(Arc::new(Messages::new(name, cfg)?) as Arc<dyn Provider>),
            ),
        );
        register_if_absent(
            Api::CodexAppServer.as_str(),
            Arc::new(|name, cfg, _key| {
                Ok(Arc::new(CodexAdapter::new(name, cfg)?) as Arc<dyn Provider>)
            }),
        );
    });
}

/// The optional `Agent` capability of a resolved provider. The design keeps
/// `Agent` out of the provider god trait and uses a downcast; Codex is the only
/// adapter that implements it.
///
/// Rust has no `dyn Any` -> `dyn Trait` downcast, so the capability is reached
/// through the concrete adapter that implements it and returned as the
/// `dyn Agent` trait object. [`ensure_agent_capable`] turns a provider that
/// claims `codex-app-server` without that capability into a clear error instead
/// of silently running the HTTP turn loop.
fn codex_agent(adapter: &Arc<dyn Provider>) -> Option<&dyn Agent> {
    (adapter.as_ref() as &dyn Any)
        .downcast_ref::<CodexAdapter>()
        .map(|codex| codex as &dyn Agent)
}

/// Reject a `codex-app-server` provider whose adapter cannot start threads.
///
/// Returns whether the adapter exposes the `Agent` capability. Non-Codex APIs
/// are never required to be agents.
fn ensure_agent_capable(
    name: &str,
    cfg: &ProviderConfig,
    adapter: &Arc<dyn Provider>,
) -> Result<bool, String> {
    let is_agent = codex_agent(adapter).is_some();
    if cfg.api == Api::CodexAppServer && !is_agent {
        return Err(format!(
            "provider {} uses codex-app-server but its adapter does not implement the Agent capability",
            go_quote(name)
        ));
    }
    Ok(is_agent)
}

/// Register a constructor unless a caller (or a test dispatcher) already owns
/// the API slot. Production has no prior registration, so all four register.
fn register_if_absent(api: &str, constructor: factory::Constructor) {
    if factory::lookup(api).is_none() {
        factory::register(api, constructor);
    }
}

/// The `subagent` / `subagent-reply` tool table.
pub struct SubagentTools {
    tool_name: String,
    config: Arc<Config>,
    manager: Arc<SessionManager>,
    runner: Arc<Runner>,
}

impl SubagentTools {
    /// Build the handlers for `tool_name` (`subagent` unless
    /// `SUBAGENT_MCP_TOOL_NAME` renamed it). The reply tool is
    /// `<tool_name>-reply`.
    pub fn new(tool_name: String, config: Arc<Config>, manager: Arc<SessionManager>) -> Self {
        register_adapters();
        Self {
            tool_name,
            config,
            manager,
            runner: Arc::new(Runner {
                executor: Arc::new(BuiltinToolExecutor::default()),
            }),
        }
    }

    fn reply_name(&self) -> String {
        format!("{}-reply", self.tool_name)
    }

    async fn handle_start(&self, args: Value, ctx: CallContext) -> Result<Value, ToolError> {
        let CallContext {
            cancel,
            progress,
            approver,
            ..
        } = ctx;
        let args = args.as_object().cloned().unwrap_or_default();

        let prompt = match require_string(&args, "prompt") {
            Ok(prompt) => prompt,
            Err(error) => return fail(format!("prompt is required: {error}")),
        };
        if prompt.trim().is_empty() {
            return fail("prompt must not be empty or whitespace-only");
        }

        let auto_requested = matches!(
            args.get("model"),
            Some(Value::String(model)) if model == "auto"
        );

        let mut provider_name = String::new();
        let mut provider_cfg: Option<ProviderConfig> = None;
        let mut adapter: Option<Arc<dyn Provider>> = None;
        let mut model = String::new();
        let mut auto_selection: Option<AutoSelection> = None;

        if auto_requested {
            if self.config.auto.is_none() {
                return fail(format!(
                    "model \"auto\" requires an [auto] section in {}",
                    self.config.path.display()
                ));
            }
            if let Some(raw) = args.get("provider") {
                let value = match raw {
                    Value::String(value) => value,
                    _ => return fail("argument \"provider\" must be a string"),
                };
                if !value.is_empty() {
                    return fail("provider must be omitted when model is \"auto\"");
                }
            }
        } else {
            match self.resolve_provider(&args) {
                Ok(resolved) => {
                    provider_name = resolved.name;
                    provider_cfg = Some(resolved.cfg);
                    adapter = Some(resolved.adapter);
                }
                Err(ResolveError::Tool(message)) => return fail(message),
            }
        }

        let sandbox_value = args
            .get("sandbox")
            .and_then(Value::as_str)
            .unwrap_or("read-only");
        let sandbox = match parse_sandbox(sandbox_value) {
            Some(sandbox) => sandbox,
            None => {
                return fail(format!(
                    "invalid sandbox {}; valid values: read-only, workspace-write, danger-full-access",
                    go_quote(sandbox_value)
                ));
            }
        };

        let approval_value = args
            .get("approval-policy")
            .and_then(Value::as_str)
            .unwrap_or("on-request");
        let approval = match parse_approval(approval_value) {
            Some(approval) => approval,
            None => {
                return fail(format!(
                    "invalid approval-policy {}; valid values: untrusted, on-request, on-failure, never",
                    go_quote(approval_value)
                ));
            }
        };

        if !auto_requested {
            model = match args.get("model") {
                None => String::new(),
                Some(Value::String(model)) => model.clone(),
                Some(_) => return fail("argument \"model\" must be a string"),
            };
            let cfg = provider_cfg.as_ref().expect("provider resolved");
            if model.is_empty() {
                model = cfg.default_model.clone();
            } else if !has_model(cfg, &model) {
                return fail(format!(
                    "unknown model {}; available models: {}",
                    go_quote(&model),
                    model_ids(cfg).join(", ")
                ));
            }
        }

        let loose_config = args
            .get("config")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let requested = match resolve_effort(&args, &loose_config) {
            Ok(requested) => requested,
            Err(message) => return fail(message),
        };

        let cwd = match args.get("cwd") {
            None => None,
            Some(Value::String(cwd)) => Some(cwd.clone()),
            Some(_) => return fail("argument \"cwd\" must be a string"),
        };
        let cwd = match cwd {
            Some(cwd) if !cwd.is_empty() => PathBuf::from(cwd),
            _ => match std::env::current_dir() {
                Ok(cwd) => cwd,
                Err(error) => return fail(format!("resolve default cwd: {error}")),
            },
        };
        if !cwd.is_absolute() {
            return fail(format!(
                "cwd must be an absolute path: {}",
                go_quote(&cwd.to_string_lossy())
            ));
        }
        match std::fs::metadata(&cwd) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return fail(format!(
                    "cwd does not exist: {}",
                    go_quote(&cwd.to_string_lossy())
                ));
            }
            Err(error) => {
                return fail(format!(
                    "inspect cwd {}: {error}",
                    go_quote(&cwd.to_string_lossy())
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return fail(format!(
                    "cwd is not a directory: {}",
                    go_quote(&cwd.to_string_lossy())
                ));
            }
            Ok(_) => {}
        }

        let mut writable_roots = Vec::new();
        let mut budgets = RunBudgets::default();
        if auto_requested {
            writable_roots = match parse_writable_roots(&loose_config) {
                Ok(roots) => roots,
                Err(message) => return fail(message),
            };
            budgets = match parse_budgets(&loose_config) {
                Ok(budgets) => budgets,
                Err(message) => return fail(message),
            };
        }

        // For auto selection the run deadline starts before the decision
        // request so it is bounded by the same wall-clock ceiling. An explicit
        // start lets the runner apply the session's run_deadline from the
        // first turn, matching Go's `WithTimeoutCause` scope.
        let deadline = if auto_requested && budgets.run_deadline > Duration::ZERO {
            Some(tokio::time::Instant::now() + budgets.run_deadline)
        } else {
            None
        };
        let run_ctx = RunContext {
            cancel: cancel.clone(),
            deadline,
        };

        if auto_requested {
            let selection =
                match resolve_auto(&run_ctx, &self.config, &self.manager, &prompt, &cwd).await {
                    Ok(selection) => selection,
                    Err(error) => {
                        // Go's handler reports the run cause (deadline/cancel)
                        // rather than the decision client's own timeout text.
                        if run_ctx.cancel.is_cancelled() {
                            return fail("auto model selection: context canceled");
                        }
                        if run_ctx.deadline_reason().is_some() {
                            return fail(format!(
                                "auto model selection: run deadline of {} reached",
                                format_go_duration(budgets.run_deadline)
                            ));
                        }
                        return fail(error.message);
                    }
                };
            provider_name = selection.provider.clone();
            model = selection.model.clone();
            let cfg = self
                .config
                .providers
                .get(&provider_name)
                .expect("resolve_auto validated the provider")
                .clone();
            let key = match api_key_for(&self.config, &provider_name) {
                Ok(key) => key,
                Err(message) => {
                    return fail(format!("auto model selection: {message}"));
                }
            };
            adapter = match factory::new(&provider_name, &cfg, &key) {
                Ok(adapter) => Some(adapter),
                Err(error) => {
                    return fail(format!("auto model selection: {}", error.message));
                }
            };
            provider_cfg = Some(cfg);
            auto_selection = Some(selection);
            if run_ctx.cancel.is_cancelled() {
                return fail("auto model selection: context canceled");
            }
            if run_ctx.deadline_reason().is_some() {
                return fail(format!(
                    "auto model selection: run deadline of {} reached",
                    format_go_duration(budgets.run_deadline)
                ));
            }
        }

        let base_instructions = args
            .get("base-instructions")
            .and_then(Value::as_str)
            .map(str::to_string);
        let developer_instructions = args
            .get("developer-instructions")
            .and_then(Value::as_str)
            .map(str::to_string);

        let adapter = adapter.expect("provider resolved");
        let provider_cfg = provider_cfg.expect("provider resolved");
        let is_agent = match ensure_agent_capable(&provider_name, &provider_cfg, &adapter) {
            Ok(is_agent) => is_agent,
            Err(message) => return fail(message),
        };
        // Codex receives its instructions through ThreadOptions; building the
        // HTTP prompt (default + AGENTS.md + developer) here would both waste
        // work and make a bad AGENTS.md fail a Codex start.
        let system_prompt = if is_agent {
            String::new()
        } else {
            match crate::agent::prompt::system_prompt(
                base_instructions.as_deref(),
                developer_instructions.as_deref(),
                &cwd,
            ) {
                Ok(prompt) => prompt,
                Err(error) => return fail(error.message),
            }
        };

        let max_turns = loose_config
            .get("max_turns")
            .and_then(Value::as_f64)
            .filter(|value| {
                value.is_finite() && *value > 0.0 && *value <= MAX_CONFIGURED_TURNS as f64
            })
            .map(|value| value as usize)
            .unwrap_or(0);

        let mut max_nudges = provider_cfg.max_nudges.unwrap_or(0);
        if let Some(value) = loose_config
            .get("max_nudges")
            .and_then(integer_value)
            .filter(|value| *value >= 0 && *value <= MAX_NUDGES_LIMIT)
        {
            max_nudges = value as usize;
        }

        if !auto_requested {
            writable_roots = match parse_writable_roots(&loose_config) {
                Ok(roots) => roots,
                Err(message) => return fail(message),
            };
            budgets = match parse_budgets(&loose_config) {
                Ok(budgets) => budgets,
                Err(message) => return fail(message),
            };
        }

        // A Codex session delegates the whole thread: the adapter starts it
        // with the caller's instructions and owns persistence and rollout.
        // Go passes the call context here, not the run deadline, which only
        // bounds `RunLocked`.
        let thread_ctx = RunContext {
            cancel: run_ctx.cancel.clone(),
            deadline: None,
        };
        let thread: Option<Arc<dyn Thread>> = if let Some(agent) = codex_agent(&adapter) {
            let options = ThreadOptions {
                model: model.clone(),
                cwd: cwd.clone(),
                sandbox,
                approval_policy: approval,
                base_instructions: base_instructions.clone(),
                developer_instructions: developer_instructions.clone(),
                writable_roots: writable_roots.clone(),
                ephemeral: false,
            };
            match agent.start_thread(&thread_ctx, options).await {
                Ok(thread) => Some(Arc::from(thread)),
                Err(error) => return fail(error.message),
            }
        } else {
            None
        };

        let options = SessionOptions {
            provider: adapter,
            thread,
            model: model.clone(),
            reasoning_effort: requested.clone(),
            effort_sent: Some(map_effort(&provider_cfg, &requested)),
            cwd: cwd.clone(),
            sandbox,
            approval,
            system_prompt: system_prompt.clone(),
            max_turns,
            max_nudges,
            writable_roots,
            max_history_bytes: budgets.history_bytes,
            max_total_tokens: budgets.total_tokens,
            run_deadline: if budgets.run_deadline > Duration::ZERO {
                Some(budgets.run_deadline)
            } else {
                None
            },
        };

        let session = match self.manager.create(options).await {
            Ok(session) => session,
            Err(error) => return Err(ToolError::internal(error.message())),
        };
        let mut lease = match self.manager.acquire(&session.id).await {
            Ok(lease) => lease,
            Err(error) => return Err(ToolError::internal(error.message())),
        };

        // Codex owns its own rollout; only HTTP sessions get the server's
        // recorder and `session_meta`.
        let created = SystemTime::now();
        if !is_agent
            && let Ok(mut recorder) = RolloutRecorder::create(
                &session.id,
                &cwd,
                &system_prompt,
                &provider_name,
                &model,
                &requested,
            )
        {
            let meta = session_meta(
                &session.id,
                &cwd,
                &system_prompt,
                &provider_name,
                auto_selection.as_ref(),
                created,
            );
            recorder.write_line("session_meta", meta);
            lease.rollout = Some(recorder);
        }

        let hooks = RunHooks {
            emitter: Arc::new(McpEmitter { progress }),
            approver,
        };
        let outcome = self
            .runner
            .run_locked(run_ctx, &mut lease, &prompt, &hooks)
            .await;

        match outcome {
            Ok(text) => Ok(start_result(
                &session.id,
                &text,
                &provider_name,
                &model,
                auto_selection.as_ref(),
                false,
            )),
            Err(error) => Ok(start_result(
                &session.id,
                &format!("error: {}", error.message),
                &provider_name,
                &model,
                auto_selection.as_ref(),
                true,
            )),
        }
    }

    async fn handle_reply(&self, args: Value, ctx: CallContext) -> Result<Value, ToolError> {
        let CallContext {
            cancel,
            progress,
            approver,
            ..
        } = ctx;
        let args = args.as_object().cloned().unwrap_or_default();

        let thread_id = match require_string(&args, "threadId") {
            Ok(thread_id) => thread_id,
            Err(error) => return fail(format!("threadId is required: {error}")),
        };
        let prompt = match require_string(&args, "prompt") {
            Ok(prompt) => prompt,
            Err(error) => return fail(format!("prompt is required: {error}")),
        };
        if prompt.trim().is_empty() {
            return fail("prompt must not be empty or whitespace-only");
        }

        let mut lease = match self.manager.acquire(&thread_id).await {
            Ok(lease) => lease,
            Err(SessionError::UnknownSession) => {
                return fail(format!("unknown threadId: {thread_id}"));
            }
            Err(SessionError::Busy) => return Ok(busy_result(&thread_id)),
        };
        let provider_name = lease.options().provider.name().to_string();
        let model = lease.options().model.clone();

        let hooks = RunHooks {
            emitter: Arc::new(McpEmitter { progress }),
            approver,
        };
        // A reply derives a fresh context from this call only; it never reuses
        // the start call's cancellation or deadline. The runner applies the
        // session's configured run_deadline.
        let run_ctx = RunContext {
            cancel,
            deadline: None,
        };
        let outcome = self
            .runner
            .run_locked(run_ctx, &mut lease, &prompt, &hooks)
            .await;

        match outcome {
            Ok(text) => Ok(reply_result(
                &thread_id,
                &text,
                &provider_name,
                &model,
                false,
            )),
            Err(error) => Ok(reply_result(
                &thread_id,
                &format!("error: {}", error.message),
                &provider_name,
                &model,
                true,
            )),
        }
    }

    fn resolve_provider(
        &self,
        args: &Map<String, Value>,
    ) -> Result<ResolvedProvider, ResolveError> {
        let provider_arg = match args.get("provider") {
            None => String::new(),
            Some(Value::String(value)) => value.clone(),
            Some(_) => {
                return Err(ResolveError::Tool(
                    "argument \"provider\" must be a string".into(),
                ));
            }
        };

        let (name, cfg) = if !provider_arg.is_empty() {
            match self.config.providers.get(&provider_arg) {
                Some(cfg) => (provider_arg, cfg.clone()),
                None => {
                    return Err(ResolveError::Tool(format!(
                        "unknown provider {}; configured providers: {}",
                        go_quote(&provider_arg),
                        provider_names(&self.config).join(", ")
                    )));
                }
            }
        } else if self.config.providers.len() == 1 {
            let (name, cfg) = self.config.providers.iter().next().expect("one provider");
            (name.clone(), cfg.clone())
        } else {
            return Err(ResolveError::Tool(format!(
                "provider is required when more than one is configured; available: {}",
                provider_names(&self.config).join(", ")
            )));
        };

        let key = api_key_for(&self.config, &name).map_err(ResolveError::Tool)?;
        let adapter =
            factory::new(&name, &cfg, &key).map_err(|error| ResolveError::Tool(error.message))?;

        Ok(ResolvedProvider { name, cfg, adapter })
    }
}

#[async_trait]
impl ToolHandler for SubagentTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            start_definition(&self.tool_name, &self.config),
            reply_definition(&self.tool_name),
        ]
    }

    async fn call(&self, name: &str, args: Value, ctx: CallContext) -> Result<Value, ToolError> {
        if name == self.tool_name {
            self.handle_start(args, ctx).await
        } else if name == self.reply_name() {
            self.handle_reply(args, ctx).await
        } else {
            Err(ToolError::invalid_params(format!(
                "tool {} not found",
                go_quote(name)
            )))
        }
    }
}

/// Per-call progress sink. `CallContext.progress` is always `Some` for a
/// `tools/call`; the sink itself suppresses `notifications/progress` when the
/// client sent no token while still emitting `subagent/event`.
struct McpEmitter {
    progress: Option<ProgressSink>,
}

impl Emitter for McpEmitter {
    fn emit(&self, thread_id: &str, event: Value) {
        if let Some(progress) = &self.progress {
            progress.emit(thread_id, &event);
        }
    }
}

struct ResolvedProvider {
    name: String,
    cfg: ProviderConfig,
    adapter: Arc<dyn Provider>,
}

enum ResolveError {
    /// A user-facing tool error (Go's `NewToolResultError`).
    Tool(String),
}

struct RunBudgets {
    history_bytes: u64,
    total_tokens: u64,
    run_deadline: Duration,
}

impl Default for RunBudgets {
    fn default() -> Self {
        Self {
            history_bytes: DEFAULT_MAX_HISTORY_BYTES,
            total_tokens: 0,
            run_deadline: Duration::ZERO,
        }
    }
}

// ---------------------------------------------------------------------------
// Result shapes
// ---------------------------------------------------------------------------

fn tool_result(content: &str, structured: Option<Value>, is_error: bool) -> Value {
    let mut result = Map::new();
    result.insert("content".into(), json!([{"type": "text", "text": content}]));
    if let Some(structured) = structured {
        result.insert("structuredContent".into(), structured);
    }
    if is_error {
        result.insert("isError".into(), json!(true));
    }
    Value::Object(result)
}

fn tool_error_result(message: impl Into<String>) -> Value {
    tool_result(&message.into(), None, true)
}

fn fail(message: impl Into<String>) -> Result<Value, ToolError> {
    Ok(tool_error_result(message))
}

fn start_result(
    thread_id: &str,
    content: &str,
    provider: &str,
    model: &str,
    auto: Option<&AutoSelection>,
    is_error: bool,
) -> Value {
    let mut structured = Map::new();
    structured.insert("threadId".into(), json!(thread_id));
    structured.insert("content".into(), json!(content));
    if !provider.is_empty() {
        structured.insert("provider".into(), json!(provider));
    }
    if !model.is_empty() {
        structured.insert("model".into(), json!(model));
    }
    if let Some(auto) = auto {
        structured.insert("auto".into(), auto.meta());
    }
    tool_result(content, Some(Value::Object(structured)), is_error)
}

fn reply_result(
    thread_id: &str,
    content: &str,
    provider: &str,
    model: &str,
    is_error: bool,
) -> Value {
    let mut structured = Map::new();
    structured.insert("threadId".into(), json!(thread_id));
    structured.insert("content".into(), json!(content));
    if !provider.is_empty() {
        structured.insert("provider".into(), json!(provider));
    }
    if !model.is_empty() {
        structured.insert("model".into(), json!(model));
    }
    tool_result(content, Some(Value::Object(structured)), is_error)
}

fn busy_result(thread_id: &str) -> Value {
    let content = format!("error: thread {thread_id} is busy with another call");
    let mut structured = Map::new();
    structured.insert("threadId".into(), json!(thread_id));
    structured.insert("content".into(), json!(content));
    tool_result(&content, Some(Value::Object(structured)), true)
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

fn require_string(args: &Map<String, Value>, name: &str) -> Result<String, String> {
    match args.get(name) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("argument {} is not a string", go_quote(name))),
        None => Err(format!("required argument {} not found", go_quote(name))),
    }
}

fn parse_sandbox(value: &str) -> Option<Sandbox> {
    match value {
        "read-only" => Some(Sandbox::ReadOnly),
        "workspace-write" => Some(Sandbox::WorkspaceWrite),
        "danger-full-access" => Some(Sandbox::DangerFullAccess),
        _ => None,
    }
}

fn parse_approval(value: &str) -> Option<ApprovalPolicy> {
    match value {
        "untrusted" => Some(ApprovalPolicy::Untrusted),
        "on-request" => Some(ApprovalPolicy::OnRequest),
        "on-failure" => Some(ApprovalPolicy::OnFailure),
        "never" => Some(ApprovalPolicy::Never),
        _ => None,
    }
}

fn effort_values_text() -> String {
    EFFORT_VALUES.join(", ")
}

fn valid_effort(value: &str) -> bool {
    EFFORT_VALUES.contains(&value)
}

fn resolve_effort(
    args: &Map<String, Value>,
    loose_config: &Map<String, Value>,
) -> Result<String, String> {
    let mut requested = "high".to_string();
    if let Some(raw) = loose_config.get("model_reasoning_effort") {
        let value = raw.as_str().ok_or_else(|| {
            format!(
                "config.model_reasoning_effort must be a string; valid values: {}",
                effort_values_text()
            )
        })?;
        if !valid_effort(value) {
            return Err(format!(
                "invalid config.model_reasoning_effort {}; valid values: {}",
                go_quote(value),
                effort_values_text()
            ));
        }
        requested = value.to_string();
    }
    if let Some(raw) = args.get("reasoning-effort") {
        let value = raw.as_str().ok_or_else(|| {
            format!(
                "reasoning-effort must be a string; valid values: {}",
                effort_values_text()
            )
        })?;
        if !value.is_empty() {
            if !valid_effort(value) {
                return Err(format!(
                    "invalid reasoning-effort {}; valid values: {}",
                    go_quote(value),
                    effort_values_text()
                ));
            }
            requested = value.to_string();
        }
    }
    Ok(requested)
}

/// A whole JSON number. A fractional value is rejected rather than truncated,
/// matching Go's `value == float64(int(value))` checks.
fn integer_value(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    if let Some(value) = number.as_i64() {
        return Some(value);
    }
    let value = number.as_f64()?;
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        Some(value as i64)
    } else {
        None
    }
}

fn parse_budgets(loose_config: &Map<String, Value>) -> Result<RunBudgets, String> {
    let mut budgets = RunBudgets::default();

    if let Some(raw) = loose_config.get("max_history_bytes") {
        let value = integer_value(raw)
            .filter(|value| *value >= 0 && (*value as u64) <= MAX_HISTORY_BYTES_LIMIT);
        let Some(value) = value else {
            return Err(format!(
                "config.max_history_bytes must be an integer from 1 to {MAX_HISTORY_BYTES_LIMIT} (0 disables the limit)"
            ));
        };
        budgets.history_bytes = value as u64;
    }

    if let Some(raw) = loose_config.get("max_total_tokens") {
        let value = integer_value(raw)
            .filter(|value| *value >= 0 && (*value as u64) <= MAX_TOTAL_TOKENS_LIMIT);
        let Some(value) = value else {
            return Err(format!(
                "config.max_total_tokens must be an integer from 1 to {MAX_TOTAL_TOKENS_LIMIT} (0 disables the limit)"
            ));
        };
        budgets.total_tokens = value as u64;
    }

    if let Some(raw) = loose_config.get("run_deadline_seconds") {
        let value = integer_value(raw)
            .filter(|value| *value >= 0 && (*value as u64) <= MAX_RUN_DEADLINE_SECONDS);
        let Some(value) = value else {
            return Err(format!(
                "config.run_deadline_seconds must be an integer from 1 to {MAX_RUN_DEADLINE_SECONDS} (0 disables the deadline)"
            ));
        };
        budgets.run_deadline = Duration::from_secs(value as u64);
    }

    Ok(budgets)
}

fn parse_writable_roots(loose_config: &Map<String, Value>) -> Result<Vec<PathBuf>, String> {
    let Some(raw) = loose_config.get("writable_roots") else {
        return Ok(Vec::new());
    };
    let Some(entries) = raw.as_array() else {
        return Err("config.writable_roots must be an array of absolute directory paths".into());
    };
    let mut roots = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(path) = entry.as_str() else {
            return Err(format!(
                "config.writable_roots entry {} must be an absolute path",
                display_value(entry)
            ));
        };
        if !Path::new(path).is_absolute() {
            return Err(format!(
                "config.writable_roots entry {} must be an absolute path",
                display_value(entry)
            ));
        }
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => {}
            _ => {
                return Err(format!(
                    "config.writable_roots entry {} must be an existing directory",
                    go_quote(path)
                ));
            }
        }
        roots.push(clean_path(path));
    }
    Ok(roots)
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn clean_path(path: &str) -> PathBuf {
    crate::util::fs::lexical_clean(Path::new(path))
}

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

pub(crate) fn provider_names(config: &Config) -> Vec<String> {
    config.providers.keys().cloned().collect()
}

pub(crate) fn api_key_for(config: &Config, name: &str) -> Result<String, String> {
    let provider = config.providers.get(name).ok_or_else(|| {
        format!(
            "unknown provider {}; configured providers: {}",
            go_quote(name),
            provider_names(config).join(", ")
        )
    })?;
    if provider.api == Api::CodexAppServer {
        return Ok(String::new());
    }
    if let Some(key) = provider.key.as_deref().filter(|key| !key.is_empty()) {
        return Ok(key.to_string());
    }
    let env_key = provider.env_key.clone().unwrap_or_default();
    match std::env::var(&env_key) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(format!(
            "environment variable {env_key} (providers.{name}.env_key) must be set to the API key"
        )),
    }
}

fn has_model(provider: &ProviderConfig, id: &str) -> bool {
    provider.models.iter().any(|model| model.id == id)
}

fn model_ids(provider: &ProviderConfig) -> Vec<String> {
    provider
        .models
        .iter()
        .map(|model| model.id.clone())
        .collect()
}

fn map_effort(provider: &ProviderConfig, value: &str) -> String {
    provider
        .effort_map
        .get(value)
        .cloned()
        .unwrap_or_else(|| value.to_string())
}

// ---------------------------------------------------------------------------
// Tool declarations
// ---------------------------------------------------------------------------

fn default_annotations() -> Value {
    json!({
        "readOnlyHint": false,
        "destructiveHint": true,
        "idempotentHint": false,
        "openWorldHint": true
    })
}

fn effort_description() -> String {
    let listed = match EFFORT_VALUES.len() {
        0 => {
            return "Reasoning effort; defaults to high, or to config.model_reasoning_effort."
                .into();
        }
        1 => EFFORT_VALUES[0].to_string(),
        len => {
            let head = EFFORT_VALUES[..len - 1].join(", ");
            format!("{head}, or {}", EFFORT_VALUES[len - 1])
        }
    };
    format!("Reasoning effort: {listed}; defaults to high, or to config.model_reasoning_effort.")
}

fn provider_description(config: &Config) -> String {
    let mut description = format!(
        "Configured provider to use for this session; required when more than one is configured. Available: {}.",
        provider_names(config).join(", ")
    );
    if config.auto.is_some() {
        description
            .push_str(" Omit it and set model to \"auto\" to let the decision model choose.");
    }
    description
}

fn model_union_schema(config: &Config) -> (Vec<String>, String) {
    let mut ids = Vec::new();
    if config.auto.is_some() {
        ids.push("auto".to_string());
    }
    let mut seen = BTreeSet::new();
    let mut groups = Vec::new();
    for name in provider_names(config) {
        let provider = &config.providers[&name];
        let mut models = Vec::new();
        for model in &provider.models {
            if seen.insert(model.id.clone()) {
                ids.push(model.id.clone());
            }
            let mut rendered = model.id.clone();
            if !model.description.is_empty() {
                rendered = format!("{rendered} ({})", model.description);
            }
            if model.id == provider.default_model {
                rendered.push_str(" (default)");
            }
            models.push(rendered);
        }
        groups.push(format!("{name}: {}", models.join(", ")));
    }
    let mut description =
        "Model to use; the available id depends on the chosen provider.".to_string();
    if config.auto.is_some() {
        description.push_str(
            " Pass \"auto\" to let the configured decision model choose among every reachable configured model.",
        );
    }
    description.push_str(&format!(" By provider: {}", groups.join("; ")));
    (ids, description)
}

fn start_definition(name: &str, config: &Config) -> ToolDefinition {
    let (model_ids, model_description) = model_union_schema(config);
    let mut provider_property = Map::new();
    provider_property.insert("type".into(), json!("string"));
    provider_property.insert("description".into(), json!(provider_description(config)));
    provider_property.insert("enum".into(), json!(provider_names(config)));

    let mut required = vec!["prompt".to_string()];
    if config.providers.len() > 1 && config.auto.is_none() {
        required.push("provider".into());
    }

    let mut properties = Map::new();
    properties.insert(
        "prompt".into(),
        json!({"type": "string", "description": PROMPT_DESCRIPTION}),
    );
    properties.insert("provider".into(), Value::Object(provider_property));
    properties.insert(
        "model".into(),
        json!({
            "type": "string",
            "description": model_description,
            "enum": model_ids
        }),
    );
    properties.insert(
        "cwd".into(),
        json!({
            "type": "string",
            "description": "Absolute path to an existing working directory; defaults to the subagent-mcp process working directory."
        }),
    );
    properties.insert(
        "sandbox".into(),
        json!({
            "type": "string",
            "description": "Sandbox mode: read-only, workspace-write, or danger-full-access; defaults to read-only."
        }),
    );
    properties.insert(
        "approval-policy".into(),
        json!({
            "type": "string",
            "description": "Approval policy: untrusted, on-request, on-failure, or never; defaults to on-request."
        }),
    );
    properties.insert(
        "reasoning-effort".into(),
        json!({
            "type": "string",
            "description": effort_description(),
            "enum": EFFORT_VALUES
        }),
    );
    properties.insert(
        "base-instructions".into(),
        json!({
            "type": "string",
            "description": "Complete replacement for the built-in base system instructions; empty or omitted uses the built-in default."
        }),
    );
    properties.insert(
        "developer-instructions".into(),
        json!({
            "type": "string",
            "description": "Additional system instructions appended after the selected base instructions; empty or omitted appends nothing."
        }),
    );
    properties.insert(
        "config".into(),
        json!({
            "type": "object",
            "properties": {},
            "description": CONFIG_DESCRIPTION
        }),
    );

    ToolDefinition {
        name: name.to_string(),
        description: START_DESCRIPTION.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": Value::Object(properties),
            "required": required
        }),
        output_schema: start_output_schema(),
        annotations: Some(default_annotations()),
    }
}

fn reply_definition(base_name: &str) -> ToolDefinition {
    ToolDefinition {
        name: format!("{base_name}-reply"),
        description: REPLY_DESCRIPTION.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "threadId": {
                    "type": "string",
                    "description": format!(
                        "Thread ID returned by a previous {base_name} or {base_name}-reply call."
                    )
                },
                "prompt": {
                    "type": "string",
                    "description": REPLY_PROMPT_DESCRIPTION
                }
            },
            "required": ["threadId", "prompt"]
        }),
        output_schema: reply_output_schema(),
        annotations: Some(default_annotations()),
    }
}

fn auto_output_schema() -> Value {
    json!({
        "type": "object",
        "description": AUTO_DESCRIPTION,
        "properties": {
            "skipped": {
                "type": "boolean",
                "description": "True when the decision model was skipped because only one candidate was available."
            },
            "omitted": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Reasons providers were omitted from the candidates, when any were omitted."
            },
            "judge_model": {
                "type": "string",
                "description": "Model that answered the decision request; absent when the decision model was skipped."
            },
            "confidence": {
                "type": "number",
                "description": "Decision model confidence in the chosen pair, from 0 to 1; absent when the decision model was skipped."
            },
            "probabilities": {
                "type": "object",
                "additionalProperties": {"type": "number"},
                "description": "Probability of each candidate, keyed by provider/model."
            },
            "input_tokens": {
                "type": "integer",
                "description": "Decision model input tokens billed for this decision; absent when the decision model was skipped."
            },
            "output_tokens": {
                "type": "integer",
                "description": "Decision model output tokens for this decision; absent when the decision model was skipped."
            }
        }
    })
}

fn start_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "threadId": {
                "type": "string",
                "description": THREAD_ID_DESCRIPTION
            },
            "content": {
                "type": "string",
                "description": CONTENT_DESCRIPTION
            },
            "provider": {
                "type": "string",
                "description": PROVIDER_OUTPUT_DESCRIPTION
            },
            "model": {
                "type": "string",
                "description": MODEL_OUTPUT_DESCRIPTION
            },
            "auto": auto_output_schema()
        },
        "required": ["threadId", "content"]
    })
}

fn reply_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "threadId": {
                "type": "string",
                "description": THREAD_ID_DESCRIPTION
            },
            "content": {
                "type": "string",
                "description": CONTENT_DESCRIPTION
            },
            "provider": {
                "type": "string",
                "description": PROVIDER_OUTPUT_DESCRIPTION
            },
            "model": {
                "type": "string",
                "description": MODEL_OUTPUT_DESCRIPTION
            }
        },
        "required": ["threadId", "content"]
    })
}

// ---------------------------------------------------------------------------
// Rollout session_meta
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn session_meta(
    thread_id: &str,
    cwd: &Path,
    system_prompt: &str,
    provider: &str,
    auto: Option<&AutoSelection>,
    created: SystemTime,
) -> Value {
    let mut meta = Map::new();
    meta.insert("session_id".into(), json!(thread_id));
    meta.insert("id".into(), json!(thread_id));
    meta.insert("timestamp".into(), json!(rfc3339_nano(created)));
    meta.insert("cwd".into(), json!(cwd.display().to_string()));
    meta.insert("originator".into(), json!("subagent-mcp"));
    meta.insert("cli_version".into(), json!(crate::VERSION));
    meta.insert("source".into(), json!("mcp"));
    meta.insert("model_provider".into(), json!(provider));
    meta.insert("base_instructions".into(), json!({"text": system_prompt}));
    if let Some(auto) = auto {
        let mut selection = match auto.meta() {
            Value::Object(selection) => selection,
            _ => Map::new(),
        };
        selection.insert("provider".into(), json!(auto.provider));
        selection.insert("model".into(), json!(auto.model));
        meta.insert("model_selection".into(), Value::Object(selection));
    }
    if let Ok(resolved) = eval_symlinks(cwd)
        && git_root(&resolved).is_some()
    {
        let info = repo::git_info(&resolved);
        meta.insert(
            "git".into(),
            json!({
                "branch": info.branch.unwrap_or_default(),
                "commit_hash": info.commit.unwrap_or_default()
            }),
        );
    }
    Value::Object(meta)
}

/// Go `repo.Root`: nearest ancestor (inclusive) with a `.git` entry.
fn git_root(dir: &Path) -> Option<PathBuf> {
    let mut current = dir.to_path_buf();
    loop {
        if std::fs::symlink_metadata(current.join(".git")).is_ok() {
            return Some(current);
        }
        match current.parent() {
            Some(parent) if parent != current && !parent.as_os_str().is_empty() => {
                current = parent.to_path_buf();
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn writable_root_paths_are_lexically_cleaned() {
        for (input, expected) in [
            ("", "."),
            ("a/../b/", "b"),
            ("../../a/../b", "../../b"),
            ("/tmp/a/../b/./", "/tmp/b"),
            ("/../../b", "/b"),
        ] {
            assert_eq!(super::clean_path(input), std::path::PathBuf::from(expected));
        }
    }
    use super::*;

    #[test]
    fn integer_value_rejects_fractions_and_accepts_whole_floats() {
        assert_eq!(integer_value(&json!(1)), Some(1));
        assert_eq!(integer_value(&json!(1.0)), Some(1));
        assert_eq!(integer_value(&json!(1.5)), None);
        assert_eq!(integer_value(&json!("1")), None);
        assert_eq!(integer_value(&json!(-1)), Some(-1));
    }

    #[test]
    fn codex_is_available_in_24b() {
        register_adapters();
        assert!(codex_available());
    }

    #[test]
    fn result_helpers_omit_empty_provider_and_model() {
        let start = start_result("thread-1", "text", "", "", None, false);
        let structured = start["structuredContent"].as_object().unwrap();
        assert!(structured.get("provider").is_none());
        assert!(structured.get("model").is_none());
        assert_eq!(structured["threadId"], "thread-1");
        assert_eq!(structured["content"], "text");

        let reply = reply_result("thread-1", "text", "", "", false);
        let structured = reply["structuredContent"].as_object().unwrap();
        assert!(structured.get("provider").is_none());
        assert!(structured.get("model").is_none());
    }

    #[test]
    fn default_budgets_match_go() {
        let budgets = RunBudgets::default();
        assert_eq!(budgets.history_bytes, 64 << 20);
        assert_eq!(budgets.total_tokens, 0);
        assert_eq!(budgets.run_deadline, Duration::ZERO);
    }

    struct PlainProvider;

    #[async_trait]
    impl Provider for PlainProvider {
        fn name(&self) -> &str {
            "plain"
        }

        async fn turn(
            &self,
            _ctx: crate::util::run::RunContext,
            _req: crate::provider::TurnRequest,
            _on_delta: crate::provider::DeltaSink,
        ) -> Result<crate::provider::TurnResult, crate::provider::ProviderError> {
            Ok(crate::provider::TurnResult::default())
        }
    }

    fn codex_provider_cfg() -> ProviderConfig {
        ProviderConfig {
            api: Api::CodexAppServer,
            base_url: String::new(),
            env_key: None,
            key: None,
            command: Some("codex-test".into()),
            default_model: "gpt-6.1-terra".into(),
            models: Vec::new(),
            effort_map: Default::default(),
            max_output_tokens: None,
            max_nudges: None,
            extra_body: None,
        }
    }

    #[test]
    fn codex_app_server_requires_the_agent_capability() {
        let plain: Arc<dyn Provider> = Arc::new(PlainProvider);
        let error = ensure_agent_capable("codex", &codex_provider_cfg(), &plain).unwrap_err();
        assert!(error.contains("codex-app-server"), "{error}");
        assert!(error.contains("Agent capability"), "{error}");

        // Non-Codex APIs are never required to be agents.
        let http = ProviderConfig {
            api: Api::Responses,
            ..codex_provider_cfg()
        };
        assert!(!ensure_agent_capable("http", &http, &plain).unwrap());

        // The registered adapter exposes the capability.
        let real: Arc<dyn Provider> =
            Arc::new(CodexAdapter::new("codex", &codex_provider_cfg()).unwrap());
        assert!(ensure_agent_capable("codex", &codex_provider_cfg(), &real).unwrap());
    }
}
