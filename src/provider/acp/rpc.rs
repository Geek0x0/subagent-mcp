//! Generic line-delimited JSON-RPC 2.0 peer over an `AsyncRead`/`AsyncWrite`
//! pair. It has no ACP knowledge: outbound calls are matched to responses by
//! id, and inbound notifications and requests are forwarded, in order, on an
//! unbounded channel. Lines that are not JSON objects (agent banners, logs,
//! non-UTF-8 bytes) are skipped.
//!
//! Frames are written by a dedicated writer task, so a caller that is
//! cancelled mid-write never leaves a partial line on the wire.
//!
//! Dropping every [`Peer`] handle does not close the transport: the reader
//! and writer tasks live until the reader hits EOF (for a child process, until
//! it exits or is killed).

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::util::run::RunContext;

/// Code for calls failed because the connection closed.
pub const CODE_CLOSED: i64 = -32099;
/// Code for calls abandoned because the caller's context was explicitly
/// cancelled.
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
type Frame = (Vec<u8>, oneshot::Sender<bool>);

struct Inner {
    frames: mpsc::UnboundedSender<Frame>,
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

    /// Queue one frame and wait for the writer task to finish it. Dropping
    /// this future does not abort the frame: the writer task owns the bytes.
    async fn write(&self, msg: &Value) -> Result<(), RpcError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(RpcError::closed());
        }
        let mut line = msg.to_string().into_bytes();
        line.push(b'\n');
        let (ack, done) = oneshot::channel();
        self.frames
            .send((line, ack))
            .map_err(|_| RpcError::closed())?;
        match done.await {
            Ok(true) => Ok(()),
            _ => Err(RpcError::closed()),
        }
    }
}

/// Cheaply clonable handle to one JSON-RPC connection.
#[derive(Clone)]
pub struct Peer {
    inner: Arc<Inner>,
}

impl Peer {
    /// Start the reader and writer tasks and return the peer plus the inbound
    /// stream. The stream ends when the reader hits EOF or a read error.
    pub fn spawn<R, W>(r: R, mut w: W) -> (Peer, mpsc::UnboundedReceiver<Inbound>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (frames, mut frame_rx) = mpsc::unbounded_channel::<Frame>();
        tokio::spawn(async move {
            while let Some((bytes, ack)) = frame_rx.recv().await {
                let ok = async {
                    w.write_all(&bytes).await?;
                    w.flush().await
                }
                .await
                .is_ok();
                let _ = ack.send(ok);
            }
        });
        let inner = Arc::new(Inner {
            frames,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let reader = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut r = BufReader::new(r);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => dispatch(&reader, &tx, &buf),
                }
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
            self.remove_pending(id);
            return Err(RpcError::closed());
        }
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let outcome = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => None,
            _ = sleep_until(ctx.deadline) => None,
            res = async {
                if let Err(e) = self.inner.write(&msg).await {
                    self.remove_pending(id);
                    return Err(e);
                }
                (&mut rx).await.unwrap_or_else(|_| Err(RpcError::closed()))
            } => Some(res),
        };
        match outcome {
            Some(res) => res,
            None => {
                self.remove_pending(id);
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

    fn remove_pending(&self, id: u64) {
        self.inner.pending.lock().unwrap().remove(&id);
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

fn dispatch(inner: &Inner, tx: &mpsc::UnboundedSender<Inbound>, line: &[u8]) {
    let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let params = obj.remove("params").unwrap_or(Value::Null);
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
