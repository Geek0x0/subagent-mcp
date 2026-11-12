//! Task 11: Responses adapter wire tests.
//!
//! Ports `internal/provider/responses/{adapter,env_credentials,shared_transport}_test.go`
//! and adds the SSE framing cases the Rust adapter owns: blank-line dispatch,
//! comment/empty-event skipping, `data:` line joining, the 32 MiB line cap,
//! EOF without a trailing blank line, and `[DONE]` ending the stream without
//! counting as a terminal response event.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::value::RawValue;
use serde_json::{Value, json};
use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::provider::responses::Responses;
use subagent_mcp::provider::sse::SSE_MAX_LINE;
use subagent_mcp::provider::{
    DeltaSink, Message, ModelLister, Provider, ProviderError, Role, ToolSpec, TurnRequest,
    TurnResult, Usage,
};
use subagent_mcp::util::run::RunContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Scripted fake Responses API (stand-in for Go's testutil.FakeResponses)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Item {
    reasoning_summary: String,
    encrypted_content: String,
    message_text: String,
    message_refusal: String,
    function_call_id: String,
    function_name: String,
    function_arguments: String,
}

impl Item {
    fn text(text: &str) -> Self {
        Self {
            message_text: text.to_string(),
            ..Default::default()
        }
    }

    fn refusal(text: &str) -> Self {
        Self {
            message_refusal: text.to_string(),
            ..Default::default()
        }
    }

    fn reasoning(summary: &str, encrypted: &str) -> Self {
        Self {
            reasoning_summary: summary.to_string(),
            encrypted_content: encrypted.to_string(),
            ..Default::default()
        }
    }

    fn call(id: &str, name: &str, arguments: &str) -> Self {
        Self {
            function_call_id: id.to_string(),
            function_name: name.to_string(),
            function_arguments: arguments.to_string(),
            ..Default::default()
        }
    }

    fn render(&self, index: usize) -> Value {
        if !self.reasoning_summary.is_empty() || !self.encrypted_content.is_empty() {
            json!({
                "type": "reasoning",
                "id": format!("rs_{index}"),
                "summary": [{"type": "summary_text", "text": self.reasoning_summary.clone()}],
                "encrypted_content": self.encrypted_content.clone(),
            })
        } else if !self.message_text.is_empty() || !self.message_refusal.is_empty() {
            let content = if !self.message_text.is_empty() {
                json!([{"type": "output_text", "text": self.message_text.clone(), "annotations": []}])
            } else {
                json!([{"type": "refusal", "refusal": self.message_refusal.clone()}])
            };
            json!({
                "type": "message",
                "id": format!("msg_{index}"),
                "role": "assistant",
                "status": "completed",
                "content": content,
            })
        } else {
            json!({
                "type": "function_call",
                "id": format!("fc_{index}"),
                "call_id": self.function_call_id.clone(),
                "name": self.function_name.clone(),
                "arguments": self.function_arguments.clone(),
                "status": "completed",
            })
        }
    }
}

#[derive(Default)]
struct TurnScript {
    items: Vec<Item>,
    response_status: Option<&'static str>,
    incomplete_reason: String,
    input_tokens: i64,
    cached_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    stream_error: Option<(String, String)>,
}

enum Scripted {
    /// One scripted Responses turn rendered as SSE.
    Turn(TurnScript),
    /// Raw SSE bytes (framing cases).
    Raw(String),
    /// Non-200 JSON error reply.
    Http(u16),
}

fn text_turn(text: &str) -> Scripted {
    Scripted::Turn(TurnScript {
        items: vec![Item::text(text)],
        ..Default::default()
    })
}

struct Recorded {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Default)]
struct State {
    scripted: VecDeque<Scripted>,
    requests: Vec<Recorded>,
    models: Vec<String>,
    models_status: u16,
    connections: usize,
}

struct FakeResponses {
    base: String,
    state: Arc<Mutex<State>>,
}

impl FakeResponses {
    fn request(&self, index: usize) -> Value {
        let raw = self.raw_request(index);
        serde_json::from_slice(&raw).expect("request body must be JSON")
    }

    fn raw_request(&self, index: usize) -> Vec<u8> {
        let guard = self.state.lock().unwrap();
        guard.requests[index].body.clone()
    }

    fn headers(&self, index: usize) -> HashMap<String, String> {
        let guard = self.state.lock().unwrap();
        guard.requests[index].headers.clone()
    }

    fn method_and_path(&self, index: usize) -> (String, String) {
        let guard = self.state.lock().unwrap();
        (
            guard.requests[index].method.clone(),
            guard.requests[index].path.clone(),
        )
    }

    fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    fn connections(&self) -> usize {
        self.state.lock().unwrap().connections
    }

    fn set_models(&self, ids: &[&str]) {
        self.state.lock().unwrap().models = ids.iter().map(|id| id.to_string()).collect();
    }

    fn set_models_status(&self, status: u16) {
        self.state.lock().unwrap().models_status = status;
    }
}

async fn spawn_fake(scripted: Vec<Scripted>) -> FakeResponses {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = Arc::new(Mutex::new(State {
        scripted: scripted.into(),
        ..Default::default()
    }));
    let server_state = Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            server_state.lock().unwrap().connections += 1;
            let state = Arc::clone(&server_state);
            tokio::spawn(async move {
                serve_connection(socket, state).await;
            });
        }
    });
    FakeResponses {
        base: format!("http://{address}"),
        state,
    }
}

async fn serve_connection(mut socket: tokio::net::TcpStream, state: Arc<Mutex<State>>) {
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let header_end = loop {
            if let Some(index) = find(&buffer, b"\r\n\r\n") {
                break index + 4;
            }
            let mut chunk = [0u8; 8192];
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut headers = HashMap::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let length: usize = headers
            .get("content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        while buffer.len() < header_end + length {
            let mut chunk = [0u8; 8192];
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let body = buffer[header_end..header_end + length].to_vec();
        buffer.drain(..header_end + length);
        let (reply, keep_alive) = handle(&state, &method, &path, &headers, &body);
        if socket.write_all(&reply).await.is_err() || !keep_alive {
            return;
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn handle(
    state: &Arc<Mutex<State>>,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> (Vec<u8>, bool) {
    let request_index = {
        let mut guard = state.lock().unwrap();
        guard.requests.push(Recorded {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers.clone(),
            body: body.to_vec(),
        });
        guard.requests.len() - 1
    };

    match (method, path) {
        ("POST", "/responses") => {
            let scripted = state.lock().unwrap().scripted.pop_front();
            let Some(scripted) = scripted else {
                return (
                    http_response(
                        500,
                        "application/json",
                        br#"{"error":{"message":"unexpected extra request","type":"server_error"}}"#,
                    ),
                    true,
                );
            };
            match scripted {
                Scripted::Http(status) => (
                    http_response(
                        status,
                        "application/json",
                        br#"{"error":{"message":"scripted failure","type":"server_error"}}"#,
                    ),
                    true,
                ),
                Scripted::Raw(raw) => (
                    http_response(200, "text/event-stream", raw.as_bytes()),
                    true,
                ),
                Scripted::Turn(script) => {
                    let request: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                    let model = request
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let raw = render_turn(&script, &model, request_index);
                    (
                        http_response(200, "text/event-stream", raw.as_bytes()),
                        true,
                    )
                }
            }
        }
        ("GET", "/models") => {
            let (status, ids) = {
                let guard = state.lock().unwrap();
                (guard.models_status, guard.models.clone())
            };
            if status != 0 && status != 200 {
                return (
                    http_response(
                        status,
                        "application/json",
                        br#"{"error":{"message":"scripted failure","type":"server_error"}}"#,
                    ),
                    true,
                );
            }
            let data: Vec<Value> = ids
                .into_iter()
                .map(|id| json!({"id": id, "object": "model"}))
                .collect();
            let body = serde_json::to_vec(&json!({"object": "list", "data": data})).unwrap();
            (http_response(200, "application/json", &body), true)
        }
        _ => (http_response(404, "text/plain", b"not found"), true),
    }
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let mut raw = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    raw
}

fn render_turn(script: &TurnScript, model: &str, request_index: usize) -> String {
    let mut out = String::new();
    let mut sequence = 0;
    let response_id = format!("resp_{request_index}");
    let created = json!({
        "id": response_id.clone(),
        "object": "response",
        "created_at": 1700000000,
        "status": "in_progress",
        "model": model,
        "output": [],
    });
    push_event(
        &mut out,
        "response.created",
        &json!({"sequence_number": sequence, "response": created}),
    );
    sequence += 1;

    let mut output = Vec::new();
    for (index, item) in script.items.iter().enumerate() {
        if !item.message_text.is_empty() {
            push_event(
                &mut out,
                "response.output_text.delta",
                &json!({
                    "sequence_number": sequence,
                    "item_id": format!("msg_{index}"),
                    "output_index": index,
                    "content_index": 0,
                    "delta": item.message_text.clone(),
                }),
            );
            sequence += 1;
        }
        output.push(item.render(index));
    }

    if let Some((code, message)) = &script.stream_error {
        push_event(
            &mut out,
            "error",
            &json!({
                "sequence_number": sequence,
                "code": code,
                "message": message,
            }),
        );
        return out;
    }

    let status = script.response_status.unwrap_or("completed");
    let mut response = json!({
        "id": response_id,
        "object": "response",
        "created_at": 1700000000,
        "status": status,
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": script.input_tokens,
            "input_tokens_details": {"cached_tokens": script.cached_tokens},
            "output_tokens": script.output_tokens,
            "output_tokens_details": {"reasoning_tokens": script.reasoning_tokens},
            "total_tokens": script.input_tokens + script.output_tokens,
        },
    });
    if status == "incomplete" {
        response["incomplete_details"] = json!({"reason": script.incomplete_reason.clone()});
    }
    if status == "failed" {
        response["error"] = json!({"code": "server_error", "message": "scripted failure"});
    }
    push_event(
        &mut out,
        &format!("response.{status}"),
        &json!({"sequence_number": sequence, "response": response}),
    );
    out
}

fn push_event(out: &mut String, kind: &str, payload: &Value) {
    // Go's fake writes the event type into the payload as well as the SSE
    // `event:` line; the adapter switches on the payload's `type`.
    let mut payload = payload.clone();
    if let Some(object) = payload.as_object_mut() {
        object.insert("type".to_string(), json!(kind));
    }
    out.push_str(&format!("event: {kind}\ndata: {payload}\n\n"));
}

// ---------------------------------------------------------------------------
// Adapter helpers
// ---------------------------------------------------------------------------

fn provider_config(base: &str, key: &str) -> ProviderConfig {
    ProviderConfig {
        api: Api::Responses,
        base_url: base.to_string(),
        env_key: None,
        key: Some(key.to_string()),
        command: None,
        default_model: "m".to_string(),
        models: Vec::new(),
        effort_map: BTreeMap::new(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    }
}

fn adapter(fake: &FakeResponses, key: &str) -> Responses {
    Responses::new("openai", &provider_config(&fake.base, key)).unwrap()
}

fn shell_spec() -> ToolSpec {
    ToolSpec {
        name: "shell".to_string(),
        description: "run".to_string(),
        parameters: RawValue::from_string(
            r#"{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}"#
                .to_string(),
        )
        .unwrap(),
    }
}

fn context() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn request(model: &str) -> TurnRequest {
    TurnRequest {
        model: model.to_string(),
        messages: vec![Message {
            role: Role::User,
            text: "hi".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

async fn run_turn(provider: &Responses, request: TurnRequest) -> Result<TurnResult, ProviderError> {
    provider.turn(context(), request, Box::new(|_| {})).await
}

fn item_kind(item: &Value) -> String {
    match item.get("type").and_then(Value::as_str) {
        None | Some("") | Some("message") => format!(
            "message:{}",
            item.get("role").and_then(Value::as_str).unwrap_or("")
        ),
        Some(kind) => kind.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Ported Go adapter tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_turn_sends_the_full_request_and_reports_usage() {
    let fake = spawn_fake(vec![Scripted::Turn(TurnScript {
        items: vec![Item::text("hello")],
        input_tokens: 10,
        cached_tokens: 4,
        output_tokens: 3,
        reasoning_tokens: 1,
        ..Default::default()
    })])
    .await;
    let provider = adapter(&fake, "k");

    let deltas = Arc::new(Mutex::new(String::new()));
    let sink: DeltaSink = {
        let deltas = Arc::clone(&deltas);
        Box::new(move |delta| deltas.lock().unwrap().push_str(&delta))
    };
    let result = provider
        .turn(
            context(),
            TurnRequest {
                model: "gpt-x".to_string(),
                effort: Some("high".to_string()),
                system: "sys".to_string(),
                messages: vec![Message {
                    role: Role::User,
                    text: "hi".to_string(),
                    ..Default::default()
                }],
                tools: vec![shell_spec()],
            },
            sink,
        )
        .await
        .unwrap();

    assert_eq!(result.text, "hello");
    assert_eq!(*deltas.lock().unwrap(), "hello");
    assert_eq!(
        result.usage,
        Some(Usage {
            input: 10,
            cached: 4,
            output: 3,
            reasoning: 1,
            total: 13,
        })
    );

    let body = fake.request(0);
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["instructions"], json!("sys"));
    assert_eq!(body["model"], json!("gpt-x"));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(
        body["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
    assert_eq!(body["input"], json!([{"content": "hi", "role": "user"}]));
    assert_eq!(
        body["max_output_tokens"],
        Value::Null,
        "an unset limit must stay off the wire"
    );

    let tool = &body["tools"][0];
    assert_eq!(tool["type"], json!("function"));
    assert_eq!(tool["name"], json!("shell"));
    assert_eq!(tool["description"], json!("run"));
    assert_eq!(tool["strict"], json!(false));
    assert!(tool["parameters"].is_object(), "raw schema must be sent");

    let headers = fake.headers(0);
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer k")
    );
    assert!(
        headers.get("accept").is_some_and(|value| !value.is_empty()),
        "standard Accept header missing"
    );
    assert_eq!(
        fake.method_and_path(0),
        ("POST".to_string(), "/responses".to_string())
    );
}

#[tokio::test]
async fn replays_every_turn_with_opaque_items() {
    let fake = spawn_fake(vec![
        Scripted::Turn(TurnScript {
            items: vec![
                Item::reasoning("plan 1", "enc-1"),
                Item::call("call_1", "shell", r#"{"command":"ls"}"#),
                Item::call("call_2", "shell", r#"{"command":"pwd"}"#),
            ],
            ..Default::default()
        }),
        Scripted::Turn(TurnScript {
            items: vec![
                Item::reasoning("plan 2", "enc-2"),
                Item::call("call_3", "shell", r#"{"command":"cat a"}"#),
            ],
            ..Default::default()
        }),
        text_turn("done"),
    ])
    .await;
    let provider = adapter(&fake, "k");

    let mut history = vec![Message {
        role: Role::User,
        text: "go".to_string(),
        ..Default::default()
    }];
    for turn in 0..3 {
        let result = run_turn(
            &provider,
            TurnRequest {
                model: "gpt-x".to_string(),
                messages: history.clone(),
                tools: vec![shell_spec()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if turn == 0 {
            assert_eq!(result.reasoning.as_deref(), Some("plan 1"));
            assert_eq!(result.tool_calls.len(), 2);
            assert_eq!(result.tool_calls[1].id, "call_2");
            assert_eq!(result.tool_calls[0].arguments, r#"{"command":"ls"}"#);
        }
        history.push(Message {
            role: Role::Assistant,
            text: result.text.clone(),
            tool_calls: result.tool_calls.clone(),
            opaque: result.opaque.clone(),
            ..Default::default()
        });
        for call in &result.tool_calls {
            history.push(Message {
                role: Role::Tool,
                tool_call_id: Some(call.id.clone()),
                text: format!("out-{}", call.id),
                ..Default::default()
            });
        }
    }

    let body = fake.request(2);
    let input = body["input"].as_array().unwrap();
    let kinds: Vec<String> = input.iter().map(item_kind).collect();
    assert_eq!(
        kinds,
        vec![
            "message:user",
            "reasoning",
            "function_call",
            "function_call",
            "function_call_output",
            "function_call_output",
            "reasoning",
            "function_call",
            "function_call_output",
        ]
    );
    assert_eq!(input[1]["encrypted_content"], json!("enc-1"));
    assert_eq!(input[6]["encrypted_content"], json!("enc-2"));
    assert_eq!(input[4]["call_id"], json!("call_1"));
    assert_eq!(input[4]["output"], json!("out-call_1"));
    assert_eq!(input[4]["type"], json!("function_call_output"));
}

/// An assistant turn's opaque items are replayed as the exact bytes the
/// terminal response carried, not re-encoded from a parsed value.
#[tokio::test]
async fn assistant_opaque_replays_item_bytes_verbatim() {
    const RAW: &str = "event: response.completed\ndata: {\"type\":\"response.completed\",\"sequence_number\":0,\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"encrypted_content\":\"enc-1\",\"id\":\"rs_0\",\"summary\":[{\"text\":\"p\",\"type\":\"summary_text\"}]}],\"usage\":{}}}\n\n";
    let fake = spawn_fake(vec![Scripted::Raw(RAW.to_string()), text_turn("ok")]).await;
    let provider = adapter(&fake, "k");

    let first = run_turn(&provider, request("gpt-x")).await.unwrap();
    assert_eq!(
        first.opaque.as_deref().map(RawValue::get),
        Some(
            r#"[{"type":"reasoning","encrypted_content":"enc-1","id":"rs_0","summary":[{"text":"p","type":"summary_text"}]}]"#
        )
    );

    run_turn(
        &provider,
        TurnRequest {
            model: "gpt-x".to_string(),
            messages: vec![
                Message {
                    role: Role::User,
                    text: "go".to_string(),
                    ..Default::default()
                },
                Message {
                    role: Role::Assistant,
                    opaque: first.opaque.clone(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let raw = String::from_utf8(fake.raw_request(1)).unwrap();
    let item = r#"{"type":"reasoning","encrypted_content":"enc-1","id":"rs_0","summary":[{"text":"p","type":"summary_text"}]}"#;
    assert!(raw.contains(item), "replayed item bytes changed:\n{raw}");
}

#[tokio::test]
async fn incomplete_and_failed_responses_use_the_go_error_text() {
    let incomplete = spawn_fake(vec![Scripted::Turn(TurnScript {
        response_status: Some("incomplete"),
        incomplete_reason: "max_output_tokens".to_string(),
        ..Default::default()
    })])
    .await;
    let error = run_turn(&adapter(&incomplete, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: response incomplete: max_output_tokens"
    );

    let failed = spawn_fake(vec![Scripted::Turn(TurnScript {
        response_status: Some("failed"),
        ..Default::default()
    })])
    .await;
    let error = run_turn(&adapter(&failed, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: response failed: scripted failure"
    );
}

#[tokio::test]
async fn refusal_part_fails_the_turn() {
    let fake = spawn_fake(vec![Scripted::Turn(TurnScript {
        items: vec![Item::refusal("I cannot help with that.")],
        ..Default::default()
    })])
    .await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: model refused: I cannot help with that."
    );
}

#[tokio::test]
async fn stream_error_event_reports_code_and_message() {
    let fake = spawn_fake(vec![Scripted::Turn(TurnScript {
        items: vec![Item::text("partial")],
        stream_error: Some((
            "invalid_request_error".to_string(),
            "scripted stream failure".to_string(),
        )),
        ..Default::default()
    })])
    .await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: invalid_request_error: scripted stream failure"
    );
}

#[tokio::test]
async fn http_error_fails_the_turn() {
    let fake = spawn_fake(vec![Scripted::Http(400)]).await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("400") && error.message.contains("scripted failure"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn corrupt_replay_payload_is_rejected_before_sending() {
    let fake = spawn_fake(Vec::new()).await;
    let error = run_turn(
        &adapter(&fake, "k"),
        TurnRequest {
            model: "gpt-x".to_string(),
            messages: vec![Message {
                role: Role::Assistant,
                ..Default::default()
            }],
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        error
            .message
            .starts_with("responses: corrupt replay payload:"),
        "unexpected error: {}",
        error.message
    );
    assert_eq!(fake.request_count(), 0, "no request must be sent");
}

#[tokio::test]
async fn a_tool_schema_that_is_not_an_object_is_rejected() {
    let fake = spawn_fake(Vec::new()).await;
    let spec = ToolSpec {
        name: "bad".to_string(),
        description: String::new(),
        parameters: RawValue::from_string("[]".to_string()).unwrap(),
    };
    let error = run_turn(
        &adapter(&fake, "k"),
        TurnRequest {
            model: "gpt-x".to_string(),
            messages: vec![Message {
                role: Role::User,
                text: "hi".to_string(),
                ..Default::default()
            }],
            tools: vec![spec],
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.message,
        "responses: tool bad schema: not a JSON object"
    );
    assert_eq!(fake.request_count(), 0);
}

#[tokio::test]
async fn list_models_returns_ids() {
    let fake = spawn_fake(Vec::new()).await;
    fake.set_models(&["model-a", "model-b", "model-c"]);
    let provider = adapter(&fake, "k");
    let ids: Vec<String> = ModelLister::list_models(&provider)
        .await
        .unwrap()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(ids, vec!["model-a", "model-b", "model-c"]);
    assert_eq!(
        fake.method_and_path(0),
        ("GET".to_string(), "/models".to_string())
    );
    assert_eq!(
        fake.headers(0).get("authorization").map(String::as_str),
        Some("Bearer k")
    );
}

#[tokio::test]
async fn list_models_http_error_is_wrapped_with_list_models() {
    let fake = spawn_fake(Vec::new()).await;
    fake.set_models_status(401);
    let error = ModelLister::list_models(&adapter(&fake, "k"))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("list models") && error.message.contains("401"),
        "unexpected error: {}",
        error.message
    );
}

#[tokio::test]
async fn configured_max_output_tokens_reaches_the_wire() {
    for (configure, want) in [
        (Some(4096), Some(json!(4096))),
        (None, None),
        (Some(0), None),
    ] {
        let fake = spawn_fake(vec![text_turn("ok")]).await;
        let mut cfg = provider_config(&fake.base, "k");
        cfg.max_output_tokens = configure;
        let provider = Responses::new("openai", &cfg).unwrap();
        run_turn(&provider, request("m")).await.unwrap();
        let body = fake.request(0);
        match want {
            None => assert_eq!(
                body["max_output_tokens"],
                Value::Null,
                "configure = {configure:?} must leave the field out"
            ),
            Some(value) => assert_eq!(body["max_output_tokens"], value),
        }
    }
}

#[tokio::test]
async fn provider_name_is_the_configured_name() {
    let fake = spawn_fake(Vec::new()).await;
    let provider = Responses::new("mine", &provider_config(&fake.base, "k")).unwrap();
    assert_eq!(provider.name(), "mine");
}

// ---------------------------------------------------------------------------
// SSE framing
// ---------------------------------------------------------------------------

const TEXT_COMPLETED: &str = include_str!("fixtures/sse/responses/text_completed.sse");
const FRAMING_MULTI_DATA: &str = include_str!("fixtures/sse/responses/framing_multi_data.sse");
const DONE_PREFIX: &str = include_str!("fixtures/sse/responses/done_prefix.sse");
const EOF_WITHOUT_BLANK: &str = include_str!("fixtures/sse/responses/eof_without_blank.sse");

#[tokio::test]
async fn terminal_event_then_done_completes_the_turn() {
    let fake = spawn_fake(vec![Scripted::Raw(TEXT_COMPLETED.to_string())]).await;
    let result = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap();
    assert_eq!(result.text, "hello");
    assert_eq!(
        result.usage,
        Some(Usage {
            input: 10,
            cached: 4,
            output: 3,
            reasoning: 1,
            total: 13,
        })
    );
}

#[tokio::test]
async fn blank_line_dispatch_skips_comments_and_empty_events() {
    let fake = spawn_fake(vec![Scripted::Raw(FRAMING_MULTI_DATA.to_string())]).await;
    let deltas = Arc::new(Mutex::new(String::new()));
    let sink: DeltaSink = {
        let deltas = Arc::clone(&deltas);
        Box::new(move |delta| deltas.lock().unwrap().push_str(&delta))
    };
    let result = adapter(&fake, "k")
        .turn(context(), request("gpt-x"), sink)
        .await
        .unwrap();
    assert_eq!(result.text, "hello");
    assert_eq!(
        *deltas.lock().unwrap(),
        "hello",
        "data: lines must be joined with a newline and comments/empty events skipped"
    );
    assert_eq!(result.usage.map(|usage| usage.total), Some(2));
}

#[tokio::test]
async fn data_beginning_with_done_ends_the_stream_but_is_not_terminal() {
    let fake = spawn_fake(vec![Scripted::Raw(DONE_PREFIX.to_string())]).await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: stream ended without a terminal response event"
    );
}

#[tokio::test]
async fn eof_without_a_trailing_blank_line_drops_the_pending_event() {
    let fake = spawn_fake(vec![Scripted::Raw(EOF_WITHOUT_BLANK.to_string())]).await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert_eq!(
        error.message,
        "responses: stream ended without a terminal response event"
    );
}

#[tokio::test]
async fn a_line_over_the_32_mib_cap_fails_the_stream() {
    let mut body = String::from("data: {\"type\":\"response.output_text.delta\",\"delta\":\"");
    body.push_str(&"x".repeat(SSE_MAX_LINE + 1));
    body.push_str("\"}\n\n");
    let fake = spawn_fake(vec![Scripted::Raw(body)]).await;
    let error = run_turn(&adapter(&fake, "k"), request("gpt-x"))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("maximum"),
        "unexpected error: {}",
        error.message
    );
}

// ---------------------------------------------------------------------------
// Credentials and connection sharing
// ---------------------------------------------------------------------------

struct EnvRestore(Vec<(String, Option<String>)>);

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            // SAFETY: this test binary only mutates these variables in this
            // test, and the guard restores every value before the test ends.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[tokio::test]
async fn credentials_never_come_from_the_environment() {
    let hostile = [
        ("ANTHROPIC_API_KEY", "sk-ant-env-DIFFERENT-key"),
        ("ANTHROPIC_AUTH_TOKEN", "sk-ant-oat-SECRET-from-env"),
        (
            "ANTHROPIC_CUSTOM_HEADERS",
            "X-Corp-Gateway-Token: gw-secret",
        ),
        ("ANTHROPIC_BASE_URL", "http://127.0.0.1:1/elsewhere"),
        ("OPENAI_API_KEY", "sk-openai-env-DIFFERENT-key"),
        ("OPENAI_ADMIN_KEY", "sk-admin-env-SECRET"),
        ("OPENAI_ORG_ID", "org-real"),
        ("OPENAI_PROJECT_ID", "proj-real"),
        ("OPENAI_CUSTOM_HEADERS", "X-Corp-Gateway-Token: gw-SECRET"),
        ("OPENAI_BASE_URL", "http://127.0.0.1:1/elsewhere"),
    ];
    let restore = EnvRestore(
        hostile
            .iter()
            .map(|(key, _)| (key.to_string(), std::env::var(key).ok()))
            .collect(),
    );
    for (key, value) in hostile {
        // SAFETY: serialized by ENV_LOCK and restored by `restore`.
        unsafe { std::env::set_var(key, value) };
    }

    let fake = spawn_fake(vec![text_turn("hello")]).await;
    fake.set_models(&["model-a"]);
    let provider = Responses::new("openai", &provider_config(&fake.base, "cfg-key-0123")).unwrap();

    let result = run_turn(&provider, request("gpt-x")).await.unwrap();
    assert_eq!(result.text, "hello");
    let ids: Vec<String> = ModelLister::list_models(&provider)
        .await
        .unwrap()
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(ids, vec!["model-a"]);
    assert_eq!(
        fake.request_count(),
        2,
        "the configured server must see exactly the turn and the model listing"
    );

    let secrets = [
        "sk-ant-env-DIFFERENT-key",
        "sk-ant-oat-SECRET-from-env",
        "gw-secret",
        "sk-openai-env-DIFFERENT-key",
        "sk-admin-env-SECRET",
        "org-real",
        "proj-real",
        "gw-SECRET",
    ];
    for index in 0..2 {
        let headers = fake.headers(index);
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer cfg-key-0123")
        );
        for forbidden in [
            "x-corp-gateway-token",
            "openai-organization",
            "openai-project",
        ] {
            assert!(
                !headers.contains_key(forbidden),
                "environment-derived header {forbidden} reached the wire"
            );
        }
        for value in headers.values() {
            for secret in secrets {
                assert!(
                    !value.contains(secret),
                    "environment value {secret} leaked into a header"
                );
            }
        }
    }

    drop(restore);
}

#[tokio::test]
async fn sequential_sessions_share_one_connection_with_their_own_keys() {
    let fake = spawn_fake(vec![text_turn("first"), text_turn("second")]).await;
    let first = adapter(&fake, "key-first");
    let second = adapter(&fake, "key-second");

    let result = run_turn(&first, request("model-1")).await.unwrap();
    assert_eq!(result.text, "first");
    let result = run_turn(&second, request("model-2")).await.unwrap();
    assert_eq!(result.text, "second");

    assert_eq!(
        fake.connections(),
        1,
        "two sequential sessions must reuse the endpoint's pooled connection"
    );
    assert_eq!(
        fake.headers(0).get("authorization").map(String::as_str),
        Some("Bearer key-first")
    );
    assert_eq!(
        fake.headers(1).get("authorization").map(String::as_str),
        Some("Bearer key-second")
    );
}
