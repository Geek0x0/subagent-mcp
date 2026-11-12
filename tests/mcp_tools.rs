//! Task 24A: non-Codex MCP tool handlers.
//!
//! Ports the non-Codex half of Go `internal/server/server_test.go` (v0.13.1):
//! the exact `subagent` / `subagent-reply` declarations, argument resolution,
//! provider/model/effort/cwd/budget handling, rollout `session_meta`, progress
//! events, and thread/error shapes. Codex-specific tests belong to Task 24B.

#![allow(clippy::await_holding_lock)] // process-global env lock, intentionally held in current-thread tests

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use subagent_mcp::config::{Api, Config, ProviderConfig};
use subagent_mcp::mcp::jsonrpc::RequestId;
use subagent_mcp::mcp::progress::ProgressSink;
use subagent_mcp::mcp::server::{CallContext, ToolError, ToolHandler};
use subagent_mcp::mcp::tools::SubagentTools;
use subagent_mcp::provider::{
    DeltaSink, Provider, ProviderError, TurnRequest, TurnResult, factory,
};
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;
use tokio::sync::{Barrier, Notify, mpsc};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Environment serialization
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Every test serializes environment changes: `CODEX_HOME` and
/// `SUBAGENT_MCP_ROLLOUT` are process-global. Rollout is off by default so a
/// test never writes into the developer's real `~/.codex`.
fn env_guard() -> MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("SUBAGENT_MCP_ROLLOUT", "off");
    }
    guard
}

// ---------------------------------------------------------------------------
// Test provider registry (factory dispatcher keyed by base_url)
// ---------------------------------------------------------------------------

static STUBS: LazyLock<Mutex<HashMap<String, Arc<dyn Provider>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_STUB: AtomicUsize = AtomicUsize::new(0);

fn ensure_test_factory() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        factory::register(
            Api::ChatCompletions.as_str(),
            Arc::new(|_name, cfg, _key| {
                STUBS
                    .lock()
                    .unwrap()
                    .get(&cfg.base_url)
                    .cloned()
                    .ok_or_else(|| ProviderError {
                        message: format!("no test stub registered for key {:?}", cfg.base_url),
                    })
            }),
        );
    });
}

fn register_stub(provider: Arc<dyn Provider>) -> String {
    ensure_test_factory();
    let key = format!(
        "test-stub://{}",
        NEXT_STUB.fetch_add(1, Ordering::SeqCst) + 1
    );
    STUBS.lock().unwrap().insert(key.clone(), provider);
    key
}

// ---------------------------------------------------------------------------
// Stub provider
// ---------------------------------------------------------------------------

type StubTurn = Result<TurnResult, String>;

fn text_turn(text: &str) -> StubTurn {
    Ok(TurnResult {
        text: text.into(),
        ..Default::default()
    })
}

fn tool_turn(calls: Vec<subagent_mcp::provider::ToolCall>) -> StubTurn {
    Ok(TurnResult {
        tool_calls: calls,
        ..Default::default()
    })
}

struct StubProvider {
    name: String,
    turns: Mutex<VecDeque<StubTurn>>,
    repeat: Option<StubTurn>,
    requests: Mutex<Vec<TurnRequest>>,
    entered: Option<Arc<Notify>>,
    release: Option<Arc<Notify>>,
    barrier: Option<Arc<Barrier>>,
}

impl StubProvider {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            turns: Mutex::new(VecDeque::new()),
            repeat: None,
            requests: Mutex::new(Vec::new()),
            entered: None,
            release: None,
            barrier: None,
        }
    }

    fn with_turns(name: &str, turns: Vec<StubTurn>) -> Self {
        let mut provider = Self::new(name);
        provider.turns = Mutex::new(turns.into());
        provider
    }

    fn with_repeat(name: &str, turn: StubTurn) -> Self {
        let mut provider = Self::new(name);
        provider.repeat = Some(turn);
        provider
    }

    fn blocking(mut self, entered: Arc<Notify>, release: Arc<Notify>) -> Self {
        self.entered = Some(entered);
        self.release = Some(release);
        self
    }

    fn with_barrier(mut self, barrier: Arc<Barrier>) -> Self {
        self.barrier = Some(barrier);
        self
    }

    fn requests(&self) -> Vec<TurnRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for StubProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn turn(
        &self,
        _ctx: RunContext,
        req: TurnRequest,
        _on_delta: DeltaSink,
    ) -> Result<TurnResult, ProviderError> {
        self.requests.lock().unwrap().push(req);
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        if let Some(entered) = &self.entered {
            entered.notify_one();
        }
        if let Some(release) = &self.release {
            release.notified().await;
        }
        let turn = {
            let mut turns = self.turns.lock().unwrap();
            turns.pop_front()
        };
        match turn.or_else(|| self.repeat.clone()) {
            Some(Ok(result)) => Ok(result),
            Some(Err(message)) => Err(ProviderError { message }),
            None => Err(ProviderError {
                message: "stub provider has no queued turn".into(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Config + call helpers
// ---------------------------------------------------------------------------

fn provider_config(base_url: String) -> ProviderConfig {
    ProviderConfig {
        api: Api::ChatCompletions,
        base_url,
        env_key: None,
        key: Some("test-key".into()),
        command: None,
        default_model: "m-fast".into(),
        models: vec![
            subagent_mcp::config::ModelConfig {
                id: "m-fast".into(),
                description: "fast".into(),
            },
            subagent_mcp::config::ModelConfig {
                id: "m-pro".into(),
                description: String::new(),
            },
        ],
        effort_map: BTreeMap::from([("xhigh".to_string(), "max".to_string())]),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    }
}

fn test_config(providers: Vec<(&str, Arc<dyn Provider>)>) -> Config {
    let mut map = BTreeMap::new();
    for (name, provider) in providers {
        let key = register_stub(provider);
        map.insert(name.to_string(), provider_config(key));
    }
    Config {
        path: PathBuf::from("/test/config.toml"),
        providers: map,
        auto: None,
    }
}

fn tools(config: Config) -> SubagentTools {
    ensure_test_factory();
    SubagentTools::new(
        "subagent".into(),
        Arc::new(config),
        Arc::new(subagent_mcp::agent::SessionManager::new()),
    )
}

/// A working directory that outlives the expression that created it. `keep`
/// avoids deleting it before the async call starts.
fn cwd() -> PathBuf {
    tempfile::tempdir().unwrap().keep()
}

struct Allow;

#[async_trait]
impl Approver for Allow {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        true
    }
}

fn call_context_with_progress(token: Option<Value>) -> (CallContext, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel(64);
    let ctx = CallContext {
        cancel: CancellationToken::new(),
        request_id: Some(RequestId::new(json!(1))),
        meta: Some(json!({"subagent-mcp/requestId": "int64:1"})),
        progress: Some(ProgressSink::new(token, tx, Arc::new(AtomicU64::new(0)))),
        approver: Arc::new(Allow),
    };
    (ctx, rx)
}

fn call_context() -> (CallContext, mpsc::Receiver<String>) {
    call_context_with_progress(None)
}

async fn call(tools: &SubagentTools, name: &str, args: Value) -> Result<Value, ToolError> {
    let (ctx, _rx) = call_context();
    tools.call(name, args, ctx).await
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

fn structured(result: &Value) -> &Map<String, Value> {
    result["structuredContent"]
        .as_object()
        .expect("structuredContent")
}

fn thread_id(result: &Value) -> &str {
    structured(result)["threadId"].as_str().unwrap()
}

fn is_error(result: &Value) -> bool {
    result["isError"].as_bool().unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

const CONFIG_DESCRIPTION: &str = "loose config map; recognized keys: max_turns (number from 1 to 100000), model_reasoning_effort (same values as reasoning-effort; the top-level argument wins), writable_roots (array of absolute directory paths the shell may also write under workspace-write), max_nudges (integer 0 to 10; how many times a reply without a tool call is answered with a continue prompt, overriding the provider default), max_history_bytes (integer ceiling on the history the thread replays; default 67108864, 0 disables), max_total_tokens (integer ceiling on the tokens the thread reports across all turns; default 0, off), run_deadline_seconds (integer wall-clock ceiling on one call; default 0, off); an unusable budget value is an error, other unknown keys are silently ignored";

fn expected_start_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": {
                "type": "string",
                "description": "Task prompt to send to the new subagent thread."
            },
            "provider": {
                "type": "string",
                "description": "Configured provider to use for this session; required when more than one is configured. Available: deepseek.",
                "enum": ["deepseek"]
            },
            "model": {
                "type": "string",
                "description": "Model to use; the available id depends on the chosen provider. By provider: deepseek: m-fast (fast) (default), m-pro",
                "enum": ["m-fast", "m-pro"]
            },
            "cwd": {
                "type": "string",
                "description": "Absolute path to an existing working directory; defaults to the subagent-mcp process working directory."
            },
            "sandbox": {
                "type": "string",
                "description": "Sandbox mode: read-only, workspace-write, or danger-full-access; defaults to read-only."
            },
            "approval-policy": {
                "type": "string",
                "description": "Approval policy: untrusted, on-request, on-failure, or never; defaults to on-request."
            },
            "reasoning-effort": {
                "type": "string",
                "description": "Reasoning effort: none, minimal, low, medium, high, xhigh, max, or ultra; defaults to high, or to config.model_reasoning_effort.",
                "enum": ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"]
            },
            "base-instructions": {
                "type": "string",
                "description": "Complete replacement for the built-in base system instructions; empty or omitted uses the built-in default."
            },
            "developer-instructions": {
                "type": "string",
                "description": "Additional system instructions appended after the selected base instructions; empty or omitted appends nothing."
            },
            "config": {
                "type": "object",
                "properties": {},
                "description": CONFIG_DESCRIPTION
            }
        },
        "required": ["prompt"]
    })
}

fn expected_reply_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "threadId": {
                "type": "string",
                "description": "Thread ID returned by a previous subagent or subagent-reply call."
            },
            "prompt": {
                "type": "string",
                "description": "Follow-up prompt to send to the existing subagent thread."
            }
        },
        "required": ["threadId", "prompt"]
    })
}

fn auto_output_schema() -> Value {
    json!({
        "type": "object",
        "description": "Auto selection details, present when the model was selected by auto mode.",
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

fn expected_start_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "threadId": {
                "type": "string",
                "description": "ID of the subagent thread that produced this result."
            },
            "content": {
                "type": "string",
                "description": "Human-readable report text from the subagent."
            },
            "provider": {
                "type": "string",
                "description": "Configured provider the session uses."
            },
            "model": {
                "type": "string",
                "description": "Model id the session uses."
            },
            "auto": auto_output_schema()
        },
        "required": ["threadId", "content"]
    })
}

fn expected_reply_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "threadId": {
                "type": "string",
                "description": "ID of the subagent thread that produced this result."
            },
            "content": {
                "type": "string",
                "description": "Human-readable report text from the subagent."
            },
            "provider": {
                "type": "string",
                "description": "Configured provider the session uses."
            },
            "model": {
                "type": "string",
                "description": "Model id the session uses."
            }
        },
        "required": ["threadId", "content"]
    })
}

fn annotations() -> Value {
    json!({
        "readOnlyHint": false,
        "destructiveHint": true,
        "idempotentHint": false,
        "openWorldHint": true
    })
}

#[test]
fn declarations_match_go_schemas_exactly() {
    let _env = env_guard();
    let tools = tools(test_config(vec![(
        "deepseek",
        Arc::new(StubProvider::new("deepseek")),
    )]));
    let definitions = tools.definitions();
    assert_eq!(definitions.len(), 2);

    let start = definitions
        .iter()
        .find(|d| d.name == "subagent")
        .expect("subagent definition");
    assert_eq!(
        start.description,
        "Start a new subagent coding-agent thread."
    );
    assert_eq!(start.input_schema, expected_start_input_schema());
    assert_eq!(start.output_schema, expected_start_output_schema());
    assert_eq!(start.annotations.as_ref(), Some(&annotations()));

    let reply = definitions
        .iter()
        .find(|d| d.name == "subagent-reply")
        .expect("subagent-reply definition");
    assert_eq!(
        reply.description,
        "Continue an existing subagent coding-agent thread."
    );
    assert_eq!(reply.input_schema, expected_reply_input_schema());
    assert_eq!(reply.output_schema, expected_reply_output_schema());
    assert_eq!(reply.annotations.as_ref(), Some(&annotations()));
}

#[test]
fn renamed_tool_changes_definitions_and_dispatch() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns(
        "deepseek",
        vec![text_turn("hi from codex"), text_turn("continued")],
    ));
    let config = test_config(vec![("deepseek", provider.clone())]);
    ensure_test_factory();
    let tools = SubagentTools::new(
        "codex".into(),
        Arc::new(config),
        Arc::new(subagent_mcp::agent::SessionManager::new()),
    );

    let names: Vec<String> = tools
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(names, vec!["codex", "codex-reply"]);

    let reply = tools
        .definitions()
        .into_iter()
        .find(|d| d.name == "codex-reply")
        .unwrap();
    let description = reply.input_schema["properties"]["threadId"]["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("codex"));
    assert!(description.contains("codex-reply"));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let (ctx, _rx) = call_context();
        let first = tools
            .call("codex", json!({"prompt": "hello", "cwd": cwd()}), ctx)
            .await
            .unwrap();
        assert!(!is_error(&first));
        assert_eq!(text(&first), "hi from codex");
        let id = thread_id(&first).to_string();

        let (ctx, _rx) = call_context();
        let reply = tools
            .call(
                "codex-reply",
                json!({"threadId": id, "prompt": "keep going"}),
                ctx,
            )
            .await
            .unwrap();
        assert!(!is_error(&reply));
        assert_eq!(text(&reply), "continued");
        assert_eq!(thread_id(&reply), id);

        let (ctx, _rx) = call_context();
        let wrong = tools.call("subagent", json!({"prompt": "x"}), ctx).await;
        assert!(matches!(wrong, Err(ToolError { code: -32602, .. })));
    });
}

#[test]
fn model_parameter_and_effort_enum_come_from_config() {
    let _env = env_guard();
    let tools = tools(test_config(vec![(
        "deepseek",
        Arc::new(StubProvider::new("deepseek")),
    )]));
    let schema = tools.definitions()[0].input_schema.clone();
    let serialized = schema.to_string();
    for want in [
        "\"m-fast\"",
        "\"m-pro\"",
        "deepseek: m-fast (fast) (default), m-pro",
    ] {
        assert!(
            serialized.contains(want),
            "schema lacks {want}: {serialized}"
        );
    }
    for effort in [
        "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
    ] {
        assert!(
            serialized.contains(&format!("\"{effort}\"")),
            "schema lacks effort {effort}"
        );
    }
}

#[test]
fn model_union_across_providers_deduplicates_and_marks_defaults() {
    let _env = env_guard();
    ensure_test_factory();
    let key_a = register_stub(Arc::new(StubProvider::new("a")));
    let key_b = register_stub(Arc::new(StubProvider::new("b")));
    let mut a = provider_config(key_a);
    a.default_model = "a-model".into();
    a.models = vec![
        subagent_mcp::config::ModelConfig {
            id: "a-model".into(),
            description: String::new(),
        },
        subagent_mcp::config::ModelConfig {
            id: "shared-model".into(),
            description: String::new(),
        },
    ];
    let mut b = provider_config(key_b);
    b.default_model = "b-model".into();
    b.models = vec![
        subagent_mcp::config::ModelConfig {
            id: "b-model".into(),
            description: "strong".into(),
        },
        subagent_mcp::config::ModelConfig {
            id: "shared-model".into(),
            description: String::new(),
        },
    ];
    let config = Config {
        path: PathBuf::from("/test/config.toml"),
        providers: BTreeMap::from([("a".to_string(), a), ("b".to_string(), b)]),
        auto: None,
    };
    let tools = tools(config);
    let schema = tools.definitions()[0].input_schema.to_string();
    for want in [
        "\"a-model\"",
        "\"b-model\"",
        "\"shared-model\"",
        "a: a-model (default), shared-model",
        "b: b-model (strong) (default), shared-model",
        "\"enum\":[\"a\",\"b\"]",
    ] {
        assert!(schema.contains(want), "schema lacks {want}: {schema}");
    }
    assert_eq!(
        schema.matches("\"shared-model\"").count(),
        1,
        "shared model must appear once in the enum"
    );
    assert!(schema.contains("\"required\":[\"prompt\",\"provider\"]"));
}

// ---------------------------------------------------------------------------
// Start: model, effort, instructions, validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_default_and_rejection() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider.clone())]));
    let cwd = tempfile::tempdir().unwrap();

    let ok = call(
        &tools,
        "subagent",
        json!({"prompt": "hi", "cwd": cwd.path()}),
    )
    .await
    .unwrap();
    assert!(!is_error(&ok));
    assert_eq!(provider.requests()[0].model, "m-fast");

    let bad = call(
        &tools,
        "subagent",
        json!({"prompt": "hi", "cwd": cwd.path(), "model": "deepseek-flash"}),
    )
    .await
    .unwrap();
    assert!(is_error(&bad));
    for want in ["deepseek-flash", "m-fast", "m-pro"] {
        assert!(
            text(&bad).contains(want),
            "error {} lacks {want}",
            text(&bad)
        );
    }
}

#[tokio::test]
async fn rejects_non_string_model_without_calling_provider() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::new("deepseek"));
    let tools = tools(test_config(vec![("deepseek", provider.clone())]));
    let result = call(
        &tools,
        "subagent",
        json!({"prompt": "x", "cwd": cwd(), "model": 5}),
    )
    .await
    .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("argument \"model\" must be a string"));
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn effort_passes_through_and_maps() {
    let _env = env_guard();
    for (requested, sent) in [
        (Some("xhigh"), "max"),
        (Some("medium"), "medium"),
        (Some("none"), "none"),
        (None, "high"),
    ] {
        let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
        let tools = tools(test_config(vec![("deepseek", provider.clone())]));
        let mut args = json!({"prompt": "hi", "cwd": cwd()});
        if let Some(requested) = requested {
            args["reasoning-effort"] = json!(requested);
        }
        let result = call(&tools, "subagent", args).await.unwrap();
        assert!(!is_error(&result));
        assert_eq!(provider.requests()[0].effort.as_deref(), Some(sent));
    }
}

#[tokio::test]
async fn reasoning_effort_sources() {
    let _env = env_guard();
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({"config": {"model_reasoning_effort": "xhigh"}}),
            "max",
        ),
        (
            json!({"config": {"model_reasoning_effort": "medium"}}),
            "medium",
        ),
        (
            json!({"config": {"model_reasoning_effort": "minimal"}}),
            "minimal",
        ),
        (
            json!({"config": {"model_reasoning_effort": "high"}}),
            "high",
        ),
        (json!({"reasoning-effort": "xhigh"}), "max"),
        (json!({"reasoning-effort": "none"}), "none"),
        (
            json!({"reasoning-effort": "low", "config": {"model_reasoning_effort": "max"}}),
            "low",
        ),
        (
            json!({"reasoning-effort": "", "config": {"model_reasoning_effort": "max"}}),
            "max",
        ),
    ];
    for (extra, want) in cases {
        let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
        let tools = tools(test_config(vec![("deepseek", provider.clone())]));
        let mut args = json!({"prompt": "hello", "cwd": cwd()});
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                args[key] = value;
            }
        }
        let result = call(&tools, "subagent", args).await.unwrap();
        assert!(!is_error(&result), "{:?}", text(&result));
        assert_eq!(
            provider.requests()[0].effort.as_deref(),
            Some(want),
            "wrong mapped effort"
        );
    }
}

#[tokio::test]
async fn rejects_invalid_config_reasoning_effort() {
    let _env = env_guard();
    for (value, contains) in [
        (
            json!("turbo"),
            vec!["config.model_reasoning_effort", "turbo", "none", "ultra"],
        ),
        (json!(3), vec!["config.model_reasoning_effort", "string"]),
    ] {
        let tools = tools(test_config(vec![(
            "deepseek",
            Arc::new(StubProvider::new("deepseek")),
        )]));
        let result = call(
            &tools,
            "subagent",
            json!({
                "prompt": "hello",
                "cwd": cwd(),
                "config": {"model_reasoning_effort": value}
            }),
        )
        .await
        .unwrap();
        assert!(is_error(&result));
        for want in contains {
            assert!(
                text(&result).contains(want),
                "{} lacks {want}",
                text(&result)
            );
        }
    }
}

#[tokio::test]
async fn start_validation_errors() {
    let _env = env_guard();
    let nonexistent = cwd().join("missing");
    let cases: Vec<(Value, Vec<&str>)> = vec![
        (json!({}), vec!["prompt is required"]),
        (
            json!({"prompt": "hello", "sandbox": "bogus"}),
            vec![
                "bogus",
                "read-only",
                "workspace-write",
                "danger-full-access",
            ],
        ),
        (
            json!({"prompt": "hello", "approval-policy": "bogus"}),
            vec!["bogus", "untrusted", "on-request", "on-failure", "never"],
        ),
        (
            json!({"prompt": "hello", "reasoning-effort": "bogus"}),
            vec![
                "reasoning-effort",
                "bogus",
                "none",
                "minimal",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
            ],
        ),
        (
            json!({"prompt": "hello", "cwd": "relative/path"}),
            vec!["cwd", "absolute"],
        ),
        (
            json!({"prompt": "hello", "cwd": 123}),
            vec!["cwd", "string"],
        ),
        (
            json!({"prompt": "hello", "cwd": nonexistent}),
            vec!["cwd", "does not exist"],
        ),
    ];
    for (args, contains) in cases {
        let tools = tools(test_config(vec![(
            "deepseek",
            Arc::new(StubProvider::new("deepseek")),
        )]));
        let result = call(&tools, "subagent", args).await.unwrap();
        assert!(is_error(&result));
        for want in contains {
            assert!(
                text(&result).contains(want),
                "{} lacks {want}",
                text(&result)
            );
        }
    }
}

#[tokio::test]
async fn empty_prompts_are_rejected_before_the_provider() {
    let _env = env_guard();
    for prompt in ["", " ", "\t\n  "] {
        let provider = Arc::new(StubProvider::new("deepseek"));
        let tools = tools(test_config(vec![("deepseek", provider.clone())]));
        let result = call(&tools, "subagent", json!({"prompt": prompt, "cwd": cwd()}))
            .await
            .unwrap();
        assert!(is_error(&result));
        assert!(text(&result).contains("prompt must not be empty"));
        assert!(provider.requests().is_empty());
    }
}

#[tokio::test]
async fn agents_md_precedes_developer_instructions() {
    let _env = env_guard();
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(cwd.path().join("AGENTS.md"), "REPO-RULE").unwrap();
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider.clone())]));
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "hello",
            "cwd": cwd.path(),
            "base-instructions": "BASE-INSTRUCTIONS",
            "developer-instructions": "DEV-INSTRUCTIONS"
        }),
    )
    .await
    .unwrap();
    assert!(!is_error(&result));
    let system = &provider.requests()[0].system;
    let base = system.find("BASE-INSTRUCTIONS").unwrap();
    let rule = system.find("REPO-RULE").unwrap();
    let dev = system.find("DEV-INSTRUCTIONS").unwrap();
    assert!(
        base == 0 && rule > base && dev > rule,
        "order wrong: {system}"
    );
}

#[tokio::test]
async fn agents_md_read_error_fails_the_call() {
    let _env = env_guard();
    let cwd = tempfile::tempdir().unwrap();
    std::fs::create_dir(cwd.path().join("AGENTS.md")).unwrap();
    let tools = tools(test_config(vec![(
        "deepseek",
        Arc::new(StubProvider::new("deepseek")),
    )]));
    let result = call(
        &tools,
        "subagent",
        json!({"prompt": "hello", "cwd": cwd.path()}),
    )
    .await
    .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("AGENTS.md"));
}

#[tokio::test]
async fn applies_model_and_instructions() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider.clone())]));
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "hello",
            "cwd": cwd(),
            "model": "m-pro",
            "reasoning-effort": "max",
            "base-instructions": "custom base",
            "developer-instructions": "custom developer",
            "config": {"unknown_future_option": true}
        }),
    )
    .await
    .unwrap();
    assert!(!is_error(&result));
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "m-pro");
    assert_eq!(requests[0].effort.as_deref(), Some("max"));
    assert_eq!(requests[0].system, "custom base\n\ncustom developer");
}

// ---------------------------------------------------------------------------
// Start/reply lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_and_reply_continue_the_same_thread() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns(
        "deepseek",
        vec![text_turn("hi"), text_turn("continued")],
    ));
    let tools = tools(test_config(vec![("deepseek", provider)]));
    let first = call(&tools, "subagent", json!({"prompt": "hello", "cwd": cwd()}))
        .await
        .unwrap();
    assert!(!is_error(&first));
    assert_eq!(text(&first), "hi");
    assert_eq!(structured(&first)["content"], "hi");
    let id = thread_id(&first).to_string();

    let reply = call(
        &tools,
        "subagent-reply",
        json!({"threadId": id, "prompt": "keep going"}),
    )
    .await
    .unwrap();
    assert!(!is_error(&reply));
    assert_eq!(text(&reply), "continued");
    assert_eq!(structured(&reply)["content"], "continued");
    assert_eq!(thread_id(&reply), id);
}

#[tokio::test]
async fn reply_reports_provider_and_model_on_success_and_error() {
    let _env = env_guard();
    for (turn, want_error) in [
        (text_turn("continued"), false),
        (Err("reply failed".to_string()), true),
    ] {
        let provider = Arc::new(StubProvider::with_turns(
            "alpha",
            vec![text_turn("started"), turn],
        ));
        let mut config = test_config(vec![
            ("alpha", provider.clone()),
            ("bravo", Arc::new(StubProvider::new("bravo"))),
        ]);
        config.providers.get_mut("alpha").unwrap().models = vec![
            subagent_mcp::config::ModelConfig {
                id: "m-fast".into(),
                description: String::new(),
            },
            subagent_mcp::config::ModelConfig {
                id: "m-pro".into(),
                description: String::new(),
            },
        ];
        let tools = tools(config);
        let start = call(
            &tools,
            "subagent",
            json!({
                "prompt": "start",
                "cwd": cwd(),
                "provider": "alpha",
                "model": "m-pro"
            }),
        )
        .await
        .unwrap();
        assert!(!is_error(&start));
        let id = thread_id(&start).to_string();
        let reply = call(
            &tools,
            "subagent-reply",
            json!({"threadId": id, "prompt": "continue"}),
        )
        .await
        .unwrap();
        assert_eq!(is_error(&reply), want_error);
        let structured = structured(&reply);
        assert_eq!(structured["threadId"], id);
        assert_eq!(structured["provider"], "alpha");
        assert_eq!(structured["model"], "m-pro");
        assert!(structured.get("auto").is_none());
    }
}

#[tokio::test]
async fn reply_unknown_thread_and_empty_prompt() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns(
        "deepseek",
        vec![text_turn("hello"), text_turn("still here")],
    ));
    let tools = tools(test_config(vec![("deepseek", provider.clone())]));
    let unknown = call(
        &tools,
        "subagent-reply",
        json!({"threadId": "not-a-thread", "prompt": "hello"}),
    )
    .await
    .unwrap();
    assert!(is_error(&unknown));
    assert!(text(&unknown).contains("unknown threadId"));

    let start = call(
        &tools,
        "subagent",
        json!({"prompt": "first prompt", "cwd": cwd()}),
    )
    .await
    .unwrap();
    let id = thread_id(&start).to_string();
    assert_eq!(provider.requests().len(), 1);
    for prompt in ["", " "] {
        let result = call(
            &tools,
            "subagent-reply",
            json!({"threadId": id, "prompt": prompt}),
        )
        .await
        .unwrap();
        assert!(is_error(&result));
        assert!(text(&result).contains("prompt must not be empty"));
    }
    assert_eq!(
        provider.requests().len(),
        1,
        "rejected replies must not run"
    );
    let reply = call(
        &tools,
        "subagent-reply",
        json!({"threadId": id, "prompt": "keep going"}),
    )
    .await
    .unwrap();
    assert_eq!(text(&reply), "still here");
}

#[tokio::test]
async fn reply_busy_keeps_the_thread_id() {
    let _env = env_guard();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let provider = Arc::new(
        StubProvider::with_repeat("deepseek", text_turn("first done"))
            .blocking(entered.clone(), release.clone()),
    );
    let tools = Arc::new(tools(test_config(vec![("deepseek", provider)])));

    let (ctx, mut rx) = call_context_with_progress(None);
    let start = {
        let tools = tools.clone();
        tokio::spawn(async move {
            tools
                .call("subagent", json!({"prompt": "first", "cwd": cwd()}), ctx)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("start did not reach the provider");

    // The task_started event names the thread while the run is still blocked.
    let id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
            if frame["method"] == "subagent/event"
                && let Some(id) = frame["params"]["threadId"].as_str()
            {
                return id.to_string();
            }
        }
    })
    .await
    .expect("start emitted no thread id");

    let (ctx, _rx) = call_context();
    let busy = tools
        .call(
            "subagent-reply",
            json!({"threadId": id, "prompt": "second"}),
            ctx,
        )
        .await
        .unwrap();
    assert!(is_error(&busy));
    assert_eq!(thread_id(&busy), id);
    assert!(text(&busy).contains("busy"));
    assert_eq!(structured(&busy)["content"], text(&busy));
    assert!(structured(&busy).get("provider").is_none());

    release.notify_one();
    let first = start.await.unwrap().unwrap();
    assert!(!is_error(&first));
    assert_eq!(text(&first), "first done");
}

#[tokio::test]
async fn cancellation_returns_thread_id_and_allows_reply() {
    let _env = env_guard();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let provider = Arc::new(
        StubProvider::with_repeat("deepseek", text_turn("recovered"))
            .blocking(entered.clone(), release.clone()),
    );
    let tools = Arc::new(tools(test_config(vec![("deepseek", provider)])));
    let cancel = CancellationToken::new();

    let start = {
        let tools = tools.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let (mut ctx, _rx) = call_context();
            ctx.cancel = cancel;
            tools
                .call("subagent", json!({"prompt": "hello", "cwd": cwd()}), ctx)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("start did not reach the provider");
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), start)
        .await
        .expect("cancelled call did not return")
        .unwrap()
        .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("context canceled"));
    let id = thread_id(&result).to_string();
    assert!(!id.is_empty());

    release.notify_one();
    let (ctx, _rx) = call_context();
    let reply = tools
        .call(
            "subagent-reply",
            json!({"threadId": id, "prompt": "continue"}),
            ctx,
        )
        .await
        .unwrap();
    assert!(!is_error(&reply), "{}", text(&reply));
    assert_eq!(text(&reply), "recovered");
}

#[tokio::test]
async fn turn_limit_preserves_thread_id() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_repeat(
        "deepseek",
        tool_turn(vec![subagent_mcp::provider::ToolCall {
            id: "call-forever".into(),
            name: "unknown_tool".into(),
            arguments: "{}".into(),
        }]),
    ));
    let tools = tools(test_config(vec![("deepseek", provider)]));
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "never finish",
            "cwd": cwd(),
            "config": {"max_turns": 1}
        }),
    )
    .await
    .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("turn limit reached (1)"));
    assert_eq!(structured(&result)["content"], text(&result));
    assert!(!thread_id(&result).is_empty());
}

#[tokio::test]
async fn excessive_max_turns_falls_back_to_the_default() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_repeat(
        "deepseek",
        tool_turn(vec![subagent_mcp::provider::ToolCall {
            id: "call-forever".into(),
            name: "unknown_tool".into(),
            arguments: "{}".into(),
        }]),
    ));
    let tools = tools(test_config(vec![("deepseek", provider.clone())]));
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "use a bounded turn count",
            "cwd": cwd(),
            "config": {"max_turns": 100001}
        }),
    )
    .await
    .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("turn limit reached (50)"));
    assert_eq!(provider.requests().len(), 50);
}

#[tokio::test]
async fn explicit_codex_provider_is_a_clear_internal_error() {
    let _env = env_guard();
    let config = Config {
        path: PathBuf::from("/test/config.toml"),
        providers: BTreeMap::from([(
            "codex".to_string(),
            ProviderConfig {
                api: Api::CodexAppServer,
                base_url: String::new(),
                env_key: None,
                key: None,
                command: None,
                default_model: "gpt-6.1-terra".into(),
                models: vec![subagent_mcp::config::ModelConfig {
                    id: "gpt-6.1-terra".into(),
                    description: String::new(),
                }],
                effort_map: BTreeMap::new(),
                max_output_tokens: None,
                max_nudges: None,
                extra_body: None,
            },
        )]),
        auto: None,
    };
    let tools = tools(config);
    let (ctx, _rx) = call_context();
    let error = tools
        .call("subagent", json!({"prompt": "hello", "cwd": cwd()}), ctx)
        .await
        .unwrap_err();
    assert_eq!(error.code, -32603);
    assert!(error.message.contains("codex-app-server"));
    assert!(error.message.contains("24B"));
}

// ---------------------------------------------------------------------------
// Provider resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_provider_may_be_omitted() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider.clone())]));
    let result = call(&tools, "subagent", json!({"prompt": "hi", "cwd": cwd()}))
        .await
        .unwrap();
    assert!(!is_error(&result));
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn provider_is_required_with_multiple_configured() {
    let _env = env_guard();
    let alpha = Arc::new(StubProvider::new("alpha"));
    let bravo = Arc::new(StubProvider::new("bravo"));
    let tools = tools(test_config(vec![
        ("alpha", alpha.clone()),
        ("bravo", bravo.clone()),
    ]));
    let result = call(&tools, "subagent", json!({"prompt": "hi", "cwd": cwd()}))
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("provider is required"));
    assert!(text(&result).ends_with("available: alpha, bravo"));
    assert!(alpha.requests().is_empty() && bravo.requests().is_empty());
}

#[tokio::test]
async fn selects_named_provider_and_rejects_unknown() {
    let _env = env_guard();
    let a = Arc::new(StubProvider::with_turns("a", vec![text_turn("from a")]));
    let b = Arc::new(StubProvider::with_turns("b", vec![text_turn("from b")]));
    let tools = tools(test_config(vec![("a", a.clone()), ("b", b.clone())]));
    let result = call(
        &tools,
        "subagent",
        json!({"prompt": "hi", "cwd": cwd(), "provider": "b"}),
    )
    .await
    .unwrap();
    assert!(!is_error(&result));
    assert_eq!(text(&result), "from b");
    assert!(a.requests().is_empty());
    assert_eq!(b.requests().len(), 1);

    let unknown = call(
        &tools,
        "subagent",
        json!({"prompt": "hi", "cwd": cwd(), "provider": "nope"}),
    )
    .await
    .unwrap();
    assert!(is_error(&unknown));
    for want in ["nope", "a", "b"] {
        assert!(text(&unknown).contains(want));
    }
}

#[tokio::test]
async fn missing_key_fails_only_that_provider() {
    let _env = env_guard();
    let a = Arc::new(StubProvider::with_turns("a", vec![text_turn("from a")]));
    let b = Arc::new(StubProvider::new("b"));
    let mut config = test_config(vec![("a", a), ("b", b)]);
    let missing = "TEST_MISSING_KEY_B_TASK24A";
    unsafe {
        std::env::remove_var(missing);
    }
    {
        let b = config.providers.get_mut("b").unwrap();
        b.key = None;
        b.env_key = Some(missing.into());
    }
    let tools = tools(config);

    let failed = call(
        &tools,
        "subagent",
        json!({"prompt": "hi", "cwd": cwd(), "provider": "b"}),
    )
    .await
    .unwrap();
    assert!(is_error(&failed));
    assert!(text(&failed).contains(missing));

    let ok = call(
        &tools,
        "subagent",
        json!({"prompt": "hi", "cwd": cwd(), "provider": "a"}),
    )
    .await
    .unwrap();
    assert!(!is_error(&ok));
}

#[tokio::test]
async fn model_and_effort_are_isolated_per_provider() {
    let _env = env_guard();
    let alpha = Arc::new(StubProvider::with_turns("alpha", vec![text_turn("ok")]));
    let bravo = Arc::new(StubProvider::with_turns("bravo", vec![text_turn("ok")]));
    let mut config = test_config(vec![("alpha", alpha.clone()), ("bravo", bravo.clone())]);
    {
        let bravo = config.providers.get_mut("bravo").unwrap();
        bravo.default_model = "b-only".into();
        bravo.models = vec![subagent_mcp::config::ModelConfig {
            id: "b-only".into(),
            description: String::new(),
        }];
        bravo.effort_map = BTreeMap::from([("xhigh".to_string(), "low".to_string())]);
    }
    let tools = tools(config);

    let bad = call(
        &tools,
        "subagent",
        json!({
            "prompt": "hi",
            "cwd": cwd(),
            "provider": "alpha",
            "model": "b-only"
        }),
    )
    .await
    .unwrap();
    assert!(is_error(&bad));
    assert!(text(&bad).ends_with("available models: m-fast, m-pro"));

    let ok = call(
        &tools,
        "subagent",
        json!({
            "prompt": "hi",
            "cwd": cwd(),
            "provider": "bravo",
            "reasoning-effort": "xhigh"
        }),
    )
    .await
    .unwrap();
    assert!(!is_error(&ok));
    let requests = bravo.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "b-only");
    assert_eq!(requests[0].effort.as_deref(), Some("low"));
    assert!(alpha.requests().is_empty());
}

// ---------------------------------------------------------------------------
// Config object: writable roots, budgets, nudges
// ---------------------------------------------------------------------------

#[tokio::test]
async fn writable_roots_are_validated() {
    let _env = env_guard();
    let file = cwd().join("file");
    std::fs::write(&file, b"").unwrap();
    let cases: Vec<Value> = vec![
        json!("/tmp"),
        json!([1]),
        json!(["rel/dir"]),
        json!([cwd().join("missing")]),
        json!([file]),
    ];
    for value in cases {
        let tools = tools(test_config(vec![(
            "deepseek",
            Arc::new(StubProvider::new("deepseek")),
        )]));
        let result = call(
            &tools,
            "subagent",
            json!({
                "prompt": "hello",
                "cwd": cwd(),
                "config": {"writable_roots": value}
            }),
        )
        .await
        .unwrap();
        assert!(is_error(&result));
        assert!(text(&result).contains("config.writable_roots"));
    }
}

#[tokio::test]
async fn writable_roots_are_accepted() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider)]));
    let root = tempfile::tempdir().unwrap();
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "hello",
            "cwd": cwd(),
            "config": {"writable_roots": [root.path()]}
        }),
    )
    .await
    .unwrap();
    assert!(!is_error(&result));
}

#[tokio::test]
async fn invalid_budget_is_rejected_before_a_session_exists() {
    let _env = env_guard();
    let tools = tools(test_config(vec![(
        "deepseek",
        Arc::new(StubProvider::new("deepseek")),
    )]));
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "hi",
            "cwd": cwd(),
            "config": {"max_total_tokens": "lots"}
        }),
    )
    .await
    .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("max_total_tokens"));
    assert!(result.get("structuredContent").is_none());
}

#[tokio::test]
async fn history_budget_stops_the_thread() {
    let _env = env_guard();
    let big_args = format!("{{\"path\":\"{}\"}}", "a".repeat(4000));
    let call_turn = tool_turn(vec![subagent_mcp::provider::ToolCall {
        id: "call-1".into(),
        name: "no_such_tool".into(),
        arguments: big_args,
    }]);
    let provider = Arc::new(StubProvider::with_turns(
        "deepseek",
        vec![call_turn.clone(), call_turn],
    ));
    let tools = tools(test_config(vec![("deepseek", provider.clone())]));
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "start",
            "cwd": cwd(),
            "config": {"max_history_bytes": 5000, "max_turns": 10}
        }),
    )
    .await
    .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("max_history_bytes"));
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn token_budget_stops_the_thread() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_repeat(
        "deepseek",
        Ok(TurnResult {
            tool_calls: vec![subagent_mcp::provider::ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"missing\"}".into(),
            }],
            usage: Some(subagent_mcp::provider::Usage {
                input: 60,
                output: 40,
                total: 100,
                ..Default::default()
            }),
            ..Default::default()
        }),
    ));
    let tools = tools(test_config(vec![("deepseek", provider.clone())]));
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "start",
            "cwd": cwd(),
            "config": {"max_total_tokens": 100, "max_turns": 10}
        }),
    )
    .await
    .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("max_total_tokens"));
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn run_deadline_bounds_a_run() {
    let _env = env_guard();
    // A provider that never observes cancellation still must not outlive the
    // runner's deadline.
    struct Stubborn(Arc<Notify>);
    #[async_trait]
    impl Provider for Stubborn {
        fn name(&self) -> &str {
            "stubborn"
        }
        async fn turn(
            &self,
            _ctx: RunContext,
            _req: TurnRequest,
            _on_delta: DeltaSink,
        ) -> Result<TurnResult, ProviderError> {
            self.0.notified().await;
            Ok(TurnResult {
                text: "late".into(),
                ..Default::default()
            })
        }
    }
    let entered = Arc::new(Notify::new());
    let stubborn = Arc::new(Stubborn(entered.clone()));
    let tools = tools(test_config(vec![("deepseek", stubborn)]));
    let started = std::time::Instant::now();
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "start",
            "cwd": cwd(),
            "config": {"run_deadline_seconds": 1}
        }),
    )
    .await
    .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("run deadline"));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn max_nudges_comes_from_provider_and_call_config() {
    let _env = env_guard();
    let cases: Vec<(Option<usize>, Option<Value>, usize, &str)> = vec![
        (None, None, 1, "Step 2: write the test."),
        (Some(1), None, 2, "All done."),
        (None, Some(json!({"max_nudges": 1})), 2, "All done."),
        (
            Some(1),
            Some(json!({"max_nudges": 0})),
            1,
            "Step 2: write the test.",
        ),
        (
            None,
            Some(json!({"max_nudges": 11})),
            1,
            "Step 2: write the test.",
        ),
        (
            None,
            Some(json!({"max_nudges": 1.5})),
            1,
            "Step 2: write the test.",
        ),
    ];
    for (provider_nudges, call_config, want_calls, want_text) in cases {
        let provider = Arc::new(StubProvider::with_turns(
            "deepseek",
            vec![text_turn("Step 2: write the test."), text_turn("All done.")],
        ));
        let mut config = test_config(vec![("deepseek", provider.clone())]);
        config.providers.get_mut("deepseek").unwrap().max_nudges = provider_nudges;
        let tools = tools(config);
        let mut args = json!({"prompt": "go", "cwd": cwd()});
        if let Some(call_config) = call_config {
            args["config"] = call_config;
        }
        let result = call(&tools, "subagent", args).await.unwrap();
        assert!(!is_error(&result), "{}", text(&result));
        assert_eq!(text(&result), want_text);
        assert_eq!(provider.requests().len(), want_calls);
    }
}

// ---------------------------------------------------------------------------
// Rollout session_meta
// ---------------------------------------------------------------------------

#[tokio::test]
async fn writes_session_meta_for_a_new_session() {
    let _env = env_guard();
    let home = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("CODEX_HOME", home.path());
        std::env::set_var("SUBAGENT_MCP_ROLLOUT", "");
    }
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider)]));
    let result = call(
        &tools,
        "subagent",
        json!({
            "prompt": "hello",
            "cwd": cwd(),
            "config": {"model_reasoning_effort": "xhigh"}
        }),
    )
    .await
    .unwrap();
    assert!(!is_error(&result));
    let id = thread_id(&result).to_string();

    let matches: Vec<PathBuf> = std::fs::read_dir(home.path().join("sessions"))
        .unwrap()
        .flat_map(|year| std::fs::read_dir(year.unwrap().path()).unwrap())
        .flat_map(|month| std::fs::read_dir(month.unwrap().path()).unwrap())
        .flat_map(|day| std::fs::read_dir(day.unwrap().path()).unwrap())
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().contains(&id))
        .collect();
    assert_eq!(matches.len(), 1, "rollout files = {matches:?}");
    let data = std::fs::read_to_string(&matches[0]).unwrap();
    let mut lines = data.lines();
    let meta: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let turn: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(meta["type"], "session_meta");
    assert_eq!(meta["payload"]["id"], id);
    assert_eq!(meta["payload"]["cli_version"], subagent_mcp::VERSION);
    assert_eq!(meta["payload"]["originator"], "subagent-mcp");
    assert_eq!(meta["payload"]["model_provider"], "deepseek");
    assert_eq!(meta["payload"]["source"], "mcp");
    assert!(
        meta["payload"]["timestamp"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    assert_eq!(turn["type"], "turn_context");
    assert_eq!(turn["payload"]["effort"], "xhigh");
    assert_eq!(turn["payload"]["effort_sent"], "max");
}

#[tokio::test]
async fn rollout_off_writes_nothing() {
    let _env = env_guard();
    let home = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("CODEX_HOME", home.path());
        std::env::set_var("SUBAGENT_MCP_ROLLOUT", "off");
    }
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider)]));
    let result = call(&tools, "subagent", json!({"prompt": "hello", "cwd": cwd()}))
        .await
        .unwrap();
    assert!(!is_error(&result));
    assert!(!home.path().join("sessions").exists());
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

fn frames(rx: &mut mpsc::Receiver<String>) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        out.push(serde_json::from_str(&frame).unwrap());
    }
    out
}

#[tokio::test]
async fn progress_notifications_follow_the_token() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns(
        "deepseek",
        vec![text_turn("line one\nline two")],
    ));
    let tools = tools(test_config(vec![("deepseek", provider)]));
    let (ctx, mut rx) = call_context_with_progress(Some(json!("tok-1")));
    let result = tools
        .call("subagent", json!({"prompt": "hello", "cwd": cwd()}), ctx)
        .await
        .unwrap();
    assert!(!is_error(&result));

    let all = frames(&mut rx);
    let events: Vec<String> = all
        .iter()
        .filter(|f| f["method"] == "subagent/event")
        .map(|f| f["params"]["msg"]["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        events,
        vec![
            "task_started",
            "provider_request",
            "agent_message",
            "task_complete"
        ]
    );
    let progress: Vec<&Value> = all
        .iter()
        .filter(|f| f["method"] == "notifications/progress")
        .collect();
    let messages: Vec<&str> = progress
        .iter()
        .map(|f| f["params"]["message"].as_str().unwrap())
        .collect();
    assert_eq!(messages, vec!["started", "agent: line one", "completed"]);
    for (index, frame) in progress.iter().enumerate() {
        assert_eq!(frame["params"]["progressToken"], "tok-1");
        assert_eq!(frame["params"]["progress"], (index + 1) as f64);
        assert!(frame["params"].get("total").is_none());
    }
}

#[tokio::test]
async fn no_progress_without_a_token() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider)]));
    let (ctx, mut rx) = call_context();
    tools
        .call("subagent", json!({"prompt": "hello", "cwd": cwd()}), ctx)
        .await
        .unwrap();
    let all = frames(&mut rx);
    assert!(all.iter().all(|f| f["method"] != "notifications/progress"));
    assert_eq!(
        all.iter()
            .filter(|f| f["method"] == "subagent/event")
            .count(),
        4
    );
}

#[tokio::test]
async fn concurrent_calls_keep_progress_separate() {
    let _env = env_guard();
    let barrier = Arc::new(Barrier::new(2));
    let provider =
        Arc::new(StubProvider::with_repeat("deepseek", text_turn("ok")).with_barrier(barrier));
    let tools = Arc::new(tools(test_config(vec![("deepseek", provider)])));

    let run = |token: &'static str| {
        let tools = tools.clone();
        async move {
            let (ctx, rx) = call_context_with_progress(Some(json!(token)));
            let result = tools
                .call("subagent", json!({"prompt": "hello", "cwd": cwd()}), ctx)
                .await
                .unwrap();
            (result, rx)
        }
    };
    let (a, b) = tokio::join!(run("tok-a"), run("tok-b"));
    assert!(!is_error(&a.0));
    assert!(!is_error(&b.0));

    let mut rx_a = a.1;
    let mut rx_b = b.1;
    let frames_a = frames(&mut rx_a);
    let frames_b = frames(&mut rx_b);
    for frame in &frames_a {
        if frame["method"] == "notifications/progress" {
            assert_eq!(frame["params"]["progressToken"], "tok-a");
        }
    }
    for frame in &frames_b {
        if frame["method"] == "notifications/progress" {
            assert_eq!(frame["params"]["progressToken"], "tok-b");
        }
    }
    assert!(
        frames_a
            .iter()
            .any(|f| f["method"] == "notifications/progress")
    );
    assert!(
        frames_b
            .iter()
            .any(|f| f["method"] == "notifications/progress")
    );
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

#[tokio::test]
async fn result_shapes_omit_empty_optionals() {
    let _env = env_guard();
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider)]));
    let result = call(&tools, "subagent", json!({"prompt": "hello", "cwd": cwd()}))
        .await
        .unwrap();
    assert!(!is_error(&result));
    assert_eq!(result["content"][0]["type"], "text");
    assert!(result.get("isError").is_none());
    let structured = structured(&result);
    assert!(structured.get("auto").is_none());
    assert_eq!(structured["provider"], "deepseek");
    assert_eq!(structured["model"], "m-fast");

    let error = call(&tools, "subagent", json!({"prompt": ""}))
        .await
        .unwrap();
    assert!(error.get("structuredContent").is_none());
    assert_eq!(error["isError"], true);
    assert_eq!(error["content"][0]["type"], "text");
}

#[tokio::test]
async fn request_id_is_read_from_meta_not_arguments() {
    let _env = env_guard();
    // The handler must not treat an `arguments`-level request id as a JSON-RPC
    // id; the only source is `CallContext.meta`. A start with such an argument
    // still runs normally.
    let provider = Arc::new(StubProvider::with_turns("deepseek", vec![text_turn("ok")]));
    let tools = tools(test_config(vec![("deepseek", provider)]));
    let (ctx, _rx) = call_context();
    let result = tools
        .call(
            "subagent",
            json!({
                "prompt": "hello",
                "cwd": cwd(),
                "requestId": "arguments-must-not-be-used"
            }),
            ctx,
        )
        .await
        .unwrap();
    assert!(!is_error(&result));
}

#[test]
fn output_schema_has_thread_id_and_content() {
    let _env = env_guard();
    let tools = tools(test_config(vec![(
        "deepseek",
        Arc::new(StubProvider::new("deepseek")),
    )]));
    for definition in tools.definitions() {
        assert_eq!(definition.output_schema["type"], "object");
        let properties = definition.output_schema["properties"].as_object().unwrap();
        for want in ["threadId", "content"] {
            assert!(properties.contains_key(want));
        }
    }
    let reply = tools
        .definitions()
        .into_iter()
        .find(|d| d.name == "subagent-reply")
        .unwrap();
    assert_eq!(
        reply.output_schema["properties"].as_object().unwrap().len(),
        4
    );
}

#[test]
fn annotations_match_go_defaults() {
    let _env = env_guard();
    let tools = tools(test_config(vec![(
        "deepseek",
        Arc::new(StubProvider::new("deepseek")),
    )]));
    for definition in tools.definitions() {
        assert_eq!(definition.annotations.as_ref(), Some(&annotations()));
    }
}
