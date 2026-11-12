//! Task 12: Anthropic Messages adapter wire tests.
//!
//! Ports `internal/provider/messages/{adapter,usage,env_credentials,shared_transport}_test.go`
//! against a local HTTP mock that records requests and serves scripted SSE, plus
//! the Messages-specific SSE framing cases from the design: blank-line dispatch,
//! empty events dispatched, the 32 MiB line cap, multiple `data:` lines
//! concatenated with `\n`, `[DONE]` treated as ordinary data, EOF without a
//! `message_delta`, and EOF with a non-empty `stop_reason` but no `message_stop`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::provider::messages::{MAX_PAUSE_CONTINUATIONS, Messages};
use subagent_mcp::provider::{
    DeltaSink, ModelLister, Provider, ProviderError, Role, ToolCall, ToolSpec, TurnRequest,
    TurnResult, Usage,
};
use subagent_mcp::util::run::RunContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Local HTTP mock (stand-in for Go's httptest.Server + testutil.FakeMessages)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Scripted {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

impl Scripted {
    fn sse(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream".into(),
            body: body.into(),
        }
    }

    fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "application/json".into(),
            body: body.into(),
        }
    }

    fn status(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "application/json".into(),
            body: body.into(),
        }
    }
}

#[derive(Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("recorded request body is JSON")
    }

    fn header(&self, name: &str) -> Option<&str> {
        let wanted = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == wanted)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct MockState {
    scripts: VecDeque<Scripted>,
    requests: Vec<Recorded>,
    connections: usize,
    unexpected: usize,
}

struct Mock {
    base: String,
    state: Arc<Mutex<MockState>>,
}

impl Mock {
    async fn start(scripts: Vec<Scripted>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(MockState {
            scripts: scripts.into(),
            ..MockState::default()
        }));
        let server_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                server_state.lock().unwrap().connections += 1;
                let state = Arc::clone(&server_state);
                tokio::spawn(async move {
                    while let Some((head, body)) = read_request(&mut socket).await {
                        let (method, path, headers) = parse_head(&head);
                        let script = {
                            let mut locked = state.lock().unwrap();
                            locked.requests.push(Recorded {
                                method,
                                path,
                                headers,
                                body,
                            });
                            locked.scripts.pop_front()
                        };
                        let script = match script {
                            Some(script) => script,
                            None => {
                                state.lock().unwrap().unexpected += 1;
                                Scripted::status(
                                    500,
                                    br#"{"type":"error","error":{"type":"server_error","message":"unexpected extra request"}}"#
                                        .to_vec(),
                                )
                            }
                        };
                        if write_response(&mut socket, &script).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        Self { base, state }
    }

    fn config(&self, max_output_tokens: i64) -> ProviderConfig {
        ProviderConfig {
            api: Api::Messages,
            base_url: self.base.clone(),
            env_key: None,
            key: Some("k".to_string()),
            command: None,
            default_model: "claude-x".to_string(),
            models: Vec::new(),
            effort_map: BTreeMap::new(),
            max_output_tokens: Some(max_output_tokens),
            max_nudges: None,
            extra_body: None,
        }
    }

    fn request(&self, index: usize) -> Recorded {
        let locked = self.state.lock().unwrap();
        locked.requests.get(index).cloned().unwrap_or_else(|| {
            panic!(
                "request index {index} out of range; recorded {}",
                locked.requests.len()
            )
        })
    }

    fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    fn connections(&self) -> usize {
        self.state.lock().unwrap().connections
    }

    fn unexpected(&self) -> usize {
        self.state.lock().unwrap().unexpected
    }
}

async fn read_request(socket: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body_end = (header_end + content_length).min(buf.len());
    Some((head, buf[header_end..body_end].to_vec()))
}

fn parse_head(head: &str) -> (String, String, Vec<(String, String)>) {
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split_whitespace();
    let method = request_line.next().unwrap_or("").to_string();
    let path = request_line.next().unwrap_or("").to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    (method, path, headers)
}

async fn write_response(socket: &mut TcpStream, script: &Scripted) -> std::io::Result<()> {
    let reason = match script.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        script.status,
        reason,
        script.content_type,
        script.body.len()
    );
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&script.body);
    socket.write_all(&bytes).await?;
    socket.flush().await
}

// ---------------------------------------------------------------------------
// Scripted Messages SSE (mirrors testutil.FakeMessages)
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum ScriptBlock {
    Thinking {
        thinking: String,
        signature: String,
    },
    Text(String),
    Tool {
        id: String,
        name: String,
        chunks: Vec<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Truncate {
    Block,
    Delta,
}

#[derive(Clone, Default)]
struct ScriptMessage {
    status: u16,
    blocks: Vec<ScriptBlock>,
    stop_reason: Option<String>,
    refusal_category: Option<String>,
    input_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    output_tokens: i64,
    thinking_tokens: i64,
    truncate: Option<Truncate>,
}

impl ScriptMessage {
    fn text(text: &str) -> Self {
        Self {
            blocks: vec![ScriptBlock::Text(text.to_string())],
            ..Self::default()
        }
    }

    fn http_error(status: u16) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    fn scripted(&self) -> Scripted {
        if self.status != 0 && self.status != 200 {
            Scripted::status(
                self.status,
                br#"{"type":"error","error":{"type":"invalid_request_error","message":"scripted failure"}}"#
                    .to_vec(),
            )
        } else {
            Scripted::sse(sse_bytes(self))
        }
    }
}

fn event_bytes(out: &mut Vec<u8>, event: &str, mut payload: Value) {
    payload["type"] = Value::String(event.to_string());
    out.extend_from_slice(format!("event: {event}\ndata: {payload}\n\n").as_bytes());
}

fn sse_bytes(script: &ScriptMessage) -> Vec<u8> {
    let mut out = Vec::new();
    event_bytes(
        &mut out,
        "message_start",
        json!({
            "message": {
                "id": "msg_0",
                "type": "message",
                "role": "assistant",
                "model": "claude-x",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": script.input_tokens,
                    "cache_read_input_tokens": script.cache_read_tokens,
                    "cache_creation_input_tokens": script.cache_creation_tokens,
                    "output_tokens": 0
                }
            }
        }),
    );

    let mut has_tool = false;
    for (index, block) in script.blocks.iter().enumerate() {
        match block {
            ScriptBlock::Thinking {
                thinking,
                signature,
            } => {
                event_bytes(
                    &mut out,
                    "content_block_start",
                    json!({
                        "index": index,
                        "content_block": {"type": "thinking", "thinking": "", "signature": ""}
                    }),
                );
                if script.truncate == Some(Truncate::Block) {
                    return out;
                }
                if !thinking.is_empty() {
                    event_bytes(
                        &mut out,
                        "content_block_delta",
                        json!({
                            "index": index,
                            "delta": {"type": "thinking_delta", "thinking": thinking}
                        }),
                    );
                }
                event_bytes(
                    &mut out,
                    "content_block_delta",
                    json!({
                        "index": index,
                        "delta": {"type": "signature_delta", "signature": signature}
                    }),
                );
            }
            ScriptBlock::Text(text) => {
                event_bytes(
                    &mut out,
                    "content_block_start",
                    json!({
                        "index": index,
                        "content_block": {"type": "text", "text": ""}
                    }),
                );
                if script.truncate == Some(Truncate::Block) {
                    return out;
                }
                event_bytes(
                    &mut out,
                    "content_block_delta",
                    json!({
                        "index": index,
                        "delta": {"type": "text_delta", "text": text}
                    }),
                );
            }
            ScriptBlock::Tool { id, name, chunks } => {
                has_tool = true;
                event_bytes(
                    &mut out,
                    "content_block_start",
                    json!({
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": {}
                        }
                    }),
                );
                if script.truncate == Some(Truncate::Block) {
                    return out;
                }
                for chunk in chunks {
                    event_bytes(
                        &mut out,
                        "content_block_delta",
                        json!({
                            "index": index,
                            "delta": {"type": "input_json_delta", "partial_json": chunk}
                        }),
                    );
                }
            }
        }
        if script.truncate == Some(Truncate::Delta) {
            return out;
        }
        event_bytes(&mut out, "content_block_stop", json!({"index": index}));
    }

    let stop_reason = script.stop_reason.clone().unwrap_or_else(|| {
        if has_tool {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    });
    let stop_details = if stop_reason == "refusal" {
        json!({
            "type": "refusal",
            "category": script.refusal_category.clone().unwrap_or_default(),
            "explanation": "scripted refusal"
        })
    } else {
        Value::Null
    };
    event_bytes(
        &mut out,
        "message_delta",
        json!({
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": null,
                "stop_details": stop_details
            },
            "usage": {
                "output_tokens": script.output_tokens,
                "output_tokens_details": {"thinking_tokens": script.thinking_tokens}
            }
        }),
    );
    event_bytes(&mut out, "message_stop", json!({}));
    out
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn shell_spec() -> ToolSpec {
    ToolSpec {
        name: "shell".to_string(),
        description: "run".to_string(),
        parameters: serde_json::value::to_raw_value(&json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        }))
        .unwrap(),
    }
}

fn user_only() -> Vec<subagent_mcp::provider::Message> {
    vec![subagent_mcp::provider::Message {
        role: Role::User,
        text: "hi".to_string(),
        ..Default::default()
    }]
}

fn request(model: &str, messages: Vec<subagent_mcp::provider::Message>) -> TurnRequest {
    TurnRequest {
        model: model.to_string(),
        messages,
        ..TurnRequest::default()
    }
}

async fn run_turn(
    provider: &Messages,
    req: TurnRequest,
) -> (Result<TurnResult, ProviderError>, String) {
    let deltas = Arc::new(Mutex::new(String::new()));
    let sink: DeltaSink = {
        let deltas = Arc::clone(&deltas);
        Box::new(move |text: String| deltas.lock().unwrap().push_str(&text))
    };
    let result = provider.turn(ctx(), req, sink).await;
    let collected = deltas.lock().unwrap().clone();
    (result, collected)
}

async fn one_turn(
    mock: &Mock,
    max_output_tokens: i64,
    req: TurnRequest,
) -> (Result<TurnResult, ProviderError>, String) {
    let provider = Messages::new("anthropic", &mock.config(max_output_tokens)).unwrap();
    run_turn(&provider, req).await
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/sse/messages/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|error| panic!("read fixture {path}: {error}"))
}

async fn run_fixture(name: &str) -> (Result<TurnResult, ProviderError>, String) {
    let mock = Mock::start(vec![Scripted::sse(fixture(name))]).await;
    one_turn(&mock, 1000, request("claude-x", user_only())).await
}

fn assistant_from(result: &TurnResult) -> subagent_mcp::provider::Message {
    subagent_mcp::provider::Message {
        role: Role::Assistant,
        text: result.text.clone(),
        tool_calls: result.tool_calls.clone(),
        opaque: result.opaque.clone(),
        ..Default::default()
    }
}

fn tool_result(call: &ToolCall, text: &str, is_error: bool) -> subagent_mcp::provider::Message {
    subagent_mcp::provider::Message {
        role: Role::Tool,
        text: text.to_string(),
        tool_call_id: Some(call.id.clone()),
        is_error,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Request shape: adaptive summarized thinking, cache control, effort, tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_turn_request_shape() {
    let script = ScriptMessage {
        blocks: vec![
            ScriptBlock::Thinking {
                thinking: "hmm".to_string(),
                signature: "sig-1".to_string(),
            },
            ScriptBlock::Text("hello".to_string()),
        ],
        input_tokens: 10,
        cache_read_tokens: 5,
        cache_creation_tokens: 2,
        output_tokens: 3,
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![script.scripted()]).await;
    let mut req = request("claude-x", user_only());
    req.effort = Some("xhigh".to_string());
    req.system = "sys".to_string();
    req.tools = vec![shell_spec()];

    let (result, deltas) = one_turn(&mock, 64000, req).await;
    let result = result.expect("turn");

    assert_eq!(result.text, "hello");
    assert_eq!(deltas, "hello");
    assert_eq!(result.reasoning.as_deref(), Some("hmm"));
    assert_eq!(
        result.usage,
        Some(Usage {
            input: 17,
            cached: 5,
            output: 3,
            reasoning: 0,
            total: 20,
        })
    );

    let recorded = mock.request(0);
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/v1/messages");
    assert_eq!(recorded.header("x-api-key"), Some("k"));
    assert_eq!(recorded.header("anthropic-version"), Some("2023-06-01"));
    assert!(
        recorded
            .header("content-type")
            .is_some_and(|value| value.starts_with("application/json")),
        "content type: {:?}",
        recorded.header("content-type")
    );

    let body = recorded.json();
    assert_eq!(body["model"], json!("claude-x"));
    assert_eq!(body["max_tokens"], json!(64000));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(body["output_config"], json!({"effort": "xhigh"}));
    assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(body["system"], json!([{"type": "text", "text": "sys"}]));

    let tool = &body["tools"][0];
    assert_eq!(tool["name"], json!("shell"));
    assert_eq!(tool["description"], json!("run"));
    assert_eq!(tool["eager_input_streaming"], json!(true));
    assert_eq!(
        tool["input_schema"],
        json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"]
        })
    );
}

#[tokio::test]
async fn every_schema_keyword_is_forwarded() {
    let mock = Mock::start(vec![ScriptMessage::text("ok").scripted()]).await;
    let spec = ToolSpec {
        name: "shell".to_string(),
        description: String::new(),
        parameters: serde_json::value::to_raw_value(&json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
            "additionalProperties": false,
            "title": "Shell",
            "$defs": {"n": {"type": "number"}}
        }))
        .unwrap(),
    };
    let mut req = request("claude-x", user_only());
    req.tools = vec![spec];

    one_turn(&mock, 1000, req).await.0.expect("turn");

    let schema = mock.request(0).json()["tools"][0]["input_schema"].clone();
    assert_eq!(schema["type"], json!("object"));
    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["title"], json!("Shell"));
    assert_eq!(schema["$defs"], json!({"n": {"type": "number"}}));
    assert_eq!(schema["properties"], json!({"command": {"type": "string"}}));
    assert_eq!(schema["required"], json!(["command"]));
}

#[tokio::test]
async fn malformed_tool_schemas_are_reported() {
    // A schema that is not a JSON object at all, plus the two `required`
    // shapes the Messages API cannot express.
    for (parameters, want) in [
        (json!([]), "tool shell schema"),
        (json!({"required": "command"}), "required must be an array"),
        (json!({"required": [1]}), "required must list only names"),
    ] {
        let mock = Mock::start(Vec::new()).await;
        let spec = ToolSpec {
            name: "shell".to_string(),
            description: String::new(),
            parameters: serde_json::value::to_raw_value(&parameters).unwrap(),
        };
        let mut req = request("claude-x", user_only());
        req.tools = vec![spec];
        let (result, _) = one_turn(&mock, 1000, req).await;
        let error = result.expect_err("malformed schema must fail");
        assert!(
            error.message.contains(want),
            "error {:?} does not contain {want:?}",
            error.message
        );
        assert_eq!(mock.request_count(), 0, "no request may be sent");
    }
}

// ---------------------------------------------------------------------------
// Opaque replay, tool-result grouping, thinking signatures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replays_thinking_and_groups_tool_results() {
    let scripts = [
        ScriptMessage {
            blocks: vec![
                ScriptBlock::Thinking {
                    thinking: "plan".to_string(),
                    signature: "sig-1".to_string(),
                },
                ScriptBlock::Tool {
                    id: "tu_1".to_string(),
                    name: "shell".to_string(),
                    chunks: vec![r#"{"command":"ls"}"#.to_string()],
                },
                ScriptBlock::Tool {
                    id: "tu_2".to_string(),
                    name: "shell".to_string(),
                    chunks: vec![r#"{"command":"pwd"}"#.to_string()],
                },
            ],
            ..ScriptMessage::default()
        },
        ScriptMessage {
            blocks: vec![
                ScriptBlock::Thinking {
                    thinking: "again".to_string(),
                    signature: "sig-2".to_string(),
                },
                ScriptBlock::Tool {
                    id: "tu_3".to_string(),
                    name: "shell".to_string(),
                    chunks: vec![r#"{"command":"cat a"}"#.to_string()],
                },
            ],
            ..ScriptMessage::default()
        },
        ScriptMessage::text("done"),
    ];
    let mock = Mock::start(scripts.iter().map(ScriptMessage::scripted).collect()).await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();

    let mut history = vec![subagent_mcp::provider::Message {
        role: Role::User,
        text: "go".to_string(),
        ..Default::default()
    }];
    for turn in 0..3 {
        let mut req = request("claude-x", history.clone());
        req.tools = vec![shell_spec()];
        let (result, _) = run_turn(&provider, req).await;
        let result = result.expect("turn");
        if turn == 0 {
            assert_eq!(result.tool_calls.len(), 2);
            assert_eq!(result.tool_calls[0].arguments, r#"{"command":"ls"}"#);
        }
        history.push(assistant_from(&result));
        for (index, call) in result.tool_calls.iter().enumerate() {
            history.push(tool_result(call, &format!("out-{}", call.id), index == 1));
        }
    }

    let messages = mock.request(2).json()["messages"].clone();
    let messages = messages.as_array().expect("messages array");
    assert_eq!(
        messages.len(),
        5,
        "want user, assistant, grouped results, assistant, grouped results"
    );
    assert_eq!(messages[1]["content"][0]["type"], json!("thinking"));
    assert_eq!(messages[1]["content"][0]["signature"], json!("sig-1"));
    assert_eq!(messages[3]["content"][0]["signature"], json!("sig-2"));

    let results = messages[2]["content"].as_array().expect("tool results");
    assert_eq!(results.len(), 2, "consecutive tool results are one message");
    assert_eq!(results[0]["type"], json!("tool_result"));
    assert_eq!(results[0]["tool_use_id"], json!("tu_1"));
    assert_eq!(results[1]["tool_use_id"], json!("tu_2"));
    assert_eq!(results[1]["is_error"], json!(true));
    assert_eq!(results[0]["content"][0]["text"], json!("out-tu_1"));
}

#[tokio::test]
async fn truncated_tool_input_surfaces_verbatim() {
    let script = ScriptMessage {
        blocks: vec![ScriptBlock::Tool {
            id: "tu_1".to_string(),
            name: "shell".to_string(),
            chunks: vec![r#"{"command":"ls"#.to_string()],
        }],
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![script.scripted()]).await;
    let mut req = request("claude-x", user_only());
    req.tools = vec![shell_spec()];

    let (result, _) = one_turn(&mock, 1000, req).await;
    let result = result.expect("adapter must not fail on bad tool JSON (the runner rejects it)");
    assert_eq!(result.tool_calls.len(), 1);
    assert!(
        serde_json::from_str::<Value>(&result.tool_calls[0].arguments).is_err(),
        "truncated input must reach the runner as invalid JSON: {:?}",
        result.tool_calls
    );
}

#[tokio::test]
async fn multi_delta_and_empty_tool_input() {
    let scripts = [
        ScriptMessage {
            blocks: vec![
                ScriptBlock::Tool {
                    id: "tu_1".to_string(),
                    name: "shell".to_string(),
                    chunks: vec![
                        r#"{"com"#.to_string(),
                        r#"mand":"l"#.to_string(),
                        r#"s"}"#.to_string(),
                    ],
                },
                ScriptBlock::Tool {
                    id: "tu_2".to_string(),
                    name: "noargs".to_string(),
                    chunks: Vec::new(),
                },
            ],
            ..ScriptMessage::default()
        },
        ScriptMessage::text("done"),
    ];
    let mock = Mock::start(scripts.iter().map(ScriptMessage::scripted).collect()).await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();

    let mut req = request("claude-x", user_only());
    req.tools = vec![shell_spec()];
    let (result, _) = run_turn(&provider, req).await;
    let result = result.expect("turn");
    assert_eq!(result.tool_calls.len(), 2);
    assert_eq!(result.tool_calls[0].arguments, r#"{"command":"ls"}"#);
    assert_eq!(result.tool_calls[1].arguments, "{}");

    let mut history = user_only();
    history.push(assistant_from(&result));
    for call in &result.tool_calls {
        history.push(tool_result(call, "out", false));
    }
    let mut req = request("claude-x", history);
    req.tools = vec![shell_spec()];
    run_turn(&provider, req).await.0.expect("second turn");

    let content = mock.request(1).json()["messages"][1]["content"].clone();
    let content = content.as_array().expect("assistant content");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], json!("tool_use"));
    assert_eq!(content[0]["id"], json!("tu_1"));
    assert_eq!(content[0]["input"], json!({"command": "ls"}));
    assert_eq!(content[1]["id"], json!("tu_2"));
    assert_eq!(content[1]["input"], json!({}));
}

#[tokio::test]
async fn empty_end_turn_is_never_replayed() {
    let scripts = [
        ScriptMessage {
            blocks: Vec::new(),
            ..ScriptMessage::default()
        },
        ScriptMessage::text("second answer"),
    ];
    let mock = Mock::start(scripts.iter().map(ScriptMessage::scripted).collect()).await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();

    let mut history = vec![subagent_mcp::provider::Message {
        role: Role::User,
        text: "first".to_string(),
        ..Default::default()
    }];
    let (result, _) = run_turn(&provider, request("claude-x", history.clone())).await;
    let result = result.expect("empty end_turn must be accepted as a final answer");
    assert_eq!(result.text, "");
    assert!(result.tool_calls.is_empty());
    assert_eq!(
        mock.request(0).json()["tools"],
        json!([]),
        "tools is emitted even when empty (Go's non-nil slice)"
    );

    history.push(assistant_from(&result));
    history.push(subagent_mcp::provider::Message {
        role: Role::User,
        text: "second".to_string(),
        ..Default::default()
    });
    let (second, _) = run_turn(&provider, request("claude-x", history)).await;
    assert_eq!(second.expect("second turn").text, "second answer");

    for (index, entry) in mock.request(1).json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .enumerate()
    {
        if entry["role"] != json!("assistant") {
            continue;
        }
        let content = entry["content"].as_array().expect("content");
        assert!(
            !content.is_empty(),
            "request 1 message {index} replays an empty assistant message: {entry}"
        );
        for block in content {
            assert!(
                !(block["type"] == json!("text") && block["text"] == json!("")),
                "request 1 message {index} replays an empty text block: {block}"
            );
        }
    }
}

#[tokio::test]
async fn corrupt_replay_payload_is_reported() {
    let mock = Mock::start(Vec::new()).await;
    let history = vec![
        subagent_mcp::provider::Message {
            role: Role::User,
            text: "hi".to_string(),
            ..Default::default()
        },
        subagent_mcp::provider::Message {
            role: Role::Assistant,
            // Valid JSON, but not the stored turn array the adapter wrote.
            opaque: Some(serde_json::value::to_raw_value(&json!({"a": 1})).unwrap()),
            ..Default::default()
        },
    ];
    let (result, _) = one_turn(&mock, 1000, request("claude-x", history)).await;
    let error = result.expect_err("corrupt replay payload");
    assert!(
        error
            .message
            .starts_with("messages: corrupt replay payload: "),
        "unexpected error: {:?}",
        error.message
    );
    assert_eq!(mock.request_count(), 0, "no request may be sent");
}

// ---------------------------------------------------------------------------
// Stop reasons and errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_reasons_fail_the_turn() {
    let refusal = ScriptMessage {
        blocks: vec![ScriptBlock::Text("no".to_string())],
        stop_reason: Some("refusal".to_string()),
        refusal_category: Some("cyber".to_string()),
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![refusal.scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    assert_eq!(
        result.expect_err("refusal").message,
        "messages: model refused the request (category \"cyber\"): scripted refusal"
    );

    let max_tokens = ScriptMessage {
        blocks: vec![ScriptBlock::Text("cut".to_string())],
        stop_reason: Some("max_tokens".to_string()),
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![max_tokens.scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    assert_eq!(
        result.expect_err("max_tokens").message,
        "messages: response hit max_tokens; raise max_output_tokens in the provider config"
    );

    let mock = Mock::start(vec![ScriptMessage::http_error(400).scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    let error = result.expect_err("HTTP 400");
    assert!(
        error.message.contains("scripted failure"),
        "HTTP error text: {:?}",
        error.message
    );
}

#[tokio::test]
async fn context_window_exceeded_fails() {
    let script = ScriptMessage {
        blocks: vec![ScriptBlock::Text("overflow".to_string())],
        stop_reason: Some("model_context_window_exceeded".to_string()),
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![script.scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    assert_eq!(
        result.expect_err("context window").message,
        "messages: response hit model_context_window_exceeded; shorten the conversation or use a model with a larger context window"
    );
}

#[tokio::test]
async fn truncated_stream_fails() {
    for (cut, want_deltas) in [(Truncate::Delta, "half an answer"), (Truncate::Block, "")] {
        let script = ScriptMessage {
            blocks: vec![ScriptBlock::Text("half an answer".to_string())],
            truncate: Some(cut),
            ..ScriptMessage::default()
        };
        let mock = Mock::start(vec![script.scripted()]).await;
        let (result, deltas) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
        let error = result.expect_err("truncated stream must fail the turn");
        assert!(
            error.message.contains("stop signal"),
            "want a missing-stop-signal error, got {:?}",
            error.message
        );
        assert_eq!(deltas, want_deltas);
        if cut == Truncate::Delta {
            assert!(
                error.message.contains("half an answer"),
                "partial text dropped from the error: {:?}",
                error.message
            );
            assert_eq!(
                error.message,
                "messages: stream ended without a terminal stop signal (partial text: \"half an answer\")"
            );
        }
    }
}

#[tokio::test]
async fn pause_turn_continues() {
    let scripts = [
        ScriptMessage {
            blocks: vec![ScriptBlock::Text("part one ".to_string())],
            stop_reason: Some("pause_turn".to_string()),
            ..ScriptMessage::default()
        },
        ScriptMessage::text("part two"),
    ];
    let mock = Mock::start(scripts.iter().map(ScriptMessage::scripted).collect()).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    let result = result.expect("pause_turn continuation");
    assert!(result.text.contains("part one"));
    assert!(result.text.contains("part two"));

    let second = mock.request(1).json()["messages"].clone();
    let second = second.as_array().expect("messages");
    assert_eq!(second.len(), 2);
    assert_eq!(second[1]["role"], json!("assistant"));
    assert_eq!(second[1]["content"][0]["text"], json!("part one "));
}

#[tokio::test]
async fn pause_turn_exhausts_continuations() {
    let scripts: Vec<Scripted> = (0..=MAX_PAUSE_CONTINUATIONS)
        .map(|index| {
            ScriptMessage {
                blocks: vec![ScriptBlock::Text(format!("part {index} "))],
                stop_reason: Some("pause_turn".to_string()),
                ..ScriptMessage::default()
            }
            .scripted()
        })
        .collect();
    let mock = Mock::start(scripts).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    assert_eq!(
        result.expect_err("still paused").message,
        "messages: turn still paused after 5 continuations"
    );
    assert_eq!(mock.request_count(), MAX_PAUSE_CONTINUATIONS + 1);
    assert_eq!(mock.unexpected(), 0);
}

// ---------------------------------------------------------------------------
// Usage mapping: input includes cache, cached is a subset, total = input+output
// ---------------------------------------------------------------------------

#[tokio::test]
async fn usage_mapping_is_one_meaning() {
    let script = ScriptMessage {
        blocks: vec![ScriptBlock::Text("hi".to_string())],
        input_tokens: 25,
        cache_read_tokens: 50,
        cache_creation_tokens: 25,
        output_tokens: 20,
        thinking_tokens: 7,
        ..ScriptMessage::default()
    };
    let mock = Mock::start(vec![script.scripted()]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    let usage = result.expect("turn").usage.expect("usage");
    assert_eq!(
        usage,
        Usage {
            input: 100,
            cached: 50,
            output: 20,
            reasoning: 7,
            total: 120,
        }
    );
    assert!(usage.cached <= usage.input, "cached is a subset of input");
    assert_eq!(usage.total, usage.input + usage.output);
}

// ---------------------------------------------------------------------------
// Model listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_models_returns_ids() {
    let mock = Mock::start(vec![Scripted::json(
        br#"{"data":[{"id":"model-a","type":"model"},{"id":"model-b","type":"model"},{"id":"model-c","type":"model"}],"has_more":false}"#.to_vec(),
    )])
    .await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    let ids = provider
        .list_models()
        .await
        .expect("list models")
        .into_iter()
        .map(|info| info.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["model-a", "model-b", "model-c"]);

    let recorded = mock.request(0);
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.path, "/v1/models");
    assert_eq!(recorded.header("x-api-key"), Some("k"));
}

#[tokio::test]
async fn list_models_follows_pagination() {
    let mock = Mock::start(vec![
        Scripted::json(
            br#"{"data":[{"id":"model-a"}],"has_more":true,"last_id":"model-a"}"#.to_vec(),
        ),
        Scripted::json(br#"{"data":[{"id":"model-b"}],"has_more":false}"#.to_vec()),
    ])
    .await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    let ids = provider
        .list_models()
        .await
        .expect("list models")
        .into_iter()
        .map(|info| info.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["model-a", "model-b"]);
    assert_eq!(mock.request(1).path, "/v1/models?after_id=model-a");
}

#[tokio::test]
async fn list_models_http_error() {
    let mock = Mock::start(vec![Scripted::status(
        401,
        br#"{"type":"error","error":{"type":"authentication_error","message":"scripted failure"}}"#
            .to_vec(),
    )])
    .await;
    let provider = Messages::new("anthropic", &mock.config(1000)).unwrap();
    let error = provider.list_models().await.expect_err("HTTP error");
    assert!(
        error.message.contains("list models"),
        "want wrapped \"list models\" context, got {:?}",
        error.message
    );
    assert!(
        error.message.contains("scripted failure"),
        "want the API message, got {:?}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// Credentials never come from the environment
// ---------------------------------------------------------------------------

struct EnvGuard {
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    fn set(pairs: &[(&str, &str)]) -> Self {
        let mut saved = Vec::new();
        for (key, value) in pairs {
            saved.push((key.to_string(), std::env::var(key).ok()));
            // SAFETY: the credential test is the only test that mutates the
            // process environment; every other test passes `cfg.key`.
            unsafe { std::env::set_var(key, value) };
        }
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}

#[tokio::test]
async fn environment_credentials_never_reach_the_wire() {
    const CONFIGURED_KEY: &str = "cfg-key-0123";
    const ENV_API_KEY: &str = "sk-ant-env-DIFFERENT-key";
    const ENV_AUTH_TOKEN: &str = "sk-ant-oat-SECRET-from-env";
    const ENV_GATEWAY: &str = "gw-secret";
    const ENV_OPENAI_KEY: &str = "sk-openai-env-DIFFERENT-key";

    let elsewhere = Mock::start(Vec::new()).await;
    let elsewhere_url = elsewhere.base.clone();
    let configured = Mock::start(vec![
        ScriptMessage::text("hello").scripted(),
        Scripted::json(br#"{"data":[{"id":"model-a"}],"has_more":false}"#.to_vec()),
    ])
    .await;

    let _guard = EnvGuard::set(&[
        ("ANTHROPIC_API_KEY", ENV_API_KEY),
        ("ANTHROPIC_AUTH_TOKEN", ENV_AUTH_TOKEN),
        (
            "ANTHROPIC_CUSTOM_HEADERS",
            "X-Corp-Gateway-Token: gw-secret",
        ),
        ("ANTHROPIC_BASE_URL", &elsewhere_url),
        ("OPENAI_API_KEY", ENV_OPENAI_KEY),
        ("OPENAI_BASE_URL", &elsewhere_url),
    ]);

    let mut cfg = configured.config(1000);
    cfg.key = Some(CONFIGURED_KEY.to_string());

    let provider = Messages::new("anthropic", &cfg).unwrap();
    let (result, _) = run_turn(&provider, request("claude-x", user_only())).await;
    assert_eq!(result.expect("turn").text, "hello");
    let ids = provider
        .list_models()
        .await
        .expect("list models")
        .into_iter()
        .map(|info| info.id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["model-a"]);

    assert_eq!(
        configured.request_count(),
        2,
        "want exactly a turn and a ListModels request"
    );
    assert_eq!(
        elsewhere.requests().len(),
        0,
        "no request may go to the environment base URL"
    );

    let forbidden_values = [
        ENV_API_KEY,
        ENV_AUTH_TOKEN,
        ENV_GATEWAY,
        ENV_OPENAI_KEY,
        elsewhere_url.as_str(),
    ];
    let forbidden_headers = [
        "authorization",
        "x-corp-gateway-token",
        "openai-organization",
    ];
    for index in 0..configured.request_count() {
        let recorded = configured.request(index);
        assert_eq!(
            recorded.header("x-api-key"),
            Some(CONFIGURED_KEY),
            "request {index} must carry only the configured key"
        );
        assert!(
            recorded.header("anthropic-version").is_some(),
            "request {index} must carry Anthropic-Version"
        );
        for (name, value) in &recorded.headers {
            assert!(
                !forbidden_headers.contains(&name.as_str()),
                "request {index} sent environment-derived header {name}: {value}"
            );
            for secret in forbidden_values {
                assert!(
                    !value.contains(secret),
                    "request {index} leaked {secret:?} into header {name}: {value}"
                );
            }
        }
    }
    assert!(
        configured
            .request(0)
            .header("content-type")
            .is_some_and(|value| value.starts_with("application/json"))
    );
}

#[tokio::test]
async fn missing_env_key_is_reported() {
    let mock = Mock::start(Vec::new()).await;
    let mut cfg = mock.config(1000);
    cfg.key = None;
    cfg.env_key = Some("SUBAGENT_MCP_TEST_MESSAGES_KEY_UNSET".to_string());
    let error = match Messages::new("anthropic", &cfg) {
        Ok(_) => panic!("a missing env key must fail"),
        Err(error) => error,
    };
    assert_eq!(
        error.message,
        "environment variable SUBAGENT_MCP_TEST_MESSAGES_KEY_UNSET (providers.anthropic.env_key) must be set to the API key"
    );
}

// ---------------------------------------------------------------------------
// Shared pool: one connection per base URL, one key per session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequential_sessions_share_one_connection() {
    let mock = Mock::start(vec![
        ScriptMessage::text("first").scripted(),
        ScriptMessage::text("second").scripted(),
    ])
    .await;

    let mut first_cfg = mock.config(1000);
    first_cfg.key = Some("key-first".to_string());
    let first = Messages::new("session-1", &first_cfg).unwrap();
    let mut second_cfg = mock.config(1000);
    second_cfg.key = Some("key-second".to_string());
    let second = Messages::new("session-2", &second_cfg).unwrap();

    assert_eq!(first.name(), "session-1");
    assert_eq!(second.name(), "session-2");

    let (result, _) = run_turn(&first, request("model-1", user_only())).await;
    assert_eq!(result.expect("first turn").text, "first");
    let (result, _) = run_turn(&second, request("model-2", user_only())).await;
    assert_eq!(result.expect("second turn").text, "second");

    assert_eq!(
        mock.connections(),
        1,
        "two sequential sessions must reuse the endpoint's pool"
    );
    assert_eq!(mock.request(0).header("x-api-key"), Some("key-first"));
    assert_eq!(mock.request(1).header("x-api-key"), Some("key-second"));
}

// ---------------------------------------------------------------------------
// extra_body comes from the config (PooledClient::from_config)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extra_body_is_applied_from_config() {
    let mock = Mock::start(vec![ScriptMessage::text("ok").scripted()]).await;
    let mut cfg = mock.config(1000);
    let mut extra = serde_json::Map::new();
    extra.insert("custom".to_string(), json!({"k": "v"}));
    cfg.extra_body = Some(extra);

    let provider = Messages::new("anthropic", &cfg).unwrap();
    run_turn(&provider, request("claude-x", user_only()))
        .await
        .0
        .expect("turn");

    let body = mock.request(0).json();
    assert_eq!(body["custom"], json!({"k": "v"}));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["model"], json!("claude-x"));
}

// ---------------------------------------------------------------------------
// Messages-specific SSE framing (fixtures under tests/fixtures/sse/messages)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn blank_line_dispatch_skips_untyped_empty_events() {
    let (result, deltas) = run_fixture("blank_lines_and_empty_events.sse").await;
    let result = result.expect("untyped empty events are dispatched and ignored");
    assert_eq!(result.text, "blank");
    assert_eq!(deltas, "blank");
}

#[tokio::test]
async fn multiple_data_lines_concatenate_with_newline() {
    let (result, _) = run_fixture("split_data_lines.sse").await;
    assert_eq!(result.expect("split data lines").text, "split");

    let (result, _) = run_fixture("two_data_lines_invalid.sse").await;
    assert!(
        result.is_err(),
        "concatenated data lines must be decoded as one JSON document and fail"
    );
}

#[tokio::test]
async fn done_is_ordinary_data() {
    let (result, _) = run_fixture("done_as_data.sse").await;
    assert!(
        result.is_err(),
        "[DONE] is ordinary data: decoding it as an event must fail"
    );
}

#[tokio::test]
async fn empty_event_with_a_known_type_is_dispatched() {
    let (result, _) = run_fixture("empty_event_dispatched.sse").await;
    assert!(
        result.is_err(),
        "an empty event must be dispatched and fail JSON decoding"
    );
}

#[tokio::test]
async fn eof_without_message_delta_fails() {
    let (result, _) = run_fixture("eof_without_message_delta.sse").await;
    let error = result.expect_err("EOF without message_delta must fail");
    assert!(
        error.message.contains("stop signal"),
        "want a missing-stop-signal error, got {:?}",
        error.message
    );
    assert!(
        error.message.contains("partial"),
        "want the streamed partial text, got {:?}",
        error.message
    );
}

#[tokio::test]
async fn eof_with_stop_reason_without_message_stop_succeeds() {
    let (result, _) = run_fixture("stop_reason_without_message_stop.sse").await;
    let result = result.expect("EOF with a non-empty stop_reason is a complete turn");
    assert_eq!(result.text, "done");
}

#[tokio::test]
async fn stream_error_event_fails() {
    let (result, _) = run_fixture("error_event.sse").await;
    let error = result.expect_err("an error event must fail the turn");
    assert!(
        error.message.contains("upstream overloaded"),
        "want the streamed error body, got {:?}",
        error.message
    );
}

#[tokio::test]
async fn line_longer_than_32_mib_fails() {
    const CAP: usize = 32 * 1024 * 1024;
    let mut body = b"event: message_start\ndata: ".to_vec();
    body.extend(std::iter::repeat_n(b'x', CAP + 1));
    body.extend_from_slice(b"\n\n");
    let mock = Mock::start(vec![Scripted::sse(body)]).await;
    let (result, _) = one_turn(&mock, 1000, request("claude-x", user_only())).await;
    let error = result.expect_err("a line over the 32 MiB cap must fail");
    assert!(
        error.message.contains("token too long"),
        "want a scanner cap error, got {:?}",
        error.message
    );
}
