//! Generic line-delimited JSON-RPC 2.0 peer over an `AsyncRead`/`AsyncWrite`
//! pair. It has no ACP knowledge: outbound calls are matched to responses by
//! id, and inbound notifications and requests are forwarded, in order, on an
//! unbounded channel. Lines that are not JSON objects (agent banners, logs)
//! are skipped.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::util::run::RunContext;

/// Code for calls failed because the connection closed.
pub const CODE_CLOSED: i64 = -32099;
/// Code for calls abandoned because the caller's context was cancelled or its
/// deadline passed.
pub const CODE_CANCELLED: i64 = -32098;
/// Code for calls abandoned because the run deadline passed.
pub const CODE_DEADLINE: i64 = -32097;

/// A message the far end initiated.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

/// A JSON-RPC error object, or a local transport failure in the same shape.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn closed() -> Self {
        Self::new(CODE_CLOSED, "connection closed")
    }

    fn from_value(v: &Value) -> Self {
        Self {
            code: v.get("code").and_then(Value::as_i64).unwrap_or(-32603),
            message: v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string(),
            data: v.get("data").cloned(),
        }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RpcError {}

type Reply = Result<Value, RpcError>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

struct Inner {
    writer: tokio::sync::Mutex<Writer>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

impl Inner {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let drained: Vec<_> = self.pending.lock().unwrap().drain().collect();
        for (_, tx) in drained {
            let _ = tx.send(Err(RpcError::closed()));
        }
    }

    async fn write(&self, msg: &Value) -> Result<(), RpcError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(RpcError::closed());
        }
        let mut line = msg.to_string();
        line.push('\n');
        let mut w = self.writer.lock().await;
        let res = async {
            w.write_all(line.as_bytes()).await?;
            w.flush().await
        }
        .await;
        res.map_err(|_| RpcError::closed())
    }
}

/// Cheaply clonable handle to one JSON-RPC connection.
#[derive(Clone)]
pub struct Peer {
    inner: Arc<Inner>,
}

impl Peer {
    /// Start the reader task and return the peer plus the inbound stream. The
    /// stream ends when the reader hits EOF or a read error.
    pub fn spawn<R, W>(r: R, w: W) -> (Peer, mpsc::UnboundedReceiver<Inbound>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let inner = Arc::new(Inner {
            writer: tokio::sync::Mutex::new(Box::new(w)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let reader = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut lines = BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                dispatch(&reader, &tx, &line);
            }
            reader.close();
        });
        (Peer { inner }, rx)
    }

    /// Send a request and wait for its response. Explicit cancellation gives
    /// `-32098 cancelled`; a passed deadline gives the run's deadline reason.
    pub async fn call(&self, ctx: &RunContext, method: &str, params: Value) -> Reply {
        if let Some(err) = ctx_error(ctx) {
            return Err(err);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        // The reader may have drained `pending` before our insert.
        if self.inner.closed.load(Ordering::SeqCst) {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(RpcError::closed());
        }
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let outcome = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => None,
            _ = sleep_until(ctx.deadline) => None,
            res = async {
                self.inner.write(&msg).await?;
                (&mut rx).await.unwrap_or_else(|_| Err(RpcError::closed()))
            } => Some(res),
        };
        match outcome {
            Some(res) => res,
            None => {
                self.inner.pending.lock().unwrap().remove(&id);
                Err(ctx_error(ctx).unwrap_or_else(|| RpcError::new(CODE_CANCELLED, "cancelled")))
            }
        }
    }

    /// Send a notification.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.inner
            .write(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    /// Answer an inbound request.
    pub async fn respond(&self, id: Value, result: Reply) -> Result<(), RpcError> {
        let msg = match result {
            Ok(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
            Err(e) => {
                let mut err = json!({"code":e.code,"message":e.message});
                if let Some(d) = e.data {
                    err["data"] = d;
                }
                json!({"jsonrpc":"2.0","id":id,"error":err})
            }
        };
        self.inner.write(&msg).await
    }

    /// Whether the reader has hit EOF or a read error.
    pub fn closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }
}

fn ctx_error(ctx: &RunContext) -> Option<RpcError> {
    if ctx.cancel.is_cancelled() {
        return Some(RpcError::new(CODE_CANCELLED, "cancelled"));
    }
    ctx.deadline_reason()
        .map(|reason| RpcError::new(CODE_DEADLINE, reason))
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

fn dispatch(inner: &Inner, tx: &mpsc::UnboundedSender<Inbound>, line: &str) {
    let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let params = take_params(&mut obj);
    match (method, obj.remove("id")) {
        (Some(method), Some(id)) if !id.is_null() => {
            let _ = tx.send(Inbound::Request { id, method, params });
        }
        (Some(method), _) => {
            let _ = tx.send(Inbound::Notification { method, params });
        }
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else { return };
            let Some(waiter) = inner.pending.lock().unwrap().remove(&id) else {
                return;
            };
            let reply = match obj.get("error") {
                Some(e) if !e.is_null() => Err(RpcError::from_value(e)),
                _ => Ok(obj.remove("result").unwrap_or(Value::Null)),
            };
            let _ = waiter.send(reply);
        }
        (None, None) => {}
    }
}

fn take_params(obj: &mut Map<String, Value>) -> Value {
    obj.remove("params").unwrap_or(Value::Null)
}
