//! `--check-config` provider validation.
//!
//! Ports Go `cmd/subagent-mcp/check.go` (subagent-mcp v0.13.1). [`run`] writes
//! human-readable status lines to stdout, never a key value, and returns the
//! process exit code: 1 when any provider failed or no provider was checked.
//! [`run_with`] exposes the adapter factory seam used by tests (Go's registry
//! plus package `init` is replaced by an explicit constructor table).
//!
//! `--live` performs the same minimal tool-call round trip as Go
//! `internal/provider/live_test.go`; Codex uses a whole agent turn instead.
//! Every request is bounded by the Go timeouts ([`LIST_TIMEOUT`],
//! [`LIVE_TIMEOUT`]).

use std::collections::{BTreeSet, HashSet};
use std::future::Future;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::value::RawValue;

use crate::config::{Api, Config, ProviderConfig};
use crate::policy::{ApprovalPolicy, Sandbox};
use crate::provider::chat_completions::ChatCompletions;
use crate::provider::codex::CodexAdapter;
use crate::provider::messages::Messages;
use crate::provider::responses::Responses;
use crate::provider::{
    Agent, AuthChecker, Message, ModelLister, Provider, ProviderError, Role, ThreadCallbacks,
    ThreadOptions, ToolSpec, TurnRequest,
};
use crate::util::approval::{ApprovalRequest, Approver};
use crate::util::duration::format_go_duration;
use crate::util::json::go_quote;
use crate::util::run::RunContext;
use tokio_util::sync::CancellationToken;

/// Go `checkListTimeout`.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(15);
/// Go `checkLiveTimeout`.
pub const LIVE_TIMEOUT: Duration = Duration::from_secs(60);

/// Go `checkToolName`; the fake values never leave this process.
pub const TOOL_NAME: &str = "get_secret_number";
/// Go `checkSecretNumber`.
pub const SECRET_NUMBER: &str = "4217";
/// Go `checkLivePrompt`; the answer never appears in the prompt.
pub const LIVE_PROMPT: &str = "What is 2713 plus 1504? Reply with just the number.";

const LIVE_ROUND_TRIP_PROMPT: &str =
    "What is the secret number? Use the tool, then answer with just the number.";
const LIVE_TOOL_DESCRIPTION: &str = "Returns the secret number. Always call this before answering.";
const LIVE_TOOL_SCHEMA: &str = r#"{"type":"object","properties":{},"required":[]}"#;

/// The adapters a check needs. Production builds these from the concrete
/// provider types; tests supply stubs through [`AdapterFactory`].
#[derive(Clone)]
pub struct Built {
    pub provider: Arc<dyn Provider>,
    pub lister: Option<Arc<dyn ModelLister>>,
    pub agent: Option<Arc<dyn Agent>>,
    pub auth: Option<Arc<dyn AuthChecker>>,
}

/// Constructor seam for [`run_with`]. Go uses the provider registry plus
/// blank-import `init` functions; the explicit table keeps `check` independent
/// of the MCP server wiring owned by Tasks 24/25B.
pub trait AdapterFactory {
    fn build(&self, name: &str, config: &ProviderConfig, key: &str)
    -> Result<Built, ProviderError>;
}

/// The production factory: build the concrete adapter for `config.api`.
pub struct ProductionFactory;

impl AdapterFactory for ProductionFactory {
    fn build(
        &self,
        name: &str,
        config: &ProviderConfig,
        _key: &str,
    ) -> Result<Built, ProviderError> {
        match config.api {
            Api::ChatCompletions => {
                let adapter = Arc::new(ChatCompletions::new(name, config)?);
                let provider: Arc<dyn Provider> = adapter.clone();
                let lister: Arc<dyn ModelLister> = adapter;
                Ok(Built {
                    provider,
                    lister: Some(lister),
                    agent: None,
                    auth: None,
                })
            }
            Api::Responses => {
                let adapter = Arc::new(Responses::new(name, config)?);
                let provider: Arc<dyn Provider> = adapter.clone();
                let lister: Arc<dyn ModelLister> = adapter;
                Ok(Built {
                    provider,
                    lister: Some(lister),
                    agent: None,
                    auth: None,
                })
            }
            Api::Messages => {
                let adapter = Arc::new(Messages::new(name, config)?);
                let provider: Arc<dyn Provider> = adapter.clone();
                let lister: Arc<dyn ModelLister> = adapter;
                Ok(Built {
                    provider,
                    lister: Some(lister),
                    agent: None,
                    auth: None,
                })
            }
            Api::CodexAppServer => {
                let adapter = Arc::new(CodexAdapter::new(name, config)?);
                let provider: Arc<dyn Provider> = adapter.clone();
                let lister: Arc<dyn ModelLister> = adapter.clone();
                let agent: Arc<dyn Agent> = adapter.clone();
                let auth: Arc<dyn AuthChecker> = adapter;
                Ok(Built {
                    provider,
                    lister: Some(lister),
                    agent: Some(agent),
                    auth: Some(auth),
                })
            }
        }
    }
}

/// Every non-empty provider and auto `env_key`, sorted and deduplicated
/// (Go `Config.EnvKeys`).
pub fn env_keys(config: &Config) -> Vec<String> {
    let mut keys = BTreeSet::new();
    for provider in config.providers.values() {
        if let Some(key) = provider.env_key.as_deref().filter(|key| !key.is_empty()) {
            keys.insert(key.to_string());
        }
    }
    if let Some(key) = config
        .auto
        .as_ref()
        .and_then(|auto| auto.env_key.as_deref())
        .filter(|key| !key.is_empty())
    {
        keys.insert(key.to_string());
    }
    keys.into_iter().collect()
}

/// Print whether the kernel sandbox is usable (Go `reportSandbox`). It is
/// informational and never changes the exit code.
pub fn report_sandbox<W: Write>(out: &mut W) {
    if crate::sandbox::available() {
        let _ = writeln!(out, "sandbox  Landlock   OK");
        return;
    }
    let _ = writeln!(
        out,
        "sandbox  Landlock   WARN: {}; auto-allowed shell calls will exit 126 (Linux 5.13+ required)",
        sandbox_unavailable_reason()
    );
}

#[cfg(target_os = "linux")]
fn sandbox_unavailable_reason() -> String {
    match crate::sandbox::landlock_abi() {
        Ok(abi) => format!("landlock unavailable: ABI {abi} is not supported"),
        Err(error) => format!("landlock unavailable: {error}"),
    }
}

#[cfg(not(target_os = "linux"))]
fn sandbox_unavailable_reason() -> String {
    "landlock unavailable: kernel sandbox requires Linux".to_string()
}

/// Validate every configured provider and return the process exit code.
pub fn run(config: &Config, live: bool) -> i32 {
    let mut stdout = std::io::stdout().lock();
    run_to(&mut stdout, config, live)
}

/// [`run`] with an explicit output sink.
pub fn run_to<W: Write>(out: &mut W, config: &Config, live: bool) -> i32 {
    run_with(out, config, live, &ProductionFactory)
}

/// [`run_to`] with an explicit adapter factory (the test seam).
pub fn run_with<W: Write, F: AdapterFactory + ?Sized>(
    out: &mut W,
    config: &Config,
    live: bool,
    factory: &F,
) -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(
                out,
                "result   FAIL (0 checked, 0 skipped, 1 failed): {error}"
            );
            return 1;
        }
    };
    runtime.block_on(run_async(out, config, live, factory))
}

async fn run_async<W: Write, F: AdapterFactory + ?Sized>(
    out: &mut W,
    config: &Config,
    live: bool,
    factory: &F,
) -> i32 {
    if live {
        let _ = writeln!(
            out,
            "--live will make real, billed API calls to each configured provider."
        );
    }

    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    // BTreeMap iterates in sorted name order, matching Go's sort.Strings.
    for (name, provider) in &config.providers {
        let _ = writeln!(out, "provider {name} ({})", provider.api.as_str());

        if provider.api == Api::CodexAppServer {
            let (auth_ok, check_failed) = check_codex(out, name, provider, live, factory).await;
            if auth_ok {
                checked += 1;
            }
            if check_failed {
                failed += 1;
            }
            continue;
        }

        // Never print the key itself, only where it comes from.
        let key = if let Some(key) = provider.key.as_deref().filter(|key| !key.is_empty()) {
            let _ = writeln!(out, "  key    config file   set");
            key.to_string()
        } else {
            let env_key = provider.env_key.clone().unwrap_or_default();
            match std::env::var(&env_key)
                .ok()
                .filter(|value| !value.is_empty())
            {
                Some(value) => {
                    let _ = writeln!(out, "  key    {env_key}   set");
                    value
                }
                None => {
                    let _ = writeln!(out, "  key    {env_key}   not set, skipped");
                    skipped += 1;
                    continue;
                }
            }
        };
        checked += 1;

        let built = match factory.build(name, provider, &key) {
            Ok(built) => built,
            Err(error) => {
                let _ = writeln!(out, "  api    {}   FAIL: {error}", provider.base_url);
                failed += 1;
                continue;
            }
        };

        let mut listing_ok = false;
        match &built.lister {
            None => {
                let _ = writeln!(
                    out,
                    "  api    {}   OK (model listing not supported)",
                    provider.base_url
                );
            }
            Some(lister) => match with_timeout(LIST_TIMEOUT, lister.list_models()).await {
                Err(error) => {
                    let _ = writeln!(out, "  api    {}   FAIL: {error}", provider.base_url);
                    failed += 1;
                    continue;
                }
                Ok(models) => {
                    let _ = writeln!(
                        out,
                        "  api    {}   OK ({} models listed)",
                        provider.base_url,
                        models.len()
                    );
                    let listed: HashSet<&str> =
                        models.iter().map(|model| model.id.as_str()).collect();
                    for model in &provider.models {
                        if listed.contains(model.id.as_str()) {
                            let _ = writeln!(out, "  model  {}     OK", model.id);
                        } else {
                            let _ = writeln!(
                                out,
                                "  model  {}     WARN: not in the provider's current model list",
                                model.id
                            );
                        }
                    }
                    listing_ok = true;
                }
            },
        }

        if !live || (!listing_ok && built.lister.is_some()) {
            continue;
        }

        let start = Instant::now();
        match live_round_trip(built.provider.as_ref(), &provider.default_model).await {
            Err(error) => {
                let _ = writeln!(out, "  live   {}   FAIL: {error}", provider.default_model);
                failed += 1;
            }
            Ok(()) => {
                let _ = writeln!(
                    out,
                    "  live   {}   OK (tool call + follow-up succeeded, {})",
                    provider.default_model,
                    round_millis(start.elapsed())
                );
            }
        }
    }

    if let Some(auto) = &config.auto {
        let _ = writeln!(out, "auto     decision model ({})", auto.model);
        let endpoint = if auto.url.is_empty() {
            &auto.base_url
        } else {
            &auto.url
        };
        let _ = writeln!(out, "  api    {endpoint}");
        if auto.key.as_deref().is_some_and(|key| !key.is_empty()) {
            let _ = writeln!(out, "  key    config file   set");
        } else {
            let env_key = auto.env_key.clone().unwrap_or_default();
            if std::env::var(&env_key)
                .ok()
                .filter(|value| !value.is_empty())
                .is_some()
            {
                let _ = writeln!(out, "  key    {env_key}   set");
            } else {
                let _ = writeln!(
                    out,
                    "  key    {env_key}   not set, decision model selection calls will fail"
                );
            }
        }
    }

    let (status, reason) = if failed > 0 {
        ("FAIL", "")
    } else if checked == 0 {
        ("FAIL", " — no configured provider has its key set")
    } else {
        ("PASS", "")
    };
    let _ = writeln!(
        out,
        "result   {status} ({checked} checked, {skipped} skipped, {failed} failed){reason}"
    );
    if status == "FAIL" { 1 } else { 0 }
}

async fn check_codex<W: Write, F: AdapterFactory + ?Sized>(
    out: &mut W,
    name: &str,
    provider: &ProviderConfig,
    live: bool,
    factory: &F,
) -> (bool, bool) {
    let built = match factory.build(name, provider, "") {
        Ok(built) => built,
        Err(error) => {
            let _ = writeln!(out, "  auth   FAIL: {error}");
            return (false, true);
        }
    };

    let Some(auth) = &built.auth else {
        let _ = writeln!(out, "  auth   FAIL: adapter does not support auth checks");
        return (false, true);
    };
    let status = match with_timeout(LIST_TIMEOUT, auth.check_auth()).await {
        Ok(status) => status,
        Err(error) => {
            let _ = writeln!(out, "  auth   FAIL: {error}");
            return (false, true);
        }
    };
    let _ = writeln!(out, "  auth   {}   OK", status.summary);

    let Some(lister) = &built.lister else {
        let _ = writeln!(out, "  api    FAIL: adapter does not support model listing");
        return (true, true);
    };
    let models = match with_timeout(LIST_TIMEOUT, lister.list_models()).await {
        Ok(models) => models,
        Err(error) => {
            let _ = writeln!(out, "  api    FAIL: {error}");
            return (true, true);
        }
    };
    let _ = writeln!(
        out,
        "  api    {}   OK ({} models listed)",
        status.version.as_deref().unwrap_or(""),
        models.len()
    );
    let listed: HashSet<&str> = models.iter().map(|model| model.id.as_str()).collect();
    for model in &provider.models {
        if listed.contains(model.id.as_str()) {
            let _ = writeln!(out, "  model  {}     OK", model.id);
        } else {
            let _ = writeln!(
                out,
                "  model  {}     WARN: not in the provider's current model list",
                model.id
            );
        }
    }

    if live {
        let Some(agent) = &built.agent else {
            let _ = writeln!(
                out,
                "  live   {}   FAIL: adapter does not support live turns",
                provider.default_model
            );
            return (true, true);
        };
        let start = Instant::now();
        match live_thread(agent.as_ref(), &provider.default_model).await {
            Err(error) => {
                let _ = writeln!(out, "  live   {}   FAIL: {error}", provider.default_model);
                return (true, true);
            }
            Ok(()) => {
                let _ = writeln!(
                    out,
                    "  live   {}   OK (turn succeeded, {})",
                    provider.default_model,
                    round_millis(start.elapsed())
                );
            }
        }
    }
    (true, false)
}

async fn with_timeout<T>(
    duration: Duration,
    future: impl Future<Output = Result<T, ProviderError>>,
) -> Result<T, ProviderError> {
    match tokio::time::timeout(duration, future).await {
        Ok(result) => result,
        Err(_) => Err(ProviderError {
            message: "context deadline exceeded".to_string(),
        }),
    }
}

fn live_context() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: Some(tokio::time::Instant::now() + LIVE_TIMEOUT),
    }
}

fn check_tool() -> ToolSpec {
    ToolSpec {
        name: TOOL_NAME.to_string(),
        description: LIVE_TOOL_DESCRIPTION.to_string(),
        parameters: RawValue::from_string(LIVE_TOOL_SCHEMA.to_string())
            .expect("the check tool schema is valid JSON"),
    }
}

/// Go `checkLiveRoundTrip`: ask, require a tool call, answer the tool, then
/// require the follow-up text to contain the tool result.
async fn live_round_trip(provider: &dyn Provider, model: &str) -> Result<(), ProviderError> {
    match tokio::time::timeout(LIVE_TIMEOUT, round_trip_inner(provider, model)).await {
        Ok(result) => result,
        Err(_) => Err(ProviderError {
            message: "context deadline exceeded".to_string(),
        }),
    }
}

async fn round_trip_inner(provider: &dyn Provider, model: &str) -> Result<(), ProviderError> {
    let context = live_context();
    let tools = vec![check_tool()];
    let mut history = vec![Message {
        role: Role::User,
        text: LIVE_ROUND_TRIP_PROMPT.to_string(),
        ..Default::default()
    }];

    let first = provider
        .turn(
            context.child(),
            TurnRequest {
                model: model.to_string(),
                effort: Some("low".to_string()),
                messages: history.clone(),
                tools: tools.clone(),
                ..Default::default()
            },
            Box::new(|_| {}),
        )
        .await?;
    if first.tool_calls.is_empty() {
        return Err(ProviderError {
            message: format!("model did not call the {TOOL_NAME} tool"),
        });
    }

    history.push(Message {
        role: Role::Assistant,
        text: first.text.clone(),
        tool_calls: first.tool_calls.clone(),
        opaque: first.opaque.clone(),
        ..Default::default()
    });
    for call in &first.tool_calls {
        history.push(Message {
            role: Role::Tool,
            tool_call_id: Some(call.id.clone()),
            text: SECRET_NUMBER.to_string(),
            ..Default::default()
        });
    }

    let second = provider
        .turn(
            context.child(),
            TurnRequest {
                model: model.to_string(),
                effort: Some("low".to_string()),
                messages: history,
                tools,
                ..Default::default()
            },
            Box::new(|_| {}),
        )
        .await
        .map_err(|error| ProviderError {
            message: format!("follow-up turn: {error}"),
        })?;
    if !second.text.contains(SECRET_NUMBER) {
        return Err(ProviderError {
            message: format!(
                "follow-up answer {} does not contain the tool result",
                go_quote(&second.text)
            ),
        });
    }
    Ok(())
}

/// Go `checkLiveThread`: one ephemeral, read-only, never-approved agent turn.
async fn live_thread(agent: &dyn Agent, model: &str) -> Result<(), ProviderError> {
    match tokio::time::timeout(LIVE_TIMEOUT, thread_inner(agent, model)).await {
        Ok(result) => result,
        Err(_) => Err(ProviderError {
            message: "context deadline exceeded".to_string(),
        }),
    }
}

async fn thread_inner(agent: &dyn Agent, model: &str) -> Result<(), ProviderError> {
    let context = live_context();
    let options = ThreadOptions {
        model: model.to_string(),
        cwd: std::env::temp_dir(),
        sandbox: Sandbox::ReadOnly,
        approval_policy: ApprovalPolicy::Never,
        base_instructions: None,
        developer_instructions: None,
        writable_roots: Vec::new(),
        ephemeral: true,
    };
    let thread = agent.start_thread(&context, options).await?;
    let callbacks = ThreadCallbacks {
        emit: Arc::new(|_, _| {}),
        approve: Arc::new(DenyApprover),
    };
    let result = thread
        .run(
            context.child(),
            LIVE_PROMPT.to_string(),
            Some("low".to_string()),
            callbacks,
        )
        .await;
    let _ = thread.close().await;
    let text = result?;
    if !text.contains(SECRET_NUMBER) {
        return Err(ProviderError {
            message: format!(
                "answer {} does not contain {}",
                go_quote(&text),
                SECRET_NUMBER
            ),
        });
    }
    Ok(())
}

struct DenyApprover;

#[async_trait]
impl Approver for DenyApprover {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        false
    }
}

/// Go `time.Since(start).Round(time.Millisecond)` then `%s`.
fn round_millis(duration: Duration) -> String {
    let millis = (duration.as_nanos() + 500_000) / 1_000_000;
    format_go_duration(Duration::from_millis(millis as u64))
}
