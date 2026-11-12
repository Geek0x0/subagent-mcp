//! Shared wire-test support for the Messages adapter.
//!
//! `tests/wire_messages.rs` and `tests/messages_env_credentials.rs` are separate
//! test binaries so the credential test can mutate the process environment
//! without racing another test's `std::env::var` calls (that race is undefined
//! behavior in Rust 2024). Each binary uses a different subset of these helpers,
//! so unused items are allowed here.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::provider::messages::Messages;
use subagent_mcp::provider::{DeltaSink, Provider, ProviderError, Role, TurnRequest, TurnResult};
use subagent_mcp::util::run::RunContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Local HTTP mock (stand-in for Go's httptest.Server + testutil.FakeMessages)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Scripted {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    drop_connection: bool,
    stall: bool,
}

impl Scripted {
    pub fn sse(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream".into(),
            body: body.into(),
            headers: Vec::new(),
            drop_connection: false,
            stall: false,
        }
    }

    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "application/json".into(),
            body: body.into(),
            headers: Vec::new(),
            drop_connection: false,
            stall: false,
        }
    }

    pub fn status(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "application/json".into(),
            body: body.into(),
            headers: Vec::new(),
            drop_connection: false,
            stall: false,
        }
    }

    /// Close the connection after recording the request, without any response.
    pub fn drop_connection() -> Self {
        Self {
            status: 0,
            content_type: String::new(),
            body: Vec::new(),
            headers: Vec::new(),
            drop_connection: true,
            stall: false,
        }
    }

    /// Write response headers declaring a body longer than `prefix`, send the
    /// prefix, then keep the socket open: a stream that never finishes.
    pub fn stall(prefix: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream".into(),
            body: prefix.into(),
            headers: Vec::new(),
            drop_connection: false,
            stall: true,
        }
    }

    /// A non-2xx response whose body never finishes: the retry path must not
    /// read it before retrying.
    pub fn stall_status(status: u16, prefix: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "application/json".into(),
            body: prefix.into(),
            headers: Vec::new(),
            drop_connection: false,
            stall: true,
        }
    }

    pub fn with_headers(mut self, headers: &[(&str, &str)]) -> Self {
        self.headers = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        self
    }
}

#[derive(Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("recorded request body is JSON")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
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

pub struct Mock {
    pub base: String,
    state: Arc<Mutex<MockState>>,
}

impl Mock {
    pub async fn start(scripts: Vec<Scripted>) -> Self {
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
                        if script.drop_connection {
                            break;
                        }
                        if write_response(&mut socket, &script).await.is_err() {
                            break;
                        }
                        if script.stall {
                            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        }
                    }
                });
            }
        });
        Self { base, state }
    }

    pub fn config(&self, max_output_tokens: i64) -> ProviderConfig {
        ProviderConfig {
            api: Api::Messages,
            base_url: self.base.clone(),
            env_key: None,
            key: Some("k".to_string()),
            command: None,
            default_model: "claude-x".to_string(),
            models: Vec::new(),
            effort_map: std::collections::BTreeMap::new(),
            max_output_tokens: Some(max_output_tokens),
            max_nudges: None,
            extra_body: None,
        }
    }

    pub fn request(&self, index: usize) -> Recorded {
        let locked = self.state.lock().unwrap();
        locked.requests.get(index).cloned().unwrap_or_else(|| {
            panic!(
                "request index {index} out of range; recorded {}",
                locked.requests.len()
            )
        })
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    pub fn connections(&self) -> usize {
        self.state.lock().unwrap().connections
    }

    pub fn unexpected(&self) -> usize {
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
        408 => "Request Timeout",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    // A stalled response declares more bytes than it sends, so the client keeps
    // waiting for the rest of the body.
    let declared = if script.stall {
        script.body.len() + 1_000_000
    } else {
        script.body.len()
    };
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\n",
        script.status, reason, script.content_type
    );
    for (name, value) in &script.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {declared}\r\nConnection: keep-alive\r\n\r\n"
    ));
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&script.body);
    socket.write_all(&bytes).await?;
    socket.flush().await
}

// ---------------------------------------------------------------------------
// Scripted Messages SSE (mirrors testutil.FakeMessages)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum ScriptBlock {
    Thinking {
        thinking: String,
        signature: String,
    },
    Redacted {
        data: String,
    },
    Text(String),
    Tool {
        id: String,
        name: String,
        chunks: Vec<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Truncate {
    Block,
    Delta,
}

#[derive(Clone, Default)]
pub struct ScriptMessage {
    pub status: u16,
    pub blocks: Vec<ScriptBlock>,
    pub stop_reason: Option<String>,
    pub refusal_category: Option<String>,
    pub input_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub output_tokens: i64,
    pub thinking_tokens: i64,
    pub truncate: Option<Truncate>,
}

impl ScriptMessage {
    pub fn text(text: &str) -> Self {
        Self {
            blocks: vec![ScriptBlock::Text(text.to_string())],
            ..Self::default()
        }
    }

    pub fn http_error(status: u16) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }

    pub fn scripted(&self) -> Scripted {
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
            ScriptBlock::Redacted { data } => {
                event_bytes(
                    &mut out,
                    "content_block_start",
                    json!({
                        "index": index,
                        "content_block": {"type": "redacted_thinking", "data": data}
                    }),
                );
                if script.truncate == Some(Truncate::Block) {
                    return out;
                }
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
// Shared test helpers
// ---------------------------------------------------------------------------

pub fn ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

pub fn user_only() -> Vec<subagent_mcp::provider::Message> {
    vec![subagent_mcp::provider::Message {
        role: Role::User,
        text: "hi".to_string(),
        ..Default::default()
    }]
}

pub fn request(model: &str, messages: Vec<subagent_mcp::provider::Message>) -> TurnRequest {
    TurnRequest {
        model: model.to_string(),
        messages,
        ..TurnRequest::default()
    }
}

pub async fn run_turn(
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
