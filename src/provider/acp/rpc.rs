//! Generic line-delimited JSON-RPC 2.0 peer over an `AsyncRead`/`AsyncWrite`
//! pair. It has no ACP knowledge: outbound calls are matched to responses by
//! id, and inbound notifications and requests are forwarded, in order, on a
//! bounded channel. When the channel is full notifications are dropped
//! (counted, logged once); requests are never dropped. A line longer than
//! [`MAX_FRAME`] closes the connection. Lines that are not JSON objects (agent banners, logs,
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

/// Largest inbound line accepted, matching the HTTP/SSE line limit.
pub const MAX_FRAME: usize = crate::provider::sse::SSE_MAX_LINE;
/// Capacity of the inbound channel.
pub const INBOUND_CAPACITY: usize = 1024;

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
    pub fn spawn<R, W>(r: R, w: W) -> (Peer, mpsc::Receiver<Inbound>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::spawn_with_limits(r, w, MAX_FRAME, INBOUND_CAPACITY)
    }

    /// [`Peer::spawn`] with an explicit line limit and channel capacity.
    pub fn spawn_with_limits<R, W>(
        r: R,
        mut w: W,
        max_frame: usize,
        capacity: usize,
    ) -> (Peer, mpsc::Receiver<Inbound>)
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
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let reader = Arc::clone(&inner);
        tokio::spawn(async move {
            let mut r = BufReader::new(r);
            let mut buf = Vec::new();
            let mut dropped = 0u64;
            while let Ok(true) = read_line_bounded(&mut r, &mut buf, max_frame).await {
                if let Some(msg) = dispatch(&reader, &buf) {
                    match msg {
                        Inbound::Request { .. } => {
                            if tx.send(msg).await.is_err() {
                                continue;
                            }
                        }
                        Inbound::Notification { .. } => {
                            if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(msg) {
                                dropped += 1;
                                if dropped == 1 {
                                    eprintln!("acp: inbound channel full; dropping notifications");
                                }
                            }
                        }
                    }
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

/// Read one `\n`-terminated line into `buf` (cleared first) without ever
/// holding more than `max` bytes. `Ok(false)` on EOF, a read error, or a line
/// over `max`; a final unterminated line at EOF is still returned.
async fn read_line_bounded<R: AsyncRead + Unpin>(
    r: &mut BufReader<R>,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<bool> {
    buf.clear();
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(!buf.is_empty());
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if buf.len() + take > max.saturating_add(1) || (!done && buf.len() + take > max) {
            return Ok(false);
        }
        buf.extend_from_slice(&chunk[..take]);
        r.consume(take);
        if done {
            return Ok(true);
        }
    }
}

fn dispatch(inner: &Inner, line: &[u8]) -> Option<Inbound> {
    let Ok(Value::Object(mut obj)) = serde_json::from_slice::<Value>(line) else {
        return None;
    };
    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let params = obj.remove("params").unwrap_or(Value::Null);
    match (method, obj.remove("id")) {
        (Some(method), Some(id)) if !id.is_null() => {
            return Some(Inbound::Request { id, method, params });
        }
        (Some(method), _) => return Some(Inbound::Notification { method, params }),
        (None, Some(id)) => {
            let id = id.as_u64()?;
            let waiter = inner.pending.lock().unwrap().remove(&id)?;
            let reply = match obj.get("error") {
                Some(e) if !e.is_null() => Err(RpcError::from_value(e)),
                _ => Ok(obj.remove("result").unwrap_or(Value::Null)),
            };
            let _ = waiter.send(reply);
        }
        (None, None) => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::sse::SSE_MAX_LINE;
    use tokio::io::AsyncWriteExt;

    fn ctx() -> RunContext {
        RunContext {
            cancel: tokio_util::sync::CancellationToken::new(),
            deadline: None,
        }
    }

    #[tokio::test]
    async fn oversized_line_closes_peer() {
        let (mut agent, ours) = tokio::io::duplex(1 << 16);
        let (r, w) = tokio::io::split(ours);
        let (peer, mut rx) = Peer::spawn_with_limits(r, w, 1024, 16);
        agent.write_all(&vec![b'x'; 4096]).await.unwrap();
        let end = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;
        assert!(matches!(end, Ok(None)), "inbound must end");
        assert!(peer.closed());
    }

    #[tokio::test]
    async fn notification_flood_does_not_block_requests() {
        let (agent, ours) = tokio::io::duplex(1 << 20);
        let (r, w) = tokio::io::split(ours);
        let (ar, mut aw) = tokio::io::split(agent);
        let (peer, mut rx) = Peer::spawn_with_limits(r, w, SSE_MAX_LINE, 4);
        let call = tokio::spawn({
            let peer = peer.clone();
            async move { peer.call(&ctx(), "ping", json!({})).await }
        });
        let mut ar = BufReader::new(ar);
        let mut line = String::new();
        ar.read_line(&mut line).await.unwrap();
        let id = serde_json::from_str::<Value>(&line).unwrap()["id"].clone();
        let mut out = String::new();
        for _ in 0..100 {
            out.push_str("{\"jsonrpc\":\"2.0\",\"method\":\"n\",\"params\":{}}\n");
        }
        out.push_str("{\"jsonrpc\":\"2.0\",\"id\":\"r1\",\"method\":\"req\",\"params\":{}}\n");
        out.push_str(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":7}}\n"
        ));
        aw.write_all(out.as_bytes()).await.unwrap();
        let mut saw_request = false;
        let mut notes = 0;
        while let Ok(Some(msg)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await
        {
            match msg {
                Inbound::Request { ref method, .. } if method == "req" => {
                    saw_request = true;
                    break;
                }
                _ => notes += 1,
            }
        }
        assert!(notes <= 100);
        let res = tokio::time::timeout(std::time::Duration::from_secs(2), call)
            .await
            .expect("call blocked by notification flood")
            .unwrap();
        assert_eq!(res.unwrap(), json!(7));
        assert!(saw_request, "request must never be dropped");
    }
}
