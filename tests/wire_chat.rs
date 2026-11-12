//! Task 10: chat-completions adapter wire tests.
//!
//! Ports the observable contracts from Go `internal/provider/chatcompletions/*_test.go`
//! (`adapter_test.go`, `client_test.go`, `retry_test.go`, `drain_test.go`,
//! `routing_test.go`, `shared_transport_test.go`, `env_credentials_test.go`).
//! The exact subagent-mcp-authored strings (finish-reason errors, tool-call
//! errors, schema errors, missing-key error) are asserted verbatim; errors that
//! originate in the Go SDK or the standard library are matched semantically.
//!
//! Framing follows go-openai v1.42.0 `stream_reader.go`: every line matching
//! `^data:\s*` is one event, blank and non-`data:` lines are not events (with
//! the 300-empty-message limit), an exact `[DONE]` ends the stream, and there is
//! no fixed scanner limit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json, value::RawValue};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::provider::chat_completions::{
    self, EMPTY_MESSAGES_LIMIT, MAX_ATTEMPTS, SetupFailure,
};
use subagent_mcp::provider::{
    ChatCompletions, DeltaSink, Message, ModelLister, Provider, ProviderError, Role, ToolCall,
    ToolSpec, TurnRequest, TurnResult, Usage,
};
use subagent_mcp::util::run::RunContext;

// ---------------------------------------------------------------------------
// Scripted HTTP mock (stand-in for Go's testutil.FakeChat / httptest.Server)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("recorded request body is JSON")
    }

    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Clone, Debug)]
enum Reply {
    /// A streamed SSE response. `keep_alive` sends an exact Content-Length so
    /// the pooled connection can be reused; otherwise the socket closes.
    Sse {
        body: Vec<u8>,
        keep_alive: bool,
        hold_open: bool,
    },
    /// A non-2xx response body for the stream endpoint.
    Status { status: u16, body: Vec<u8> },
    /// The `/models` endpoint reply.
    Models { status: u16, body: Vec<u8> },
}

impl Reply {
    fn sse(body: impl Into<Vec<u8>>) -> Self {
        Reply::Sse {
            body: body.into(),
            keep_alive: false,
            hold_open: false,
        }
    }

    fn sse_keep_alive(body: impl Into<Vec<u8>>) -> Self {
        Reply::Sse {
            body: body.into(),
            keep_alive: true,
            hold_open: false,
        }
    }

    fn sse_hold_open(body: impl Into<Vec<u8>>) -> Self {
        Reply::Sse {
            body: body.into(),
            keep_alive: false,
            hold_open: true,
        }
    }

    fn status(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Reply::Status {
            status,
            body: body.into(),
        }
    }

    fn models_ok(ids: &[&str]) -> Self {
        let data: Vec<Value> = ids
            .iter()
            .map(|id| json!({"id": id, "object": "model"}))
            .collect();
        Reply::Models {
            status: 200,
            body: json!({"object": "list", "data": data})
                .to_string()
                .into_bytes(),
        }
    }

    fn models_status(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Reply::Models {
            status,
            body: body.into(),
        }
    }
}

#[derive(Clone)]
struct FakeChat {
    base: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    connections: Arc<AtomicUsize>,
}

impl FakeChat {
    fn request(&self, index: usize) -> RecordedRequest {
        self.requests.lock().unwrap()[index].clone()
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

async fn spawn_fake(turns: Vec<Reply>, models: Reply) -> FakeChat {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    let turns = Arc::new(turns);
    let models = Arc::new(models);
    let turn_index = Arc::new(AtomicUsize::new(0));

    let requests_task = Arc::clone(&requests);
    let connections_task = Arc::clone(&connections);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            connections_task.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(serve_connection(
                socket,
                Arc::clone(&requests_task),
                Arc::clone(&turns),
                Arc::clone(&models),
                Arc::clone(&turn_index),
            ));
        }
    });

    FakeChat {
        base,
        requests,
        connections,
    }
}

async fn serve_connection(
    mut socket: TcpStream,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    turns: Arc<Vec<Reply>>,
    models: Arc<Reply>,
    turn_index: Arc<AtomicUsize>,
) {
    loop {
        let Some(request) = read_request(&mut socket).await else {
            break;
        };
        let path = request.path.clone();
        requests.lock().unwrap().push(request);
        let reply = if path == "/models" {
            (*models).clone()
        } else {
            let index = turn_index.fetch_add(1, Ordering::SeqCst);
            turns
                .get(index)
                .cloned()
                .unwrap_or_else(|| Reply::status(500, "no scripted turn"))
        };
        if !write_reply(&mut socket, &reply).await {
            break;
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

async fn write_reply(socket: &mut TcpStream, reply: &Reply) -> bool {
    match reply {
        Reply::Sse {
            body,
            keep_alive,
            hold_open,
        } => {
            let head = if *keep_alive {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                    body.len()
                )
            } else {
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                    .to_string()
            };
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body).await;
            let _ = socket.flush().await;
            if *hold_open {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            *keep_alive && !*hold_open
        }
        Reply::Status { status, body } => {
            let head = format!(
                "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reason(*status),
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body).await;
            false
        }
        Reply::Models { status, body } => {
            let head = format!(
                "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
                reason(*status),
                body.len(),
                if *status < 400 { "keep-alive" } else { "close" }
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body).await;
            *status < 400
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = socket.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Some(RecordedRequest {
        method,
        path,
        headers,
        body,
    })
}

// ---------------------------------------------------------------------------
// Adapter / request helpers
// ---------------------------------------------------------------------------

const TEXT_STOP_USAGE: &str = include_str!("fixtures/sse/chat/text_stop_usage.sse");
const REASONING_STOP: &str = include_str!("fixtures/sse/chat/reasoning_stop.sse");
const TOOL_CALLS_INDEXED: &str = include_str!("fixtures/sse/chat/tool_calls_indexed.sse");

const SHELL_SCHEMA: &str =
    r#"{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}"#;

fn provider_config(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        api: Api::ChatCompletions,
        base_url: base_url.to_string(),
        env_key: None,
        key: Some("key".to_string()),
        command: None,
        default_model: "m".to_string(),
        models: Vec::new(),
        effort_map: BTreeMap::new(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    }
}

fn config_with_key(base_url: &str, key: &str) -> ProviderConfig {
    let mut cfg = provider_config(base_url);
    cfg.key = Some(key.to_string());
    cfg
}

fn adapter(base_url: &str) -> ChatCompletions {
    ChatCompletions::new("deepseek", &provider_config(base_url)).unwrap()
}

fn adapter_with_backoff(base_url: &str, backoff: Duration) -> ChatCompletions {
    let mut adapter = adapter(base_url);
    adapter.set_backoff(Arc::new(move |_| backoff));
    adapter
}

fn run_ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn raw_json(text: &str) -> Box<RawValue> {
    RawValue::from_string(text.to_string()).unwrap()
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        text: text.to_string(),
        ..Default::default()
    }
}

fn turn_request(model: &str) -> TurnRequest {
    TurnRequest {
        model: model.to_string(),
        ..Default::default()
    }
}

fn shell_spec() -> ToolSpec {
    ToolSpec {
        name: "shell".to_string(),
        description: "run".to_string(),
        parameters: raw_json(SHELL_SCHEMA),
    }
}

fn sse_text(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":text}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    )
}

async fn run_turn(
    adapter: &ChatCompletions,
    req: TurnRequest,
) -> Result<TurnResult, ProviderError> {
    adapter.turn(run_ctx(), req, Box::new(|_| {})).await
}

async fn run_turn_with_deltas(
    adapter: &ChatCompletions,
    req: TurnRequest,
) -> (Result<TurnResult, ProviderError>, Vec<String>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink_log = Arc::clone(&log);
    let sink: DeltaSink = Box::new(move |delta| sink_log.lock().unwrap().push(delta));
    let result = adapter.turn(run_ctx(), req, sink).await;
    let deltas = log.lock().unwrap().clone();
    (result, deltas)
}

fn message_texts(body: &Value) -> Vec<(String, String)> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            (
                message["role"].as_str().unwrap_or_default().to_string(),
                message["content"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Adapter: request construction, message replay, tools, max tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn adapter_text_turn_puts_the_expected_request_on_the_wire() {
    let fake = spawn_fake(
        vec![Reply::sse(TEXT_STOP_USAGE)],
        Reply::models_ok(&["model-a"]),
    )
    .await;
    let adapter = adapter(&fake.base);

    let mut req = turn_request("m");
    req.effort = Some("high".to_string());
    req.system = "sys".to_string();
    req.messages = vec![user("hi")];
    req.tools = vec![shell_spec()];

    let (result, deltas) = run_turn_with_deltas(&adapter, req).await;
    let result = result.unwrap();

    assert_eq!(result.text, "Let me check.");
    assert_eq!(deltas, vec!["Let me ".to_string(), "check.".to_string()]);
    assert_eq!(
        result.usage,
        Some(Usage {
            input: 7,
            cached: 3,
            output: 5,
            reasoning: 2,
            total: 12,
        })
    );
    assert_eq!(adapter.name(), "deepseek");

    assert_eq!(fake.request_count(), 1);
    let recorded = fake.request(0);
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/chat/completions");
    assert_eq!(recorded.header("authorization"), Some("Bearer key"));
    assert_eq!(recorded.header("accept"), Some("text/event-stream"));
    assert!(
        recorded
            .header("content-type")
            .is_some_and(|value| value.starts_with("application/json"))
    );
    assert_eq!(recorded.header("cache-control"), Some("no-cache"));

    let body = recorded.json();
    assert_eq!(body["model"], "m");
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert!(body.get("max_tokens").is_none());

    assert_eq!(
        message_texts(&body),
        vec![
            ("system".to_string(), "sys".to_string()),
            ("user".to_string(), "hi".to_string()),
        ]
    );

    let tools = body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "shell");
    assert_eq!(tools[0]["function"]["description"], "run");
    assert_eq!(
        tools[0]["function"]["parameters"],
        serde_json::from_str::<Value>(SHELL_SCHEMA).unwrap()
    );
}

#[tokio::test]
async fn adapter_replays_reasoning_and_tool_results() {
    let fake = spawn_fake(
        vec![Reply::sse(TOOL_CALLS_INDEXED), Reply::sse(sse_text("done"))],
        Reply::models_ok(&[]),
    )
    .await;
    let adapter = adapter(&fake.base);

    let mut first_req = turn_request("m");
    first_req.messages = vec![user("go")];
    first_req.tools = vec![shell_spec()];
    let first = run_turn(&adapter, first_req).await.unwrap();

    assert_eq!(first.text, "");
    assert_eq!(first.tool_calls.len(), 2);
    assert_eq!(first.tool_calls[0].id, "call_a");
    assert_eq!(first.tool_calls[0].name, "shell");
    assert_eq!(first.tool_calls[0].arguments, r#"{"command":"ls"}"#);
    assert_eq!(first.tool_calls[1].id, "call_b");
    assert_eq!(first.tool_calls[1].name, "get_weather");
    assert_eq!(first.tool_calls[1].arguments, r#"{"city":"Vancouver"}"#);

    let mut history = vec![user("go")];
    history.push(Message {
        role: Role::Assistant,
        text: first.text.clone(),
        tool_calls: first.tool_calls.clone(),
        tool_call_id: None,
        is_error: false,
        opaque: first.opaque.clone(),
    });
    history.push(Message {
        role: Role::Tool,
        text: "a.txt".to_string(),
        tool_calls: Vec::new(),
        tool_call_id: Some("call_a".to_string()),
        is_error: true,
        opaque: None,
    });

    let mut second_req = turn_request("m");
    second_req.messages = history;
    second_req.tools = vec![shell_spec()];
    run_turn(&adapter, second_req).await.unwrap();

    let body = fake.request(1).json();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "no system message was requested");

    let assistant = &messages[1];
    assert_eq!(assistant["role"], "assistant");
    assert!(assistant["tool_calls"].is_array());
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(assistant["tool_calls"][0]["id"], "call_a");
    assert_eq!(assistant["tool_calls"][0]["type"], "function");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "shell");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        r#"{"command":"ls"}"#
    );

    let tool = &messages[2];
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["tool_call_id"], "call_a");
    assert_eq!(tool["content"], "a.txt");
    assert!(
        tool.get("is_error").is_none(),
        "chat completions carries tool errors in the content text, not a flag"
    );
}

#[tokio::test]
async fn adapter_replays_reasoning_content_from_opaque() {
    let fake = spawn_fake(
        vec![Reply::sse(REASONING_STOP), Reply::sse(sse_text("done"))],
        Reply::models_ok(&[]),
    )
    .await;
    let adapter = adapter(&fake.base);

    let mut req = turn_request("m");
    req.messages = vec![user("hi")];
    let first = run_turn(&adapter, req).await.unwrap();
    assert_eq!(first.reasoning.as_deref(), Some("think first"));
    assert_eq!(first.text, "answer");
    let opaque = first.opaque.expect("reasoning produces an opaque payload");
    assert_eq!(
        serde_json::from_str::<Value>(opaque.get()).unwrap(),
        json!({"reasoning_content": "think first"})
    );

    let mut history = vec![user("hi")];
    history.push(Message {
        role: Role::Assistant,
        text: first.text.clone(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        is_error: false,
        opaque: Some(opaque),
    });
    let mut second_req = turn_request("m");
    second_req.messages = history;
    run_turn(&adapter, second_req).await.unwrap();

    let body = fake.request(1).json();
    let assistant = &body["messages"].as_array().unwrap()[1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["reasoning_content"], "think first");
    assert_eq!(assistant["content"], "answer");
}

#[tokio::test]
async fn list_models_returns_ids_and_wraps_http_errors() {
    let fake = spawn_fake(vec![], Reply::models_ok(&["model-a", "model-b"])).await;
    let lister = adapter(&fake.base);
    let models = lister.list_models().await.unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["model-a", "model-b"]
    );

    let recorded = fake.request(0);
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.path, "/models");
    assert_eq!(recorded.header("authorization"), Some("Bearer key"));
    assert_eq!(recorded.header("accept"), Some("application/json"));

    let failing = spawn_fake(
        vec![],
        Reply::models_status(401, r#"{"error":{"message":"unauthorized"}}"#),
    )
    .await;
    let err = adapter(&failing.base)
        .list_models()
        .await
        .expect_err("a 401 must fail ListModels");
    assert!(
        err.message.contains("list models"),
        "ListModels error must carry its context: {}",
        err.message
    );
    assert!(err.message.contains("401"), "error: {}", err.message);
}

#[tokio::test]
async fn turn_sends_configured_max_output_tokens_and_omits_an_unset_one() {
    for (configure, expected) in [(Some(4096i64), Some(4096i64)), (None, None)] {
        let fake = spawn_fake(vec![Reply::sse(sse_text("ok"))], Reply::models_ok(&[])).await;
        let mut cfg = provider_config(&fake.base);
        cfg.max_output_tokens = configure;
        let adapter = ChatCompletions::new("deepseek", &cfg).unwrap();

        let mut req = turn_request("m");
        req.messages = vec![user("hi")];
        run_turn(&adapter, req).await.unwrap();

        let body = fake.request(0).json();
        match expected {
            Some(value) => assert_eq!(body["max_tokens"], value),
            None => assert!(
                body.get("max_tokens").is_none(),
                "max_tokens must be absent when the config sets no limit: {body}"
            ),
        }
    }
}

#[tokio::test]
async fn turn_rejects_a_tool_schema_that_is_not_an_object_without_sending() {
    let fake = spawn_fake(vec![Reply::sse(sse_text("ok"))], Reply::models_ok(&[])).await;
    let adapter = adapter(&fake.base);

    let mut req = turn_request("m");
    req.messages = vec![user("hi")];
    req.tools = vec![ToolSpec {
        name: "shell".to_string(),
        description: String::new(),
        parameters: raw_json("[]"),
    }];

    let err = run_turn(&adapter, req).await.unwrap_err();
    assert!(
        err.message.contains("tool shell schema"),
        "error must name the malformed schema: {}",
        err.message
    );
    assert_eq!(
        fake.request_count(),
        0,
        "a malformed schema must not reach the API"
    );
}

#[tokio::test]
async fn turn_sends_an_object_for_an_absent_tool_schema() {
    let fake = spawn_fake(vec![Reply::sse(sse_text("ok"))], Reply::models_ok(&[])).await;
    let adapter = adapter(&fake.base);

    let mut req = turn_request("m");
    req.messages = vec![user("hi")];
    req.tools = vec![ToolSpec {
        name: "ping".to_string(),
        description: String::new(),
        parameters: Box::default(),
    }];
    run_turn(&adapter, req).await.unwrap();

    let body = fake.request(0).json();
    let parameters = &body["tools"][0]["function"]["parameters"];
    assert_eq!(
        parameters,
        &json!({}),
        "an absent schema must be sent as an empty object, not null"
    );
}

#[tokio::test]
async fn turn_sends_the_callers_raw_tool_schema_bytes() {
    let schema =
        r#"{"type":"object","properties":{"n":{"type":"number","default":1e0}},"required":["n"]}"#;
    let fake = spawn_fake(vec![Reply::sse(sse_text("ok"))], Reply::models_ok(&[])).await;
    let adapter = adapter(&fake.base);

    let mut req = turn_request("m");
    req.messages = vec![user("hi")];
    req.tools = vec![ToolSpec {
        name: "count".to_string(),
        description: "count".to_string(),
        parameters: raw_json(schema),
    }];
    run_turn(&adapter, req).await.unwrap();

    let raw = fake.request(0).body_text();
    assert!(
        raw.contains(&format!(r#""parameters":{schema}"#)),
        "the caller's schema bytes must go out verbatim (no re-encoding): {raw}"
    );
}

// ---------------------------------------------------------------------------
// Streaming: finish reasons, tool-call assembly, usage, upstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_turn_reconstructs_tool_calls_by_index() {
    let fake = spawn_fake(vec![Reply::sse(TOOL_CALLS_INDEXED)], Reply::models_ok(&[])).await;
    let result = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .unwrap();

    assert_eq!(
        result.tool_calls,
        vec![
            ToolCall {
                id: "call_a".to_string(),
                name: "shell".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
            },
            ToolCall {
                id: "call_b".to_string(),
                name: "get_weather".to_string(),
                arguments: r#"{"city":"Vancouver"}"#.to_string(),
            },
        ]
    );
    assert_eq!(result.usage.as_ref().map(|u| u.total), Some(12));
}

#[tokio::test]
async fn chat_turn_assembles_a_single_indexless_tool_call() {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"tool_calls":[{
            "id":"call_indexless","type":"function",
            "function":{"name":"get_weather","arguments":"{\"city\":\"Vancouver\"}"}
        }]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    );
    let fake = spawn_fake(vec![Reply::sse(body)], Reply::models_ok(&[])).await;
    let result = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .unwrap();

    assert_eq!(
        result.tool_calls,
        vec![ToolCall {
            id: "call_indexless".to_string(),
            name: "get_weather".to_string(),
            arguments: r#"{"city":"Vancouver"}"#.to_string(),
        }]
    );
}

#[tokio::test]
async fn chat_turn_assembles_tool_call_fragments() {
    struct Case {
        name: &'static str,
        body: String,
        want: Vec<ToolCall>,
        want_err: Option<&'static str>,
    }

    let finish = json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]});
    let cases = vec![
        Case {
            name: "two index-less calls in separate chunks",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"tool_calls":[{
                    "id":"call_ls","type":"function",
                    "function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}
                }]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{
                    "id":"call_read","type":"function",
                    "function":{"name":"read_file","arguments":"{\"path\":\"x.txt\"}"}
                }]}}]}),
                finish,
            ),
            want: vec![
                ToolCall {
                    id: "call_ls".to_string(),
                    name: "shell".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                },
                ToolCall {
                    id: "call_read".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"x.txt"}"#.to_string(),
                },
            ],
            want_err: None,
        },
        Case {
            name: "index-only first fragment filled by the id and name fragment",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"tool_calls":[{"index":0}]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{
                    "id":"call_weather","type":"function",
                    "function":{"name":"get_weather","arguments":"{\"city\":\"Vancouver\"}"}
                }]}}]}),
                finish,
            ),
            want: vec![ToolCall {
                id: "call_weather".to_string(),
                name: "get_weather".to_string(),
                arguments: r#"{"city":"Vancouver"}"#.to_string(),
            }],
            want_err: None,
        },
        Case {
            name: "index-less continuation appends arguments",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"tool_calls":[{
                    "id":"call_shell","type":"function",
                    "function":{"name":"shell","arguments":"{\"comm"}
                }]}}]}),
                json!({"choices":[{"delta":{"tool_calls":[{
                    "function":{"arguments":"and\":\"ls\"}"}
                }]}}]}),
                finish,
            ),
            want: vec![ToolCall {
                id: "call_shell".to_string(),
                name: "shell".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
            }],
            want_err: None,
        },
        Case {
            name: "call without a name fails",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"tool_calls":[{
                    "id":"call_anon","type":"function",
                    "function":{"arguments":"{\"city\":\"Vancouver\"}"}
                }]}}]}),
                finish,
            ),
            want: Vec::new(),
            want_err: Some("no name"),
        },
        Case {
            name: "call without an id fails",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"tool_calls":[{
                    "type":"function",
                    "function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}
                }]}}]}),
                finish,
            ),
            want: Vec::new(),
            want_err: Some("has no id"),
        },
    ];

    for case in cases {
        let fake = spawn_fake(vec![Reply::sse(case.body)], Reply::models_ok(&[])).await;
        let result = run_turn(&adapter(&fake.base), turn_request("m")).await;
        match case.want_err {
            Some(want) => {
                let err = result.expect_err(case.name);
                assert!(
                    err.message.contains(want),
                    "{}: error = {:?}, want it to contain {want:?}",
                    case.name,
                    err.message
                );
            }
            None => {
                let result = result.unwrap_or_else(|e| panic!("{}: {e}", case.name));
                assert_eq!(result.tool_calls, case.want, "{}", case.name);
            }
        }
    }
}

#[tokio::test]
async fn chat_turn_rejects_unfinished_streams() {
    struct Case {
        name: &'static str,
        body: String,
        want_err: &'static str,
    }

    let usage =
        json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":5,"total_tokens":12}});
    let mid_tool_call = json!({"choices":[{"delta":{"tool_calls":[{
        "id":"call_write","type":"function",
        "function":{"name":"write_file","arguments":"{\"path\":\"a.go\",\"content\":\"package ma"}
    }]}}]});

    let cases = vec![
        Case {
            name: "finish_reason length after text",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content":"The fix is to chan"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
                usage,
            ),
            want_err: r#"chat completions: finish_reason "length": response was truncated by the output token limit"#,
        },
        Case {
            name: "finish_reason length mid tool call",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                mid_tool_call,
                json!({"choices":[{"delta":{},"finish_reason":"length"}]}),
                usage,
            ),
            want_err: r#"chat completions: finish_reason "length": response was truncated by the output token limit"#,
        },
        Case {
            name: "finish_reason content_filter after text",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content":"Sure, here"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"content_filter"}]}),
                usage,
            ),
            want_err: r#"chat completions: finish_reason "content_filter": response was stopped by the content filter"#,
        },
        Case {
            name: "body ends without finish reason or DONE",
            body: format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"content":"half an ans"}}]}),
            ),
            want_err: "chat completions: stream ended without a finish reason (truncated response)",
        },
    ];

    for case in cases {
        let fake = spawn_fake(vec![Reply::sse(case.body)], Reply::models_ok(&[])).await;
        let err = run_turn(&adapter(&fake.base), turn_request("m"))
            .await
            .expect_err(case.name);
        assert_eq!(err.message, case.want_err, "{}", case.name);
    }
}

#[tokio::test]
async fn chat_turn_accepts_well_formed_streams() {
    struct Case {
        name: &'static str,
        body: String,
        want_text: &'static str,
        want_reasoning: Option<&'static str>,
        want_calls: Vec<ToolCall>,
    }

    let usage =
        json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":5,"total_tokens":12}});
    let cases = vec![
        Case {
            name: "deepseek text stream ending in stop with usage chunk",
            body: TEXT_STOP_USAGE.to_string(),
            want_text: "Let me check.",
            want_reasoning: None,
            want_calls: Vec::new(),
        },
        Case {
            name: "indexed tool call stream ending in tool_calls",
            body: TOOL_CALLS_INDEXED.to_string(),
            want_text: "",
            want_reasoning: None,
            want_calls: vec![
                ToolCall {
                    id: "call_a".to_string(),
                    name: "shell".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                },
                ToolCall {
                    id: "call_b".to_string(),
                    name: "get_weather".to_string(),
                    arguments: r#"{"city":"Vancouver"}"#.to_string(),
                },
            ],
        },
        Case {
            name: "compatible server finishing with eos",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content":"done"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"eos"}]}),
                usage,
            ),
            want_text: "done",
            want_reasoning: None,
            want_calls: Vec::new(),
        },
        Case {
            name: "unrecognised finish reason is accepted",
            body: format!(
                "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content":"done"}}]}),
                json!({"choices":[{"delta":{},"finish_reason":"vendor_specific"}]}),
                usage,
            ),
            want_text: "done",
            want_reasoning: None,
            want_calls: Vec::new(),
        },
        Case {
            name: "reasoning content stream ending in stop",
            body: REASONING_STOP.to_string(),
            want_text: "answer",
            want_reasoning: Some("think first"),
            want_calls: Vec::new(),
        },
    ];

    for case in cases {
        let fake = spawn_fake(vec![Reply::sse(case.body)], Reply::models_ok(&[])).await;
        let result = run_turn(&adapter(&fake.base), turn_request("m"))
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", case.name));
        assert_eq!(result.text, case.want_text, "{}", case.name);
        assert_eq!(
            result.reasoning.as_deref(),
            case.want_reasoning,
            "{}",
            case.name
        );
        assert_eq!(result.tool_calls, case.want_calls, "{}", case.name);
        assert_eq!(
            result.usage.as_ref().map(|u| u.total),
            Some(12),
            "{}",
            case.name
        );
    }
}

#[tokio::test]
async fn chat_turn_reports_the_serving_upstream() {
    let routed = concat!(
        "data: {\"id\":\"g1\",\"provider\":\"StreamLake\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
        "data: {\"id\":\"g1\",\"provider\":\"StreamLake\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let fake = spawn_fake(
        vec![Reply::sse(routed), Reply::sse(sse_text("plain server"))],
        Reply::models_ok(&[]),
    )
    .await;
    let adapter = adapter(&fake.base);

    let first = run_turn(&adapter, turn_request("m")).await.unwrap();
    assert_eq!(first.upstream.as_deref(), Some("StreamLake"));
    let second = run_turn(&adapter, turn_request("m")).await.unwrap();
    assert_eq!(second.upstream, None);
}

#[tokio::test]
async fn chat_turn_returns_when_the_server_keeps_the_stream_open_after_done() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let fake = spawn_fake(vec![Reply::sse_hold_open(body)], Reply::models_ok(&[])).await;
    let adapter = adapter(&fake.base);

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        run_turn(&adapter, turn_request("m")),
    )
    .await
    .expect("the turn must not hang after [DONE]")
    .unwrap();
    assert_eq!(result.text, "hi");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the bounded drain must return promptly"
    );
}

// ---------------------------------------------------------------------------
// Framing: line-based, no blank-line requirement, [DONE], no scanner limit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn framing_two_data_lines_without_a_blank_line_yield_two_events() {
    let body = format!(
        "data: {}\ndata: {}\ndata: {}\ndata: {}\ndata: [DONE]\n",
        json!({"choices":[{"delta":{"content":"a"}}]}),
        json!({"choices":[{"delta":{"content":"b"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}),
    );
    let fake = spawn_fake(vec![Reply::sse(body)], Reply::models_ok(&[])).await;
    let (result, deltas) = run_turn_with_deltas(&adapter(&fake.base), turn_request("m")).await;
    let result = result.unwrap();

    assert_eq!(result.text, "ab");
    assert_eq!(deltas, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(result.usage.as_ref().map(|u| u.total), Some(3));
}

#[tokio::test]
async fn framing_blank_and_non_data_lines_are_not_events() {
    let body = format!(
        ": ping\nevent: message\nid: 1\n\nretry: 100\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"ok"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
    );
    let fake = spawn_fake(vec![Reply::sse(body)], Reply::models_ok(&[])).await;
    let result = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .unwrap();
    assert_eq!(result.text, "ok");
}

#[tokio::test]
async fn framing_exact_done_ends_the_stream() {
    // A finish reason before [DONE] is kept; anything after [DONE] is never read.
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\ndata: {}\n\ndata: {}\n\n",
        json!({"choices":[{"delta":{"content":"before"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
        json!({"choices":[{"delta":{"content":"after"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
    );
    let fake = spawn_fake(vec![Reply::sse(body)], Reply::models_ok(&[])).await;
    let result = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .unwrap();
    assert_eq!(result.text, "before", "events after [DONE] must be ignored");

    // Trailing whitespace is trimmed, so this is still the sentinel.
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]   \n\n",
        json!({"choices":[{"delta":{"content":"trimmed"}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
    );
    let fake = spawn_fake(vec![Reply::sse(body)], Reply::models_ok(&[])).await;
    let result = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .unwrap();
    assert_eq!(result.text, "trimmed");

    // `[DONE]x` is not the sentinel; it is an undecodable event.
    let fake = spawn_fake(vec![Reply::sse("data: [DONE]x\n\n")], Reply::models_ok(&[])).await;
    let err = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .expect_err("a non-sentinel [DONE] line is not the end of the stream");
    assert!(
        err.message.contains("stream chunk") || err.message.contains("expected"),
        "unexpected error: {}",
        err.message
    );
}

#[tokio::test]
async fn framing_has_no_fixed_scanner_limit() {
    // go-openai uses bufio.Reader.ReadBytes, which grows with the line. A
    // million-byte event must be delivered whole rather than hitting a scanner
    // cap.
    let content = "x".repeat(1 << 20);
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":content}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
    );
    let fake = spawn_fake(vec![Reply::sse(body)], Reply::models_ok(&[])).await;
    let result = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .unwrap();
    assert_eq!(result.text.len(), 1 << 20);
    assert!(result.text.bytes().all(|byte| byte == b'x'));
}

#[tokio::test]
async fn framing_accepts_null_stream_fields() {
    // OpenAI-compatible servers routinely send `"content":null` on tool-call
    // chunks; Go's encoding/json decodes null into the zero value.
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":null,\"tool_calls\":null},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}],\"usage\":null}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let fake = spawn_fake(vec![Reply::sse(body)], Reply::models_ok(&[])).await;
    let result = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .unwrap();
    assert_eq!(result.text, "ok");
}

#[tokio::test]
async fn framing_reports_an_api_error_frame() {
    let fake = spawn_fake(
        vec![Reply::sse(
            "data: {\"error\":{\"message\":\"bad key\"}}\n\n",
        )],
        Reply::models_ok(&[]),
    )
    .await;
    let err = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .expect_err("an API error frame must fail the turn");
    assert_eq!(err.message, "error, bad key");
}

#[tokio::test]
async fn framing_enforces_the_go_openai_empty_message_limit() {
    assert_eq!(EMPTY_MESSAGES_LIMIT, 300);

    let at_limit = format!("{}{TEXT_STOP_USAGE}", "\n".repeat(EMPTY_MESSAGES_LIMIT));
    let fake = spawn_fake(vec![Reply::sse(at_limit)], Reply::models_ok(&[])).await;
    run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .expect("300 empty messages is within the limit");

    let over_limit = format!("{}{TEXT_STOP_USAGE}", "\n".repeat(EMPTY_MESSAGES_LIMIT + 1));
    let fake = spawn_fake(vec![Reply::sse(over_limit)], Reply::models_ok(&[])).await;
    let err = run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .expect_err("301 empty messages exceeds the limit");
    assert_eq!(err.message, "stream has sent too many empty messages");
}

// ---------------------------------------------------------------------------
// Retry policy: statuses, connection failures, backoff, cancellation
// ---------------------------------------------------------------------------

#[test]
fn is_retryable_covers_transport_failures() {
    assert!(chat_completions::is_retryable(SetupFailure::HttpStatus(
        429
    )));
    assert!(chat_completions::is_retryable(SetupFailure::HttpStatus(
        503
    )));
    assert!(chat_completions::is_retryable(SetupFailure::HttpStatus(0)));
    assert!(!chat_completions::is_retryable(SetupFailure::HttpStatus(
        400
    )));
    assert!(chat_completions::is_retryable(SetupFailure::Connection));
    assert!(!chat_completions::is_retryable(SetupFailure::Canceled));
    assert!(!chat_completions::is_retryable(
        SetupFailure::DeadlineExceeded
    ));
    assert!(!chat_completions::is_retryable(SetupFailure::Other));
}

#[test]
fn default_backoff_jitters_around_the_base() {
    for (attempt, base) in [
        (0usize, Duration::from_secs(1)),
        (1, Duration::from_secs(2)),
        (2, Duration::from_secs(4)),
    ] {
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let got = chat_completions::default_backoff(attempt);
            assert!(
                got >= base / 2 && got <= base,
                "default_backoff({attempt}) = {got:?}, want between {:?} and {base:?}",
                base / 2
            );
            seen.insert(got);
        }
        assert!(
            seen.len() > 1,
            "default_backoff({attempt}) returned one value over 200 draws, want a jittered schedule"
        );
    }
}

#[tokio::test]
async fn chat_turn_retries_retryable_statuses_then_succeeds() {
    let fake = spawn_fake(
        vec![
            Reply::status(500, "boom"),
            Reply::status(429, "slow down"),
            Reply::sse(sse_text("ok")),
        ],
        Reply::models_ok(&[]),
    )
    .await;
    let adapter = adapter_with_backoff(&fake.base, Duration::ZERO);

    let result = run_turn(&adapter, turn_request("m")).await.unwrap();
    assert_eq!(result.text, "ok");
    assert_eq!(fake.request_count(), 3);
}

#[tokio::test]
async fn chat_turn_returns_an_error_after_retries_are_exhausted() {
    let fake = spawn_fake(
        vec![
            Reply::status(500, "boom"),
            Reply::status(500, "boom"),
            Reply::status(500, "boom"),
            Reply::status(500, "boom"),
        ],
        Reply::models_ok(&[]),
    )
    .await;
    let adapter = adapter_with_backoff(&fake.base, Duration::ZERO);

    let err = run_turn(&adapter, turn_request("m"))
        .await
        .expect_err("four 500s must fail the turn");
    assert!(
        err.message.contains("500"),
        "unexpected error: {}",
        err.message
    );
    assert_eq!(fake.request_count(), MAX_ATTEMPTS);
}

#[tokio::test]
async fn chat_turn_does_not_retry_a_non_retryable_status() {
    let fake = spawn_fake(
        vec![Reply::status(400, "bad request")],
        Reply::models_ok(&[]),
    )
    .await;
    let adapter = adapter_with_backoff(&fake.base, Duration::ZERO);

    let err = run_turn(&adapter, turn_request("m"))
        .await
        .expect_err("a 400 must not be retried");
    assert!(
        err.message.contains("400"),
        "unexpected error: {}",
        err.message
    );
    assert_eq!(fake.request_count(), 1);
}

#[tokio::test]
async fn chat_turn_stops_backoff_when_the_context_is_canceled() {
    let fake = spawn_fake(vec![Reply::status(500, "boom")], Reply::models_ok(&[])).await;
    let adapter = Arc::new(adapter_with_backoff(&fake.base, Duration::from_secs(3600)));
    let cancel = CancellationToken::new();
    let turn_cancel = cancel.clone();

    let started = Instant::now();
    let handle = tokio::spawn(async move {
        adapter
            .turn(
                RunContext {
                    cancel: turn_cancel,
                    deadline: None,
                },
                turn_request("m"),
                Box::new(|_| {}),
            )
            .await
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });

    let err = tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("cancellation must stop the one-hour backoff promptly")
        .unwrap()
        .expect_err("a canceled run must fail");
    assert!(
        err.message.contains("context canceled"),
        "unexpected error: {}",
        err.message
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "cancellation took {:?}",
        started.elapsed()
    );
    assert_eq!(fake.request_count(), 1);
}

#[tokio::test]
async fn chat_turn_stops_a_stream_when_the_context_is_canceled() {
    // The server sends one content event and then holds the stream open. A
    // canceled run must not wait for the next event indefinitely.
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let fake = spawn_fake(vec![Reply::sse_hold_open(body)], Reply::models_ok(&[])).await;
    let adapter = Arc::new(adapter(&fake.base));
    let cancel = CancellationToken::new();
    let turn_cancel = cancel.clone();

    let started = Instant::now();
    let handle = tokio::spawn(async move {
        adapter
            .turn(
                RunContext {
                    cancel: turn_cancel,
                    deadline: None,
                },
                turn_request("m"),
                Box::new(|_| {}),
            )
            .await
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });

    let err = tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("mid-stream cancellation must stop the turn promptly")
        .unwrap()
        .expect_err("a canceled run must fail");
    assert!(
        err.message.contains("context canceled"),
        "unexpected error: {}",
        err.message
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "mid-stream cancellation took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn chat_turn_retries_a_connection_that_never_reached_a_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let attempts = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&attempts);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            accepted.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });

    let adapter = adapter_with_backoff(&base, Duration::ZERO);
    let err = run_turn(&adapter, turn_request("m"))
        .await
        .expect_err("a listener that never answers must fail the turn");
    assert!(
        !err.message.is_empty(),
        "the transport error must be reported"
    );

    // maxAttempts is 4; a loop that does not retry a transport failure stops at 1.
    let deadline = Instant::now() + Duration::from_secs(2);
    while attempts.load(Ordering::SeqCst) < MAX_ATTEMPTS && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        MAX_ATTEMPTS,
        "stream setup must be retried when the connection never reaches a server"
    );
}

// ---------------------------------------------------------------------------
// extra_body routing fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extra_body_reaches_the_request_and_absent_extra_body_leaves_it_alone() {
    let routed = concat!(
        "data: {\"id\":\"g1\",\"provider\":\"StreamLake\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
        "data: {\"id\":\"g1\",\"provider\":\"StreamLake\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let fake = spawn_fake(
        vec![Reply::sse(routed), Reply::sse(sse_text("plain"))],
        Reply::models_ok(&[]),
    )
    .await;

    let mut extra = serde_json::Map::new();
    extra.insert(
        "provider".to_string(),
        json!({"ignore": ["relace"], "sort": "throughput"}),
    );
    extra.insert("seed".to_string(), json!(7));
    let mut cfg = provider_config(&fake.base);
    cfg.extra_body = Some(extra);
    let with_extra = ChatCompletions::new("deepseek", &cfg).unwrap();
    run_turn(&with_extra, turn_request("m")).await.unwrap();

    let body = fake.request(0).json();
    assert_eq!(body["provider"]["sort"], "throughput");
    assert_eq!(body["provider"]["ignore"], json!(["relace"]));
    assert_eq!(body["seed"], 7);
    assert_eq!(body["model"], "m");
    assert_eq!(body["stream"], true);
    assert!(body["messages"].is_array());

    run_turn(&adapter(&fake.base), turn_request("m"))
        .await
        .unwrap();
    let plain = fake.request(1).json();
    assert!(
        plain.get("provider").is_none(),
        "a request without extra_body must carry no routing field: {plain}"
    );
}

// ---------------------------------------------------------------------------
// Shared pool: sessions reuse one endpoint pool while keeping their own key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequential_sessions_share_one_connection_and_keep_their_keys() {
    let fake = spawn_fake(
        vec![
            Reply::sse_keep_alive(TEXT_STOP_USAGE),
            Reply::sse_keep_alive(TEXT_STOP_USAGE),
        ],
        Reply::models_ok(&[]),
    )
    .await;

    let first =
        ChatCompletions::new("session-1", &config_with_key(&fake.base, "key-first")).unwrap();
    let second =
        ChatCompletions::new("session-2", &config_with_key(&fake.base, "key-second")).unwrap();

    run_turn(&first, turn_request("model-1")).await.unwrap();
    run_turn(&second, turn_request("model-2")).await.unwrap();

    assert_eq!(
        fake.connection_count(),
        1,
        "two sequential sessions for one endpoint must reuse its pool"
    );
    assert_eq!(
        fake.request(0).header("authorization"),
        Some("Bearer key-first")
    );
    assert_eq!(
        fake.request(1).header("authorization"),
        Some("Bearer key-second")
    );
}

#[tokio::test]
async fn concurrent_sessions_keep_their_own_keys_on_the_shared_pool() {
    const SESSIONS: usize = 24;
    let turns = (0..SESSIONS)
        .map(|_| Reply::sse(sse_text("ok")))
        .collect::<Vec<_>>();
    let fake = spawn_fake(turns, Reply::models_ok(&[])).await;

    let mut handles = Vec::new();
    for index in 0..SESSIONS {
        let fake = fake.clone();
        handles.push(tokio::spawn(async move {
            let key = format!("key-{index:02}");
            let adapter = ChatCompletions::new(
                &format!("session-{index}"),
                &config_with_key(&fake.base, &key),
            )
            .unwrap();
            let mut req = turn_request(&format!("model-{index}"));
            req.messages = vec![user("hi")];
            let result = run_turn(&adapter, req).await;
            (index, key, result.map(|result| result.text))
        }));
    }

    let mut keys = HashMap::new();
    for handle in handles {
        let (index, key, result) = handle.await.unwrap();
        assert_eq!(result.unwrap(), "ok", "session {index}");
        keys.insert(format!("model-{index}"), key);
    }

    assert_eq!(fake.request_count(), SESSIONS);
    for recorded in fake.requests() {
        let model = recorded.json()["model"].as_str().unwrap().to_string();
        let want = keys
            .get(&model)
            .expect("every request names a session model");
        let want_header = format!("Bearer {want}");
        assert_eq!(
            recorded.header("authorization"),
            Some(want_header.as_str()),
            "{model} carried another session's key"
        );
    }
}

// ---------------------------------------------------------------------------
// Credential environment never reaches the wire
// ---------------------------------------------------------------------------

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    async fn apply(changes: &[(&'static str, Option<&str>)]) -> Self {
        let lock = ENV_LOCK.lock().await;
        let mut saved = Vec::new();
        for &(key, value) in changes {
            saved.push((key, std::env::var(key).ok()));
            // SAFETY: environment mutation is serialized by ENV_LOCK for the
            // whole test, and every saved value is restored on drop.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        EnvGuard { saved, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            // SAFETY: the lock is still held by this guard.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

const CREDENTIAL_ENV: [&str; 10] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_ADMIN_KEY",
    "OPENAI_ORG_ID",
    "OPENAI_PROJECT_ID",
    "OPENAI_CUSTOM_HEADERS",
    "OPENAI_BASE_URL",
];

async fn spawn_capture() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}",
                )
                .await;
        }
    });
    (base, hits)
}

fn assert_no_environment_headers(headers: &HashMap<String, String>, forbidden: &[&str], op: &str) {
    for (name, value) in headers {
        assert!(
            name != "x-corp-gateway-token",
            "{op}: environment-derived header {name}: {value}"
        );
        assert!(
            name != "openai-organization",
            "{op}: environment-derived header {name}: {value}"
        );
        assert!(
            name != "openai-project",
            "{op}: environment-derived header {name}: {value}"
        );
        for secret in forbidden {
            assert!(
                !value.contains(secret),
                "{op}: environment value {secret:?} leaked into header {name}: {value}"
            );
        }
    }
}

#[tokio::test]
async fn chat_completions_env_credentials_never_reach_the_wire() {
    let (anthropic_elsewhere, anthropic_hits) = spawn_capture().await;
    let (openai_elsewhere, openai_hits) = spawn_capture().await;

    let hostile = [
        ("ANTHROPIC_API_KEY", Some("sk-ant-env-DIFFERENT-key")),
        ("ANTHROPIC_AUTH_TOKEN", Some("sk-ant-oat-SECRET-from-env")),
        (
            "ANTHROPIC_CUSTOM_HEADERS",
            Some("X-Corp-Gateway-Token: gw-secret"),
        ),
        ("ANTHROPIC_BASE_URL", Some(anthropic_elsewhere.as_str())),
        ("OPENAI_API_KEY", Some("sk-openai-env-DIFFERENT-key")),
        ("OPENAI_ADMIN_KEY", Some("sk-admin-env-SECRET")),
        ("OPENAI_ORG_ID", Some("org-real")),
        ("OPENAI_PROJECT_ID", Some("proj-real")),
        (
            "OPENAI_CUSTOM_HEADERS",
            Some("X-Corp-Gateway-Token: gw-SECRET"),
        ),
        ("OPENAI_BASE_URL", Some(openai_elsewhere.as_str())),
    ];
    let _guard = EnvGuard::apply(&hostile).await;

    let forbidden = [
        "sk-ant-env-DIFFERENT-key",
        "sk-ant-oat-SECRET-from-env",
        "gw-secret",
        "sk-openai-env-DIFFERENT-key",
        "sk-admin-env-SECRET",
        "org-real",
        "proj-real",
        "gw-SECRET",
        anthropic_elsewhere.as_str(),
        openai_elsewhere.as_str(),
    ];

    let fake = spawn_fake(
        vec![Reply::sse(sse_text("hello"))],
        Reply::models_ok(&["model-a"]),
    )
    .await;
    let adapter =
        ChatCompletions::new("deepseek", &config_with_key(&fake.base, "cfg-key-0123")).unwrap();

    let mut req = turn_request("m");
    req.messages = vec![user("hi")];
    let result = run_turn(&adapter, req).await.unwrap();
    assert_eq!(result.text, "hello");
    let ids = adapter.list_models().await.unwrap();
    assert_eq!(
        ids.iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["model-a"]
    );

    assert_eq!(fake.request_count(), 2, "turn and ListModels only");
    for (index, op) in [(0usize, "turn"), (1, "ListModels")] {
        let recorded = fake.request(index);
        assert_eq!(
            recorded.header("authorization"),
            Some("Bearer cfg-key-0123")
        );
        assert!(
            recorded.header("accept").is_some(),
            "{op}: standard Accept header missing"
        );
        assert_no_environment_headers(&recorded.headers, &forbidden, op);
    }
    assert_eq!(
        anthropic_hits.load(Ordering::SeqCst) + openai_hits.load(Ordering::SeqCst),
        0,
        "no request may go to an environment base_url"
    );
}

#[tokio::test]
async fn new_resolves_the_key_from_env_key_and_reports_a_missing_one_verbatim() {
    let _guard = EnvGuard::apply(&[
        ("WIRE_CHAT_TEST_KEY", Some("env-key")),
        ("WIRE_CHAT_MISSING_KEY", None),
    ])
    .await;

    let fake = spawn_fake(vec![Reply::sse(sse_text("ok"))], Reply::models_ok(&[])).await;
    let mut cfg = provider_config(&fake.base);
    cfg.key = None;
    cfg.env_key = Some("WIRE_CHAT_TEST_KEY".to_string());
    let adapter = ChatCompletions::new("deepseek", &cfg).unwrap();
    run_turn(&adapter, turn_request("m")).await.unwrap();
    assert_eq!(
        fake.request(0).header("authorization"),
        Some("Bearer env-key")
    );

    let mut missing = provider_config(&fake.base);
    missing.key = None;
    missing.env_key = Some("WIRE_CHAT_MISSING_KEY".to_string());
    let err = ChatCompletions::new("deepseek", &missing)
        .err()
        .expect("a missing env_key must fail construction");
    assert_eq!(
        err.message,
        "environment variable WIRE_CHAT_MISSING_KEY (providers.deepseek.env_key) must be set to the API key"
    );
}

#[test]
fn credential_env_list_matches_the_go_test() {
    // Guard against the list drifting: every variable the Go test treats as
    // credential-bearing is covered here too.
    let expected = [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_CUSTOM_HEADERS",
        "ANTHROPIC_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_ADMIN_KEY",
        "OPENAI_ORG_ID",
        "OPENAI_PROJECT_ID",
        "OPENAI_CUSTOM_HEADERS",
        "OPENAI_BASE_URL",
    ];
    assert_eq!(CREDENTIAL_ENV, expected);
}
