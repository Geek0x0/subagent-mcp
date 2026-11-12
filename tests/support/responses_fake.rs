//! Shared scripted Responses API fake for the Task 11 wire tests.
//!
//! Stand-in for Go's `internal/testutil.FakeResponses`: it records every
//! request, serves scripted SSE turns or raw bodies, lists models, and counts
//! TCP connections so the pool-reuse contract is observable. This file lives
//! under `tests/support/` so it is compiled into the test binaries that
//! `#[path]`-include it, not treated as a test target of its own.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::provider::responses::Responses;
use subagent_mcp::provider::{
    Message, Provider, ProviderError, Role, ToolSpec, TurnRequest, TurnResult,
};
use subagent_mcp::util::run::RunContext;

#[derive(Clone, Default)]
pub struct Item {
    pub reasoning_summary: String,
    pub encrypted_content: String,
    pub message_text: String,
    pub message_refusal: String,
    pub function_call_id: String,
    pub function_name: String,
    pub function_arguments: String,
}

impl Item {
    pub fn text(text: &str) -> Self {
        Self {
            message_text: text.to_string(),
            ..Default::default()
        }
    }

    pub fn refusal(text: &str) -> Self {
        Self {
            message_refusal: text.to_string(),
            ..Default::default()
        }
    }

    pub fn reasoning(summary: &str, encrypted: &str) -> Self {
        Self {
            reasoning_summary: summary.to_string(),
            encrypted_content: encrypted.to_string(),
            ..Default::default()
        }
    }

    pub fn call(id: &str, name: &str, arguments: &str) -> Self {
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
pub struct TurnScript {
    pub items: Vec<Item>,
    pub response_status: Option<&'static str>,
    pub incomplete_reason: String,
    pub input_tokens: i64,
    pub cached_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub stream_error: Option<(String, String)>,
}

pub enum Scripted {
    /// One scripted Responses turn rendered as SSE.
    Turn(TurnScript),
    /// Raw SSE bytes (framing cases).
    Raw(String),
    /// Non-200 JSON error reply with optional extra response headers.
    Http {
        status: u16,
        headers: Vec<(String, String)>,
    },
}

pub fn text_turn(text: &str) -> Scripted {
    Scripted::Turn(TurnScript {
        items: vec![Item::text(text)],
        ..Default::default()
    })
}

pub fn http_error(status: u16) -> Scripted {
    Scripted::Http {
        status,
        headers: Vec::new(),
    }
}

pub fn http_error_with(status: u16, headers: &[(&str, &str)]) -> Scripted {
    Scripted::Http {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
    }
}

pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Default)]
pub struct State {
    scripted: VecDeque<Scripted>,
    requests: Vec<Recorded>,
    models: Vec<String>,
    models_failure_status: u16,
    models_failures: usize,
    connections: usize,
}

pub struct FakeResponses {
    pub base: String,
    state: Arc<Mutex<State>>,
}

impl FakeResponses {
    pub fn request(&self, index: usize) -> Value {
        let raw = self.raw_request(index);
        serde_json::from_slice(&raw).expect("request body must be JSON")
    }

    pub fn raw_request(&self, index: usize) -> Vec<u8> {
        let guard = self.state.lock().unwrap();
        guard.requests[index].body.clone()
    }

    pub fn headers(&self, index: usize) -> HashMap<String, String> {
        let guard = self.state.lock().unwrap();
        guard.requests[index].headers.clone()
    }

    pub fn method_and_path(&self, index: usize) -> (String, String) {
        let guard = self.state.lock().unwrap();
        (
            guard.requests[index].method.clone(),
            guard.requests[index].path.clone(),
        )
    }

    pub fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    pub fn connections(&self) -> usize {
        self.state.lock().unwrap().connections
    }

    pub fn set_models(&self, ids: &[&str]) {
        self.state.lock().unwrap().models = ids.iter().map(|id| id.to_string()).collect();
    }

    /// Every `GET /models` fails with `status`.
    pub fn set_models_status(&self, status: u16) {
        let mut guard = self.state.lock().unwrap();
        guard.models_failure_status = status;
        guard.models_failures = usize::MAX;
    }

    /// The next `count` `GET /models` requests fail with `status`, then succeed.
    pub fn set_models_transient_failures(&self, status: u16, count: usize) {
        let mut guard = self.state.lock().unwrap();
        guard.models_failure_status = status;
        guard.models_failures = count;
    }
}

pub async fn spawn_fake(scripted: Vec<Scripted>) -> FakeResponses {
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

/// A TCP listener that accepts and immediately closes every connection, for
/// exercising the SDK's connection-error retries.
pub async fn spawn_closing_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&attempts);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    (format!("http://{address}"), attempts)
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
                        &[],
                    ),
                    true,
                );
            };
            match scripted {
                Scripted::Http { status, headers } => (
                    http_response(
                        status,
                        "application/json",
                        br#"{"error":{"message":"scripted failure","type":"server_error"}}"#,
                        &headers,
                    ),
                    true,
                ),
                Scripted::Raw(raw) => (
                    http_response(200, "text/event-stream", raw.as_bytes(), &[]),
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
                        http_response(200, "text/event-stream", raw.as_bytes(), &[]),
                        true,
                    )
                }
            }
        }
        ("GET", "/models") => {
            let (status, ids) = {
                let mut guard = state.lock().unwrap();
                let status = if guard.models_failures > 0 {
                    guard.models_failures = guard.models_failures.saturating_sub(1);
                    guard.models_failure_status
                } else {
                    0
                };
                (status, guard.models.clone())
            };
            if status != 0 && status != 200 {
                return (
                    http_response(
                        status,
                        "application/json",
                        br#"{"error":{"message":"scripted failure","type":"server_error"}}"#,
                        &[],
                    ),
                    true,
                );
            }
            let data: Vec<Value> = ids
                .into_iter()
                .map(|id| json!({"id": id, "object": "model"}))
                .collect();
            let body = serde_json::to_vec(&json!({"object": "list", "data": data})).unwrap();
            (http_response(200, "application/json", &body, &[]), true)
        }
        _ => (http_response(404, "text/plain", b"not found", &[]), true),
    }
}

fn http_response(
    status: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(String, String)],
) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut raw = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n",
        body.len()
    )
    .into_bytes();
    for (name, value) in extra_headers {
        raw.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    raw.extend_from_slice(b"\r\n");
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

pub fn provider_config(base: &str, key: &str) -> ProviderConfig {
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

pub fn adapter(fake: &FakeResponses, key: &str) -> Responses {
    Responses::new("openai", &provider_config(&fake.base, key)).unwrap()
}

/// An adapter whose retry waits are zero, so retry tests do not sleep.
pub fn fast_adapter(fake: &FakeResponses) -> Responses {
    let mut provider = adapter(fake, "k");
    provider.set_retry_delay(Arc::new(|_, _| Duration::ZERO));
    provider
}

pub fn shell_spec() -> ToolSpec {
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

pub fn context() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

pub fn request(model: &str) -> TurnRequest {
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

pub async fn run_turn(
    provider: &Responses,
    request: TurnRequest,
) -> Result<TurnResult, ProviderError> {
    provider.turn(context(), request, Box::new(|_| {})).await
}

pub fn item_kind(item: &Value) -> String {
    match item.get("type").and_then(Value::as_str) {
        None | Some("") | Some("message") => format!(
            "message:{}",
            item.get("role").and_then(Value::as_str).unwrap_or("")
        ),
        Some(kind) => kind.to_string(),
    }
}
