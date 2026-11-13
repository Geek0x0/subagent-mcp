//! Task 24A: auto selection for the three HTTP APIs.
//!
//! Ports the non-Codex half of Go `internal/server/auto_test.go` (v0.13.1):
//! candidate filtering, decision-model calls, skipped decisions, metadata
//! omission rules, run-deadline propagation, and dispatch through the factory.
//! Codex candidates are deliberately unavailable in 24A; Task 24B adds them.

#![allow(clippy::await_holding_lock)] // process-global env lock, intentionally held in current-thread tests

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use subagent_mcp::agent::SessionManager;
use subagent_mcp::config::{Api, AutoConfig, Config, ModelConfig, ProviderConfig};
use subagent_mcp::mcp::auto::resolve_auto;
use subagent_mcp::mcp::jsonrpc::RequestId;
use subagent_mcp::mcp::progress::ProgressSink;
use subagent_mcp::mcp::server::{CallContext, ToolHandler};
use subagent_mcp::mcp::tools::SubagentTools;
use subagent_mcp::provider::{
    DeltaSink, Provider, ProviderError, TurnRequest, TurnResult, factory,
};
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Environment serialization
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_guard() -> MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("SUBAGENT_MCP_ROLLOUT", "off");
    }
    guard
}

// ---------------------------------------------------------------------------
// Test provider registry (factory dispatcher under "responses")
// ---------------------------------------------------------------------------

static STUBS: LazyLock<Mutex<HashMap<String, Arc<dyn Provider>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_STUB: AtomicUsize = AtomicUsize::new(0);

fn ensure_test_factory() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        factory::register(
            Api::Responses.as_str(),
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

struct StubProvider {
    name: String,
    turns: Mutex<VecDeque<StubTurn>>,
    repeat: Option<StubTurn>,
    requests: Mutex<Vec<TurnRequest>>,
}

impl StubProvider {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            turns: Mutex::new(VecDeque::new()),
            repeat: None,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn with_turns(name: &str, turns: Vec<StubTurn>) -> Self {
        let mut provider = Self::new(name);
        provider.turns = Mutex::new(turns.into());
        provider
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
// Config helpers
// ---------------------------------------------------------------------------

fn provider_config(base_url: String) -> ProviderConfig {
    ProviderConfig {
        api: Api::Responses,
        base_url,
        env_key: None,
        key: Some("test-key".into()),
        command: None,
        default_model: "m-fast".into(),
        models: vec![
            ModelConfig {
                id: "m-fast".into(),
                description: "fast".into(),
            },
            ModelConfig {
                id: "m-pro".into(),
                description: String::new(),
            },
        ],
        effort_map: BTreeMap::new(),
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

fn auto_config(base_url: &str) -> AutoConfig {
    AutoConfig {
        base_url: base_url.into(),
        url: String::new(),
        env_key: None,
        key: Some("jev-key".into()),
        model: "jev-latest".into(),
        timeout_seconds: 5,
    }
}

fn tools(config: Config) -> SubagentTools {
    ensure_test_factory();
    SubagentTools::new(
        "subagent".into(),
        Arc::new(config),
        Arc::new(SessionManager::new()),
    )
}

/// A working directory that outlives the expression that created it.
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

fn call_context() -> (CallContext, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel(64);
    (
        CallContext {
            cancel: CancellationToken::new(),
            request_id: Some(RequestId::new(json!(1))),
            meta: Some(json!({"subagent-mcp/requestId": "int64:1"})),
            progress: Some(ProgressSink::new(None, tx, Arc::new(AtomicU64::new(0)))),
            approver: Arc::new(Allow),
        },
        rx,
    )
}

fn plain_ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn is_error(result: &Value) -> bool {
    result["isError"].as_bool().unwrap_or(false)
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

fn structured(result: &Value) -> &serde_json::Map<String, Value> {
    result["structuredContent"]
        .as_object()
        .expect("structuredContent")
}

// ---------------------------------------------------------------------------
// Local HTTP mock
// ---------------------------------------------------------------------------

struct RecordedRequest {
    body: Vec<u8>,
}

impl RecordedRequest {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

enum Reply {
    Response { status: u16, body: String },
    Slow { delay: Duration, body: String },
}

struct Mock {
    base: String,
    connections: Arc<AtomicUsize>,
    request: oneshot::Receiver<RecordedRequest>,
}

async fn spawn_mock(reply: Reply) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = oneshot::channel();
    let conn_count = connections.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        conn_count.fetch_add(1, Ordering::SeqCst);
        let request = read_http_request(&mut socket).await;
        let _ = tx.send(request);
        let (status, body, delay) = match reply {
            Reply::Response { status, body } => (status, body, Duration::ZERO),
            Reply::Slow { delay, body } => (200, body, delay),
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let reason = if (200..300).contains(&status) {
            "OK"
        } else {
            "Error"
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });
    Mock {
        base: format!("http://{addr}"),
        connections,
        request: rx,
    }
}

async fn recorded(mock: Mock) -> (RecordedRequest, Arc<AtomicUsize>) {
    let request = tokio::time::timeout(Duration::from_secs(5), mock.request)
        .await
        .expect("no request reached the mock server")
        .expect("mock server dropped the request channel");
    (request, mock.connections)
}

fn find_crlfcrlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

async fn read_http_request(socket: &mut TcpStream) -> RecordedRequest {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = socket.read(&mut tmp).await.unwrap();
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_crlfcrlf(&buf) {
            break pos;
        }
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut content_length = 0usize;
    for line in header_text.split("\r\n") {
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    while buf.len().saturating_sub(header_end) < content_length {
        let n = socket.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    RecordedRequest {
        body: buf[header_end..buf.len().min(header_end + content_length)].to_vec(),
    }
}

fn auto_response(choice: &str, confidence: f64) -> String {
    json!({
        "model": "jev-1.14.0",
        "answers": {
            "model": {
                "type": "choice",
                "choice": choice,
                "confidence": confidence,
                "probabilities": {}
            }
        },
        "usage": {"input_tokens": 42, "output_tokens": 3}
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// resolve_auto
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_auto_selects_provider_from_jev() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: json!({
            "model": "jev-1.14.0",
            "answers": {
                "model": {
                    "type": "choice",
                    "choice": "c4",
                    "confidence": 0.9,
                    "probabilities": {"c3": 0.05, "c4": 0.95}
                }
            },
            "usage": {"input_tokens": 42, "output_tokens": 3}
        })
        .to_string(),
    })
    .await;
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    config.auto = Some(auto_config(&mock.base));
    let manager = SessionManager::new();

    let selection = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "fix the parser",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap();
    assert_eq!(selection.provider, "bravo");
    assert_eq!(selection.model, "m-pro");
    assert!(!selection.skipped);
    assert_eq!(selection.judge_model.as_deref(), Some("jev-1.14.0"));
    assert_eq!(selection.confidence, Some(0.9));
    assert_eq!(selection.input_tokens, Some(42));
    assert_eq!(selection.output_tokens, Some(3));
    assert_eq!(selection.probabilities.get("bravo/m-pro"), Some(&0.95));

    let (request, connections) = recorded(mock).await;
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    let criteria = &request.json()["questions"]["model"]["criteria"];
    for key in ["c1", "c2", "c3", "c4"] {
        assert!(criteria.get(key).is_some(), "criteria missing {key}");
    }
}

#[tokio::test]
async fn single_candidate_skips_jev() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("c1", 0.9),
    })
    .await;
    let mut config = test_config(vec![("alpha", Arc::new(StubProvider::new("alpha")))]);
    config.providers.get_mut("alpha").unwrap().models = vec![ModelConfig {
        id: "m-fast".into(),
        description: String::new(),
    }];
    config.auto = Some(auto_config(&mock.base));
    let manager = SessionManager::new();

    let selection = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap();
    assert_eq!(selection.provider, "alpha");
    assert_eq!(selection.model, "m-fast");
    assert!(selection.skipped);
    let meta = selection.meta();
    assert_eq!(meta["skipped"], true);
    assert!(meta.get("judge_model").is_none());
    assert!(meta.get("confidence").is_none());
    assert!(meta.get("probabilities").is_none());
    assert!(meta.get("input_tokens").is_none());
    assert!(meta.get("output_tokens").is_none());
    assert!(meta.get("omitted").is_none());
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn omits_provider_without_key() {
    let _env = env_guard();
    let missing = "TEST_AUTO_MISSING_BRAVO_TASK24A";
    unsafe {
        std::env::remove_var(missing);
    }
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("c1", 0.8),
    })
    .await;
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    {
        let bravo = config.providers.get_mut("bravo").unwrap();
        bravo.key = None;
        bravo.env_key = Some(missing.into());
    }
    config.auto = Some(auto_config(&mock.base));
    let manager = SessionManager::new();

    let selection = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap();
    assert_eq!(selection.provider, "alpha");
    let meta = selection.meta();
    let omitted = meta["omitted"].as_array().unwrap();
    assert_eq!(omitted.len(), 1);
    assert!(omitted[0].as_str().unwrap().contains("bravo"));
    assert!(omitted[0].as_str().unwrap().contains(missing));

    let (request, _) = recorded(mock).await;
    let criteria = &request.json()["questions"]["model"]["criteria"];
    for description in criteria.as_object().unwrap().values() {
        assert!(
            !description.as_str().unwrap().starts_with("provider bravo "),
            "criteria offered an unavailable provider: {description}"
        );
    }
}

#[tokio::test]
async fn no_reachable_provider_fails() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("c1", 0.8),
    })
    .await;
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    for (name, missing) in [
        ("alpha", "TEST_AUTO_MISSING_ALPHA_TASK24A"),
        ("bravo", "TEST_AUTO_MISSING_BRAVO2_TASK24A"),
    ] {
        unsafe {
            std::env::remove_var(missing);
        }
        let provider = config.providers.get_mut(name).unwrap();
        provider.key = None;
        provider.env_key = Some(missing.into());
    }
    config.auto = Some(auto_config(&mock.base));
    let manager = SessionManager::new();

    let error = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap_err();
    for want in [
        "no provider is usable",
        "TEST_AUTO_MISSING_ALPHA_TASK24A",
        "TEST_AUTO_MISSING_BRAVO2_TASK24A",
    ] {
        assert!(
            error.message.contains(want),
            "{} lacks {want}",
            error.message
        );
    }
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejects_provider_argument() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("c1", 0.8),
    })
    .await;
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    config.auto = Some(auto_config(&mock.base));
    // `resolve_auto` has no provider argument in the Rust interface; the
    // handler enforces omission before calling it. This test pins that the
    // handler rejects a non-empty provider when model is "auto".
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({"prompt": "task", "cwd": cwd(), "model": "auto", "provider": "alpha"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("provider must be omitted"));
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn requires_auto_section() {
    let _env = env_guard();
    let config = test_config(vec![("alpha", Arc::new(StubProvider::new("alpha")))]);
    let manager = SessionManager::new();
    let error = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("[auto]"), "{}", error.message);
}

#[tokio::test]
async fn requires_jev_key_when_choosing() {
    let _env = env_guard();
    let missing = "TEST_AUTO_KEY_MISSING_TASK24A";
    unsafe {
        std::env::remove_var(missing);
    }
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("c1", 0.8),
    })
    .await;
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    config.auto = Some(AutoConfig {
        base_url: mock.base.clone(),
        url: String::new(),
        env_key: Some(missing.into()),
        key: None,
        model: "jev-latest".into(),
        timeout_seconds: 5,
    });
    let manager = SessionManager::new();
    let error = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap_err();
    for want in [missing, "auto.env_key"] {
        assert!(
            error.message.contains(want),
            "{} lacks {want}",
            error.message
        );
    }
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reports_jev_failure() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 500,
        body: r#"{"detail":"upstream failed"}"#.into(),
    })
    .await;
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    config.auto = Some(auto_config(&mock.base));
    let manager = SessionManager::new();
    let error = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("auto model selection"));
    assert!(error.message.contains("500"));
}

#[tokio::test]
async fn rejects_unknown_choice() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("nope", 0.8),
    })
    .await;
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    config.auto = Some(auto_config(&mock.base));
    let manager = SessionManager::new();
    let error = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap_err();
    for want in ["unknown choice", "nope"] {
        assert!(
            error.message.contains(want),
            "{} lacks {want}",
            error.message
        );
    }
}

#[tokio::test]
async fn candidate_descriptions_use_defaults_and_config() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("c1", 0.8),
    })
    .await;
    let models = vec![
        ModelConfig {
            id: "claude-opus-5".into(),
            description: String::new(),
        },
        ModelConfig {
            id: "gateway/anthropic/claude-opus-5".into(),
            description: String::new(),
        },
        ModelConfig {
            id: "anthropic/claude-sonnet-5".into(),
            description: "my configured description".into(),
        },
        ModelConfig {
            id: "unknown/model".into(),
            description: String::new(),
        },
        ModelConfig {
            id: "glm-5.3".into(),
            description: String::new(),
        },
    ];
    let mut config = test_config(vec![("alpha", Arc::new(StubProvider::new("alpha")))]);
    config.providers.get_mut("alpha").unwrap().models = models;
    config.auto = Some(auto_config(&mock.base));
    let manager = SessionManager::new();
    let _ = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap();

    let (request, _) = recorded(mock).await;
    let criteria = request.json()["questions"]["model"]["criteria"]
        .as_object()
        .unwrap()
        .clone();
    let description = |key: &str| criteria[key].as_str().unwrap().to_string();
    assert!(description("c1").contains("Anthropic flagship"));
    assert!(description("c2").contains("Anthropic flagship"));
    assert!(description("c3").ends_with("my configured description"));
    assert_eq!(
        description("c4"),
        "provider alpha (api responses); model unknown/model"
    );
    assert!(description("c5").contains("Z.ai latest GLM"));
}

fn codex_provider_config() -> ProviderConfig {
    ProviderConfig {
        api: Api::CodexAppServer,
        base_url: String::new(),
        env_key: None,
        key: None,
        command: None,
        default_model: "gpt-6.1-terra".into(),
        models: vec![ModelConfig {
            id: "gpt-6.1-terra".into(),
            description: String::new(),
        }],
        effort_map: BTreeMap::new(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    }
}

#[tokio::test]
async fn codex_single_candidate_is_skipped_without_a_judge() {
    let _env = env_guard();
    let mut config = Config {
        path: PathBuf::from("/test/config.toml"),
        providers: BTreeMap::new(),
        auto: None,
    };
    config
        .providers
        .insert("codex".into(), codex_provider_config());
    config.auto = Some(auto_config("http://127.0.0.1:1"));
    // Constructing the tools registers the Codex adapter, making the
    // configured candidate available.
    let _tools = tools(config.clone());
    let manager = SessionManager::new();
    let selection = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap();
    assert_eq!(selection.provider, "codex");
    assert_eq!(selection.model, "gpt-6.1-terra");
    assert!(selection.skipped);
    assert!(selection.judge_model.is_none());
    assert!(selection.confidence.is_none());
    assert!(selection.input_tokens.is_none());
    assert!(selection.output_tokens.is_none());
    assert!(selection.omitted.is_empty());
}

#[tokio::test]
async fn codex_candidate_can_be_chosen_by_the_judge() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: json!({
            "model": "jev-1.14.0",
            "answers": {
                "model": {
                    "type": "choice",
                    "choice": "c3",
                    "confidence": 0.87,
                    "probabilities": {"c1": 0.05, "c2": 0.08, "c3": 0.87}
                }
            },
            "usage": {"input_tokens": 40, "output_tokens": 4}
        })
        .to_string(),
    })
    .await;
    let mut config = test_config(vec![(
        "alpha",
        Arc::new(StubProvider::with_turns("alpha", vec![text_turn("ok")])),
    )]);
    config
        .providers
        .insert("codex".into(), codex_provider_config());
    config.auto = Some(auto_config(&mock.base));
    let _tools = tools(config.clone());
    let manager = SessionManager::new();
    let selection = resolve_auto(
        &plain_ctx(),
        &config,
        &manager,
        "task",
        &cwd().to_path_buf(),
    )
    .await
    .unwrap();
    assert_eq!(selection.provider, "codex");
    assert_eq!(selection.model, "gpt-6.1-terra");
    assert!(!selection.skipped);
    assert_eq!(selection.judge_model.as_deref(), Some("jev-1.14.0"));
    assert_eq!(selection.confidence, Some(0.87));
    assert_eq!(selection.input_tokens, Some(40));
    assert_eq!(selection.output_tokens, Some(4));
    assert!(selection.omitted.is_empty());

    let (request, _) = recorded(mock).await;
    let criteria = request.json()["questions"]["model"]["criteria"]
        .as_object()
        .unwrap()
        .clone();
    assert!(
        criteria
            .values()
            .any(|description| description.as_str().unwrap().contains("codex")),
        "criteria = {criteria:?}"
    );
}

// ---------------------------------------------------------------------------
// handle_start auto dispatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handle_start_auto_dispatches_chosen_model() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: json!({
            "model": "jev-1.14.0",
            "answers": {
                "model": {
                    "type": "choice",
                    "choice": "c2",
                    "confidence": 0.9,
                    "probabilities": {"c1": 0.1, "c2": 0.9}
                }
            },
            "usage": {"input_tokens": 42, "output_tokens": 3}
        })
        .to_string(),
    })
    .await;
    let alpha = Arc::new(StubProvider::new("alpha"));
    let bravo = Arc::new(StubProvider::with_turns(
        "bravo",
        vec![text_turn("chosen result")],
    ));
    let mut config = test_config(vec![("alpha", alpha.clone()), ("bravo", bravo.clone())]);
    for (name, model) in [("alpha", "m-fast"), ("bravo", "m-pro")] {
        let provider = config.providers.get_mut(name).unwrap();
        provider.models = vec![ModelConfig {
            id: model.into(),
            description: String::new(),
        }];
        provider.default_model = model.into();
    }
    config.auto = Some(auto_config(&mock.base));
    let tools = tools(config);

    let result = tools
        .call(
            "subagent",
            json!({"prompt": "fix the parser", "cwd": cwd(), "model": "auto"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(!is_error(&result), "{}", text(&result));
    assert_eq!(text(&result), "chosen result");
    assert!(alpha.requests().is_empty());
    assert_eq!(bravo.requests().len(), 1);

    let structured = structured(&result);
    assert_eq!(structured["provider"], "bravo");
    assert_eq!(structured["model"], "m-pro");
    let auto = structured["auto"].as_object().unwrap();
    assert_eq!(auto["judge_model"], "jev-1.14.0");
    assert_eq!(auto["confidence"], 0.9);
    assert_eq!(auto["input_tokens"], 42);
    assert_eq!(auto["output_tokens"], 3);
    let probabilities = auto["probabilities"].as_object().unwrap();
    assert_eq!(probabilities["alpha/m-fast"], 0.1);
    assert_eq!(probabilities["bravo/m-pro"], 0.9);
    assert!(auto.get("omitted").is_none());
    assert!(auto.get("skipped").is_none());
}

#[tokio::test]
async fn handle_start_auto_single_candidate_needs_no_jev_key() {
    let _env = env_guard();
    let alpha = Arc::new(StubProvider::with_turns("alpha", vec![text_turn("single")]));
    let mut config = test_config(vec![("alpha", alpha)]);
    config.providers.get_mut("alpha").unwrap().models = vec![ModelConfig {
        id: "m-fast".into(),
        description: String::new(),
    }];
    config.auto = Some(AutoConfig {
        base_url: "http://127.0.0.1:1".into(),
        url: String::new(),
        env_key: Some("TEST_AUTO_KEY_UNSET_SINGLE_TASK24A".into()),
        key: None,
        model: "jev-latest".into(),
        timeout_seconds: 5,
    });
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({"prompt": "task", "cwd": cwd(), "model": "auto"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(!is_error(&result), "{}", text(&result));
    let structured = structured(&result);
    assert_eq!(structured["provider"], "alpha");
    assert_eq!(structured["model"], "m-fast");
    assert_eq!(structured["auto"], json!({"skipped": true}));
    let auto = structured["auto"].as_object().unwrap();
    assert_eq!(auto["skipped"], true);
    for key in [
        "judge_model",
        "confidence",
        "probabilities",
        "input_tokens",
        "output_tokens",
        "omitted",
    ] {
        assert!(auto.get(key).is_none(), "unexpected {key}: {auto:?}");
    }
}

#[tokio::test]
async fn handle_start_auto_skipped_metadata_preserves_omitted_providers() {
    let _env = env_guard();
    let home = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("CODEX_HOME", home.path());
        std::env::set_var("SUBAGENT_MCP_ROLLOUT", "");
    }
    let missing = "TEST_AUTO_MISSING_BRAVO3_TASK24A";
    unsafe {
        std::env::remove_var(missing);
    }
    let alpha = Arc::new(StubProvider::with_turns("alpha", vec![text_turn("single")]));
    let mut config = test_config(vec![
        ("alpha", alpha),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    config.providers.get_mut("alpha").unwrap().models = vec![ModelConfig {
        id: "m-fast".into(),
        description: String::new(),
    }];
    {
        let bravo = config.providers.get_mut("bravo").unwrap();
        bravo.key = None;
        bravo.env_key = Some(missing.into());
    }
    config.auto = Some(AutoConfig {
        base_url: "http://127.0.0.1:1".into(),
        url: String::new(),
        env_key: None,
        key: Some("jev-key".into()),
        model: "jev-latest".into(),
        timeout_seconds: 5,
    });
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({"prompt": "task", "cwd": cwd(), "model": "auto"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(!is_error(&result), "{}", text(&result));
    let id = structured(&result)["threadId"]
        .as_str()
        .unwrap()
        .to_string();
    let expected = json!({
        "skipped": true,
        "omitted": ["bravo: environment variable TEST_AUTO_MISSING_BRAVO3_TASK24A (providers.bravo.env_key) must be set to the API key"]
    });
    assert_eq!(structured(&result)["auto"], expected);

    let path = find_rollout(home.path(), &id);
    let data = std::fs::read_to_string(path).unwrap();
    let first: Value = serde_json::from_str(data.lines().next().unwrap()).unwrap();
    let selection = first["payload"]["model_selection"].as_object().unwrap();
    let mut expected_rollout = expected.clone();
    expected_rollout["provider"] = json!("alpha");
    expected_rollout["model"] = json!("m-fast");
    assert_eq!(first["payload"]["model_selection"], expected_rollout);
    assert_eq!(selection["omitted"], expected["omitted"]);
    assert_eq!(selection["skipped"], true);
    for key in [
        "judge_model",
        "confidence",
        "probabilities",
        "input_tokens",
        "output_tokens",
    ] {
        assert!(selection.get(key).is_none(), "unexpected {key}");
    }
}

fn find_rollout(home: &std::path::Path, id: &str) -> PathBuf {
    for year in std::fs::read_dir(home.join("sessions")).unwrap() {
        for month in std::fs::read_dir(year.unwrap().path()).unwrap() {
            for day in std::fs::read_dir(month.unwrap().path()).unwrap() {
                for entry in std::fs::read_dir(day.unwrap().path()).unwrap() {
                    let path = entry.unwrap().path();
                    if path.to_string_lossy().contains(id) {
                        return path;
                    }
                }
            }
        }
    }
    panic!("no rollout file for {id}");
}

#[tokio::test]
async fn auto_failure_creates_no_session() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 500,
        body: r#"{"detail":"upstream failed"}"#.into(),
    })
    .await;
    let alpha = Arc::new(StubProvider::new("alpha"));
    let bravo = Arc::new(StubProvider::new("bravo"));
    let mut config = test_config(vec![("alpha", alpha.clone()), ("bravo", bravo.clone())]);
    config.auto = Some(auto_config(&mock.base));
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({"prompt": "task", "cwd": cwd(), "model": "auto"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(is_error(&result));
    for want in ["auto model selection", "500"] {
        assert!(
            text(&result).contains(want),
            "{} lacks {want}",
            text(&result)
        );
    }
    assert!(alpha.requests().is_empty());
    assert!(bravo.requests().is_empty());
    assert!(result.get("structuredContent").is_none());
}

#[tokio::test]
async fn auto_execution_error_keeps_metadata() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("c4", 0.9),
    })
    .await;
    let bravo = Arc::new(StubProvider::with_turns("bravo", vec![Err("boom".into())]));
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", bravo),
    ]);
    config.auto = Some(auto_config(&mock.base));
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({"prompt": "task", "cwd": cwd(), "model": "auto"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(is_error(&result));
    let structured = structured(&result);
    assert!(!structured["threadId"].as_str().unwrap().is_empty());
    assert_eq!(structured["provider"], "bravo");
    assert_eq!(structured["model"], "m-pro");
    assert!(structured.get("auto").is_some());
}

#[tokio::test]
async fn auto_does_not_call_jev_on_invalid_args() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("c1", 0.9),
    })
    .await;
    let mut config = test_config(vec![("alpha", Arc::new(StubProvider::new("alpha")))]);
    config.auto = Some(auto_config(&mock.base));
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({"prompt": "task", "cwd": cwd(), "model": "auto", "sandbox": "bogus"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("read-only"));
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn auto_validates_writable_roots_before_jev() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Response {
        status: 200,
        body: auto_response("c1", 0.9),
    })
    .await;
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    config.auto = Some(auto_config(&mock.base));
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({
                "prompt": "task",
                "cwd": cwd(),
                "model": "auto",
                "config": {"writable_roots": ["relative/path"]}
            }),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("writable_roots"));
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn auto_honors_run_deadline_during_jev_selection() {
    let _env = env_guard();
    let mock = spawn_mock(Reply::Slow {
        delay: Duration::from_millis(1100),
        body: auto_response("c1", 0.9),
    })
    .await;
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    let mut auto = auto_config(&mock.base);
    auto.timeout_seconds = 3;
    config.auto = Some(auto);
    let tools = tools(config);
    let started = Instant::now();
    let result = tools
        .call(
            "subagent",
            json!({
                "prompt": "task",
                "cwd": cwd(),
                "model": "auto",
                "config": {"run_deadline_seconds": 1}
            }),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("deadline"), "{}", text(&result));
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(mock.connections.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Explicit provider behavior under auto config
// ---------------------------------------------------------------------------

#[tokio::test]
async fn explicit_keeps_provider_error_first() {
    let _env = env_guard();
    let config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({"prompt": "task", "cwd": cwd(), "provider": "bogus", "sandbox": "bogus"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("unknown provider"));
    assert!(!text(&result).contains("read-only"));
}

#[tokio::test]
async fn explicit_result_reports_provider_and_model() {
    let _env = env_guard();
    let alpha = Arc::new(StubProvider::with_turns("alpha", vec![text_turn("ok")]));
    let config = test_config(vec![
        ("alpha", alpha),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({"prompt": "task", "cwd": cwd(), "provider": "alpha", "model": "m-pro"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(!is_error(&result));
    let structured = structured(&result);
    assert_eq!(structured["provider"], "alpha");
    assert_eq!(structured["model"], "m-pro");
    assert!(structured.get("auto").is_none());
}

#[tokio::test]
async fn explicit_model_still_needs_provider_under_auto() {
    let _env = env_guard();
    let mut config = test_config(vec![
        ("alpha", Arc::new(StubProvider::new("alpha"))),
        ("bravo", Arc::new(StubProvider::new("bravo"))),
    ]);
    config.auto = Some(auto_config("https://api.typesafe.ai"));
    let tools = tools(config);
    let result = tools
        .call(
            "subagent",
            json!({"prompt": "task", "cwd": cwd(), "model": "m-pro"}),
            call_context().0,
        )
        .await
        .unwrap();
    assert!(is_error(&result));
    assert!(text(&result).contains("provider is required"));
}

#[test]
fn factory_new_works_for_the_http_apis() {
    let _env = env_guard();
    // Constructing the tools registers chat-completions, responses, messages,
    // and Codex. This binary overrides responses with a test dispatcher, so
    // assert the real wrappers that remain; `mcp_tools.rs` constructs the real
    // `Responses::new` and exercises the registered Codex adapter.
    let _tools = tools(test_config(vec![(
        "alpha",
        Arc::new(StubProvider::new("alpha")),
    )]));
    let key = Some("test-key".to_string());
    let chat = ProviderConfig {
        api: Api::ChatCompletions,
        base_url: "https://example.invalid/v1".into(),
        env_key: None,
        key: key.clone(),
        command: None,
        default_model: "m".into(),
        models: vec![],
        effort_map: BTreeMap::new(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    };
    let messages = ProviderConfig {
        api: Api::Messages,
        ..chat.clone()
    };
    assert!(factory::new("chat", &chat, "test-key").is_ok());
    assert!(factory::new("messages", &messages, "test-key").is_ok());
    assert!(factory::lookup(Api::ChatCompletions.as_str()).is_some());
    assert!(factory::lookup(Api::Messages.as_str()).is_some());
    assert!(factory::lookup(Api::Responses.as_str()).is_some());
    assert!(factory::lookup(Api::CodexAppServer.as_str()).is_some());
}
