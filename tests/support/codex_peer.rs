//! Minimal scripted Codex app-server peer for the MCP integration tests.
//!
//! The full protocol suite lives in `tests/codex_fake.rs`; this peer is only
//! enough to drive one live thread, an optional approval, `account/read`, and
//! `model/list` through the real [`CodexAdapter`] and the process-wide pool.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, WriteHalf, split};

use subagent_mcp::provider::codex::client::{Client, ClientError};
use subagent_mcp::provider::codex::pool::{self, Closer, DialFuture, Dialed, Pool};

pub const USER_AGENT: &str = "codex-cli/0.156.1 (fake)";
pub const THREAD_ID: &str = "thr-1";
pub const TURN_ID: &str = "turn-1";
pub const APPROVAL_ID: &str = "approval-1";

/// What one dialed fake app-server should do.
#[derive(Clone)]
pub struct PeerConfig {
    pub final_text: String,
    pub approval: bool,
    pub auth: AuthScript,
    pub models: Vec<String>,
}

impl PeerConfig {
    pub fn new(final_text: impl Into<String>) -> Self {
        Self {
            final_text: final_text.into(),
            approval: false,
            auth: AuthScript::LoggedIn,
            models: vec!["gpt-6.1-terra".into(), "gpt-6.1-sol".into()],
        }
    }
}

#[derive(Clone, Copy)]
pub enum AuthScript {
    LoggedIn,
    LoggedOut,
}

/// Every request/notification and every client response the peer saw.
#[derive(Default)]
pub struct PeerLog {
    requests: Mutex<Vec<Value>>,
    responses: Mutex<Vec<Value>>,
}

impl PeerLog {
    pub fn requests(&self, method: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.get("method").and_then(Value::as_str) == Some(method))
            .cloned()
            .collect()
    }

    pub fn all_requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    pub fn responses(&self) -> Vec<Value> {
        self.responses.lock().unwrap().clone()
    }
}

/// Install a fake pool for `command` and return its request log. The caller
/// must call [`remove`] when the test is done.
pub fn install(command: &str, config: PeerConfig) -> Arc<PeerLog> {
    let log = Arc::new(PeerLog::default());
    pool::install_pool(command, fake_pool(config, log.clone()));
    log
}

pub fn remove(command: &str) {
    pool::remove_pool(command);
}

fn fake_pool(config: PeerConfig, log: Arc<PeerLog>) -> Arc<Pool> {
    let dialer: pool::Dialer = Arc::new(move || {
        let config = config.clone();
        let log = log.clone();
        let future: DialFuture = Box::pin(async move {
            let (client_side, server_side) = tokio::io::duplex(1 << 20);
            let (reader, writer) = split(client_side);
            let client = Client::new(reader, writer);
            tokio::spawn(serve(server_side, config, log));
            Ok::<Dialed, ClientError>(Dialed {
                client,
                closer: Arc::new(NoopCloser),
                stderr_tail: Some(Arc::new(|| "fake peer closed".to_string())),
            })
        });
        future
    });
    Arc::new(Pool::new(dialer))
}

struct NoopCloser;

#[async_trait]
impl Closer for NoopCloser {
    async fn close(&self) {}

    fn close_blocking(&self) {}
}

async fn serve(server: DuplexStream, config: PeerConfig, log: Arc<PeerLog>) {
    let (reader, mut writer) = split(server);
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            log.responses.lock().unwrap().push(message);
            continue;
        };
        log.requests.lock().unwrap().push(message.clone());
        match method {
            "initialize" => {
                respond(&mut writer, &message, json!({"userAgent": USER_AGENT})).await;
            }
            "initialized" => {}
            "thread/start" => {
                respond(&mut writer, &message, json!({"thread": {"id": THREAD_ID}})).await;
            }
            "turn/start" => {
                respond(&mut writer, &message, json!({"turn": {"id": TURN_ID}})).await;
                notify(
                    &mut writer,
                    "turn/started",
                    json!({"threadId": THREAD_ID, "turn": {"id": TURN_ID}}),
                )
                .await;
                if config.approval {
                    send_request(
                        &mut writer,
                        json!(APPROVAL_ID),
                        "item/commandExecution/requestApproval",
                        json!({
                            "threadId": THREAD_ID,
                            "turnId": TURN_ID,
                            "itemId": "item-1",
                            "command": "echo hi",
                            "reason": "need approval",
                        }),
                    )
                    .await;
                }
                notify(
                    &mut writer,
                    "item/completed",
                    json!({
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "item": {
                            "id": "item-1",
                            "type": "agentMessage",
                            "text": config.final_text,
                            "phase": "final_answer",
                        },
                    }),
                )
                .await;
                notify(
                    &mut writer,
                    "thread/tokenUsage/updated",
                    json!({
                        "threadId": THREAD_ID,
                        "turnId": TURN_ID,
                        "tokenUsage": {
                            "last": {
                                "inputTokens": 10,
                                "outputTokens": 2,
                                "totalTokens": 12,
                            },
                        },
                    }),
                )
                .await;
                notify(
                    &mut writer,
                    "turn/completed",
                    json!({"threadId": THREAD_ID, "turn": {"id": TURN_ID, "status": "completed"}}),
                )
                .await;
            }
            "account/read" => {
                let result = match config.auth {
                    AuthScript::LoggedIn => {
                        json!({"account": {"type": "chatgpt", "planType": "pro"}, "requiresOpenaiAuth": true})
                    }
                    AuthScript::LoggedOut => json!({"account": null, "requiresOpenaiAuth": true}),
                };
                respond(&mut writer, &message, result).await;
            }
            "model/list" => {
                let data: Vec<Value> = config.models.iter().map(|id| json!({"id": id})).collect();
                respond(
                    &mut writer,
                    &message,
                    json!({"data": data, "nextCursor": null}),
                )
                .await;
            }
            "thread/unsubscribe" | "turn/interrupt" => {
                respond(&mut writer, &message, json!({})).await;
            }
            _ => {
                respond_error(
                    &mut writer,
                    &message,
                    -32601,
                    "not supported by subagent-mcp",
                )
                .await;
            }
        }
    }
}

async fn respond(writer: &mut WriteHalf<DuplexStream>, request: &Value, result: Value) {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    write_line(writer, &json!({"id": id, "result": result})).await;
}

async fn respond_error(
    writer: &mut WriteHalf<DuplexStream>,
    request: &Value,
    code: i64,
    message: &str,
) {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    write_line(
        writer,
        &json!({"id": id, "error": {"code": code, "message": message}}),
    )
    .await;
}

async fn send_request(
    writer: &mut WriteHalf<DuplexStream>,
    id: Value,
    method: &str,
    params: Value,
) {
    write_line(
        writer,
        &json!({"id": id, "method": method, "params": params}),
    )
    .await;
}

async fn notify(writer: &mut WriteHalf<DuplexStream>, method: &str, params: Value) {
    write_line(writer, &json!({"method": method, "params": params})).await;
}

async fn write_line(writer: &mut WriteHalf<DuplexStream>, message: &Value) {
    let mut bytes = serde_json::to_vec(message).expect("serialize fake peer message");
    bytes.push(b'\n');
    let _ = writer.write_all(&bytes).await;
    let _ = writer.flush().await;
}
