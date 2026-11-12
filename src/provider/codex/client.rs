//! Newline-delimited JSON-RPC client for the Codex app-server.
//!
//! Ports Go `internal/provider/codexappserver/client.go` and the transport half
//! of `subscription_test.go`. One reader task multiplexes request replies and
//! per-thread notifications over the child's stdout. Enqueuing is a short
//! synchronous push, so a subscriber that stops draining can never stall the
//! reader; instead its queue fails once it outgrows [`MAX_QUEUED_EVENTS`].
//!
//! The app-server omits the JSON-RPC `jsonrpc` field, so [`Message`] does too.
//! Request IDs are raw JSON, preserving both string and number IDs.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::util::run::RunContext;

/// Maximum queued notifications for one thread subscription.
///
/// Streamed text is merged, so reaching this cap means the consumer stopped
/// draining while the server kept sending distinct notifications. Failing the
/// turn with a clear error beats growing without bound.
pub const MAX_QUEUED_EVENTS: usize = 4096;

/// The notification that streams assistant text, one message per token.
const DELTA_METHOD: &str = "item/agentMessage/delta";

/// Exact subagent-mcp-authored refusal for a server request no thread consumes.
const UNSUPPORTED_REQUEST: &str = "not supported by subagent-mcp";

/// Exact subagent-mcp-authored error after a thread subscription is closed.
const SUBSCRIPTION_CLOSED: &str = "codex app-server subscription closed";

// --- Errors ---------------------------------------------------------------

/// An error returned by the remote endpoint (`RPCError` in Go).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RpcError {}

/// Client failure. Wrapping mirrors Go's `%w` chains, so a caller can still
/// recognize an EOF or JSON error after `call`/`write`/`decode` prefixes.
#[derive(Clone, Debug)]
pub enum ClientError {
    /// A JSON-RPC error response, returned unwrapped like Go's `*RPCError`.
    Rpc(RpcError),
    /// An I/O failure from the child transport.
    Io(Arc<io::Error>),
    /// A JSON encoding or decoding failure.
    Json(Arc<serde_json::Error>),
    /// A subagent-mcp-authored message (shutdown, backlog, or protocol state).
    Message(String),
    /// The caller's context was cancelled (`context canceled`).
    Cancelled,
    /// The caller's context deadline elapsed (`context deadline exceeded`).
    DeadlineExceeded,
    /// A prefix plus an underlying client error (Go `fmt.Errorf("...: %w")`).
    Context {
        message: String,
        source: Arc<ClientError>,
    },
}

impl ClientError {
    /// Wrap `source` with the exact subagent-mcp prefix Go would use.
    pub fn context(message: impl Into<String>, source: ClientError) -> Self {
        Self::Context {
            message: message.into(),
            source: Arc::new(source),
        }
    }

    /// A plain subagent-mcp-authored error.
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    fn eof() -> Self {
        Self::Io(Arc::new(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "EOF",
        )))
    }

    /// Whether this failure (possibly wrapped) is EOF.
    pub fn is_eof(&self) -> bool {
        match self {
            Self::Io(error) => {
                error.kind() == io::ErrorKind::UnexpectedEof || error.to_string() == "EOF"
            }
            Self::Context { source, .. } => source.is_eof(),
            _ => false,
        }
    }

    /// Whether this failure (possibly wrapped) is a closed pipe.
    pub fn is_closed_pipe(&self) -> bool {
        match self {
            Self::Io(error) => {
                let text = error.to_string();
                error.kind() == io::ErrorKind::BrokenPipe
                    || text.contains("broken pipe")
                    || text.contains("closed pipe")
                    || text.contains("EPIPE")
            }
            Self::Context { source, .. } => source.is_closed_pipe(),
            _ => false,
        }
    }

    /// Whether this failure (possibly wrapped) came from JSON encoding/decoding.
    pub fn is_json(&self) -> bool {
        match self {
            Self::Json(_) => true,
            Self::Context { source, .. } => source.is_json(),
            _ => false,
        }
    }

    /// Whether this failure (possibly wrapped) is caller cancellation.
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::Context { source, .. } => source.is_cancelled(),
            _ => false,
        }
    }

    /// Whether this failure (possibly wrapped) is a context deadline.
    pub fn is_deadline_exceeded(&self) -> bool {
        match self {
            Self::DeadlineExceeded => true,
            Self::Context { source, .. } => source.is_deadline_exceeded(),
            _ => false,
        }
    }

    /// The RPC error, if this failure is one (possibly wrapped).
    pub fn as_rpc(&self) -> Option<&RpcError> {
        match self {
            Self::Rpc(error) => Some(error),
            Self::Context { source, .. } => source.as_rpc(),
            _ => None,
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(error) => write!(f, "{error}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::Message(message) => f.write_str(message),
            Self::Cancelled => f.write_str("context canceled"),
            Self::DeadlineExceeded => f.write_str("context deadline exceeded"),
            Self::Context { message, source } => write!(f, "{message}: {source}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error.as_ref()),
            Self::Json(error) => Some(error.as_ref()),
            Self::Context { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(Arc::new(error))
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(Arc::new(error))
    }
}

// --- Context helpers ------------------------------------------------------

/// Return the context's terminal error, if it has one, matching Go's
/// `ctx.Err()` precedence: cancellation before deadline.
pub(crate) fn ctx_error(ctx: &RunContext) -> Option<ClientError> {
    if ctx.cancel.is_cancelled() {
        return Some(ClientError::Cancelled);
    }
    if ctx
        .deadline
        .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
    {
        return Some(ClientError::DeadlineExceeded);
    }
    None
}

/// Wait until the context is cancelled or its deadline elapses.
pub(crate) async fn wait_ctx(ctx: &RunContext) -> ClientError {
    match ctx.deadline {
        Some(deadline) => tokio::select! {
            _ = ctx.cancel.cancelled() => ClientError::Cancelled,
            _ = tokio::time::sleep_until(deadline) => ClientError::DeadlineExceeded,
        },
        None => {
            ctx.cancel.cancelled().await;
            ClientError::Cancelled
        }
    }
}

// --- Wire messages --------------------------------------------------------

/// A Codex request, notification, or response.
///
/// Codex omits the JSON-RPC version field, and IDs remain raw JSON so string and
/// number IDs both round-trip exactly.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Message {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Deserialize)]
struct ThreadParams {
    #[serde(rename = "threadId")]
    thread_id: String,
}

/// The subset of notification params needed to merge streamed deltas.
///
/// Every field is always serialized (no `omitempty`), matching Go's
/// `notificationParams` when a merged delta run is re-marshalled.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct NotificationParams {
    #[serde(rename = "threadId")]
    thread_id: String,
    #[serde(rename = "turnId")]
    turn_id: String,
    #[serde(rename = "itemId")]
    item_id: String,
    delta: String,
    item: CodexItem,
    turn: TurnParams,
    #[serde(rename = "tokenUsage")]
    token_usage: TokenUsage,
    error: ErrorParams,
    #[serde(rename = "willRetry")]
    will_retry: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct CodexItem {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    text: String,
    phase: String,
    command: String,
    #[serde(rename = "exitCode")]
    exit_code: Option<i64>,
    changes: Option<Vec<Change>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct Change {
    path: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct TurnParams {
    id: String,
    status: String,
    error: ErrorParams,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct ErrorParams {
    message: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct TokenUsage {
    last: TokenLast,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct TokenLast {
    #[serde(rename = "inputTokens")]
    input_tokens: i64,
    #[serde(rename = "outputTokens")]
    output_tokens: i64,
    #[serde(rename = "totalTokens")]
    total_tokens: i64,
}

// --- Client ---------------------------------------------------------------

enum CallReply {
    Message(Message),
    Error(ClientError),
}

struct State {
    next_id: u64,
    pending: HashMap<String, oneshot::Sender<CallReply>>,
    subscriptions: HashMap<String, Subscription>,
    error: Option<ClientError>,
}

struct Inner {
    writer: tokio::sync::Mutex<Option<Box<dyn AsyncWrite + Unpin + Send>>>,
    write_lock: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    done: CancellationToken,
}

/// Multiplexes calls and per-thread events over one connection.
///
/// The caller owns the streams; closing them (or [`Client::abort_reader`]) stops
/// the reader. Clones share the connection because [`Client`] is held in an
/// `Arc`.
pub struct Client {
    inner: Arc<Inner>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl Client {
    /// Start the single reader task for `reader` and write requests to `writer`.
    pub fn new<R, W>(reader: R, writer: W) -> Arc<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let inner = Arc::new(Inner {
            writer: tokio::sync::Mutex::new(Some(Box::new(writer))),
            write_lock: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                next_id: 0,
                pending: HashMap::new(),
                subscriptions: HashMap::new(),
                error: None,
            }),
            done: CancellationToken::new(),
        });
        let reader_inner = inner.clone();
        let handle = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let message: Message = match serde_json::from_str(&line) {
                            Ok(message) => message,
                            Err(error) => {
                                reader_inner.stop(ClientError::Json(Arc::new(error)));
                                return;
                            }
                        };
                        if let Err(error) = reader_inner.handle(message).await {
                            reader_inner.stop(error);
                            return;
                        }
                    }
                    Ok(None) => {
                        reader_inner.stop(ClientError::eof());
                        return;
                    }
                    Err(error) => {
                        reader_inner.stop(ClientError::Io(Arc::new(error)));
                        return;
                    }
                }
            }
        });
        Arc::new(Self {
            inner,
            reader: Mutex::new(Some(handle)),
        })
    }

    /// Send a request and return its decoded result (`null` when absent).
    pub async fn call(
        &self,
        ctx: &RunContext,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, ClientError> {
        if let Some(error) = ctx_error(ctx) {
            return Err(error);
        }
        let params = match params {
            Some(params) => Some(serde_json::value::to_raw_value(&params).map_err(|error| {
                ClientError::context(format!("encode {method} params"), ClientError::from(error))
            })?),
            None => None,
        };

        // Allocate IDs under the write lock so they also increase in wire order.
        let write_guard = tokio::select! {
            guard = self.inner.write_lock.lock() => guard,
            error = wait_ctx(ctx) => return Err(error),
        };
        if let Some(error) = ctx_error(ctx) {
            return Err(error);
        }
        let (id, reply) = {
            let mut state = self.inner.state.lock().unwrap();
            if let Some(error) = &state.error {
                return Err(ClientError::context(
                    format!("call {method}"),
                    error.clone(),
                ));
            }
            state.next_id += 1;
            let id = state.next_id.to_string();
            let (sender, receiver) = oneshot::channel();
            state.pending.insert(id.clone(), sender);
            (id, receiver)
        };
        let guard = PendingGuard {
            inner: self.inner.clone(),
            id: id.clone(),
        };
        let message = Message {
            id: Some(
                RawValue::from_string(id).expect("numeric request id is always valid raw JSON"),
            ),
            method: Some(method.to_string()),
            params,
            ..Message::default()
        };
        let write = self.inner.write_message_locked(&message).await;
        drop(write_guard);
        if let Err(error) = write {
            return Err(ClientError::context(format!("write {method}"), error));
        }

        let reply = tokio::select! {
            reply = reply => reply,
            error = wait_ctx(ctx) => return Err(error),
        };
        drop(guard);
        let reply = reply.map_err(|_| {
            ClientError::context(
                format!("call {method}"),
                ClientError::message("codex app-server response channel closed"),
            )
        })?;
        match reply {
            CallReply::Message(message) => {
                if let Some(error) = message.error {
                    return Err(ClientError::Rpc(error));
                }
                match message.result {
                    Some(result) => serde_json::from_str(result.get()).map_err(|error| {
                        ClientError::context(
                            format!("decode {method} result"),
                            ClientError::from(error),
                        )
                    }),
                    None => Ok(Value::Null),
                }
            }
            CallReply::Error(error) => Err(ClientError::context(format!("call {method}"), error)),
        }
    }

    /// Send a request and decode its result into `T`.
    pub async fn call_into<T: DeserializeOwned>(
        &self,
        ctx: &RunContext,
        method: &str,
        params: Option<Value>,
    ) -> Result<T, ClientError> {
        let result = self.call(ctx, method, params).await?;
        serde_json::from_value(result).map_err(|error| {
            ClientError::context(format!("decode {method} result"), ClientError::from(error))
        })
    }

    /// Send a notification, which has no request ID or response.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), ClientError> {
        let params = match params {
            Some(params) => {
                Some(serde_json::value::to_raw_value(&params).map_err(ClientError::from)?)
            }
            None => None,
        };
        self.inner
            .write(Message {
                method: Some(method.to_string()),
                params,
                ..Message::default()
            })
            .await
    }

    /// Return the existing queue for `thread_id`, or register a new one.
    ///
    /// A queue that failed permanently — its backlog outgrew the cap — is
    /// replaced, so a thread that recovers its consumer can be served again
    /// rather than being poisoned for the life of the process.
    pub fn subscribe(&self, thread_id: &str) -> Subscription {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(subscription) = state.subscriptions.get(thread_id)
            && !subscription.failed_permanently()
        {
            return subscription.clone();
        }
        let subscription = Subscription::new();
        if let Some(error) = &state.error {
            subscription.close(error.clone());
        }
        state
            .subscriptions
            .insert(thread_id.to_string(), subscription.clone());
        subscription
    }

    /// Remove a thread's queue and wake its waiters. Requests still queued for
    /// that thread are rejected rather than leaving the server waiting.
    pub async fn unsubscribe(&self, thread_id: &str) {
        let queued = {
            let mut state = self.inner.state.lock().unwrap();
            state
                .subscriptions
                .remove(thread_id)
                .map(|subscription| subscription.close(ClientError::message(SUBSCRIPTION_CLOSED)))
                .unwrap_or_default()
        };
        for message in queued {
            if let Some(id) = &message.id {
                let _ = self.respond_error(id, -32601, UNSUPPORTED_REQUEST).await;
            }
        }
    }

    /// Answer a server request, preserving the request's ID type.
    pub async fn respond(&self, id: &RawValue, result: Value) -> Result<(), ClientError> {
        let result = serde_json::value::to_raw_value(&result).map_err(ClientError::from)?;
        self.inner
            .write(Message {
                id: Some(
                    RawValue::from_string(id.get().to_string())
                        .expect("a raw id stays valid raw JSON"),
                ),
                result: Some(result),
                ..Message::default()
            })
            .await
    }

    /// Answer a server request with a protocol error.
    pub async fn respond_error(
        &self,
        id: &RawValue,
        code: i64,
        message: &str,
    ) -> Result<(), ClientError> {
        self.inner
            .write(Message {
                id: Some(
                    RawValue::from_string(id.get().to_string())
                        .expect("a raw id stays valid raw JSON"),
                ),
                error: Some(RpcError {
                    code,
                    message: message.to_string(),
                }),
                ..Message::default()
            })
            .await
    }

    /// A token that is cancelled once the reader stops.
    pub fn done(&self) -> CancellationToken {
        self.inner.done.clone()
    }

    /// Whether the reader has stopped.
    pub fn is_done(&self) -> bool {
        self.inner.done.is_cancelled()
    }

    /// Why the reader stopped, or `None` while it is running.
    pub fn error(&self) -> Option<ClientError> {
        self.inner.state.lock().unwrap().error.clone()
    }

    /// Drop the write half, which closes the child's stdin.
    pub async fn close_writer(&self) {
        self.inner.writer.lock().await.take();
    }

    /// Stop the reader and fail every pending call and subscription.
    ///
    /// This is the cleanup path for a child that retained stdout past the kill
    /// deadline: aborting the reader drops its `ChildStdout`, releasing the pipe
    /// even when a descendant still holds the write end.
    pub fn abort_reader(&self) {
        self.inner.stop(ClientError::eof());
        if let Some(handle) = self.reader.lock().unwrap().take() {
            handle.abort();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(handle) = self.reader.lock().unwrap().take() {
            handle.abort();
        }
    }
}

impl Inner {
    async fn write(&self, message: Message) -> Result<(), ClientError> {
        let _guard = self.write_lock.lock().await;
        if let Some(error) = self.state.lock().unwrap().error.clone() {
            return Err(error);
        }
        self.write_message_locked(&message).await
    }

    async fn write_message_locked(&self, message: &Message) -> Result<(), ClientError> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        let mut writer = self.writer.lock().await;
        let Some(writer) = writer.as_mut() else {
            return Err(ClientError::message("codex app-server writer closed"));
        };
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn handle(&self, message: Message) -> Result<(), ClientError> {
        if message.method.as_deref().unwrap_or("").is_empty() {
            let id = message
                .id
                .as_ref()
                .map(|id| id.get())
                .unwrap_or("")
                .to_string();
            let reply = self.state.lock().unwrap().pending.remove(&id);
            if let Some(reply) = reply {
                // Buffered by the oneshot, even for a cancelled caller.
                let _ = reply.send(CallReply::Message(message));
            }
            return Ok(());
        }

        let thread_id = message.params.as_ref().and_then(|params| {
            serde_json::from_str::<ThreadParams>(params.get())
                .ok()
                .map(|params| params.thread_id)
        });
        let mut routed = false;
        if let Some(thread_id) = thread_id
            && !thread_id.is_empty()
        {
            let state = self.state.lock().unwrap();
            if let Some(subscription) = state.subscriptions.get(&thread_id) {
                subscription.enqueue(message.clone());
                routed = true;
            }
        }
        if !routed && let Some(id) = &message.id {
            self.respond_error_locked(id, -32601, UNSUPPORTED_REQUEST)
                .await?;
        }
        Ok(())
    }

    async fn respond_error_locked(
        &self,
        id: &RawValue,
        code: i64,
        message: &str,
    ) -> Result<(), ClientError> {
        self.write(Message {
            id: Some(
                RawValue::from_string(id.get().to_string()).expect("a raw id stays valid raw JSON"),
            ),
            error: Some(RpcError {
                code,
                message: message.to_string(),
            }),
            ..Message::default()
        })
        .await
    }

    fn stop(&self, error: ClientError) {
        let (pending, subscriptions) = {
            let mut state = self.state.lock().unwrap();
            if state.error.is_some() {
                return;
            }
            state.error = Some(error.clone());
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.subscriptions),
            )
        };
        for (_, reply) in pending {
            let _ = reply.send(CallReply::Error(error.clone()));
        }
        for (_, subscription) in subscriptions {
            subscription.close(error.clone());
        }
        self.done.cancel();
    }
}

struct PendingGuard {
    inner: Arc<Inner>,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.inner.state.lock().unwrap().pending.remove(&self.id);
    }
}

// --- Subscriptions --------------------------------------------------------

struct QueuedEvent {
    message: Message,
    item: String,
    text: String,
}

impl QueuedEvent {
    fn message(&self) -> Message {
        if self.text.is_empty() {
            return self.message.clone();
        }
        let mut params: NotificationParams =
            match serde_json::from_str(self.message.params.as_ref().unwrap().get()) {
                Ok(params) => params,
                Err(_) => return self.message.clone(),
            };
        params.delta.clone_from(&self.text);
        let raw = match serde_json::value::to_raw_value(&params) {
            Ok(raw) => raw,
            Err(_) => return self.message.clone(),
        };
        Message {
            method: self.message.method.clone(),
            params: Some(raw),
            ..Message::default()
        }
    }
}

struct SubscriptionState {
    queue: VecDeque<QueuedEvent>,
    error: Option<ClientError>,
    permanent: bool,
}

struct SubscriptionInner {
    state: Mutex<SubscriptionState>,
    notify: tokio::sync::Notify,
}

/// An in-order queue that merges runs of streamed text.
///
/// A slow consumer never holds up the shared reader. Closing a queue discards
/// events and wakes every waiter.
#[derive(Clone)]
pub struct Subscription {
    inner: Arc<SubscriptionInner>,
}

impl PartialEq for Subscription {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for Subscription {}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

impl Subscription {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(SubscriptionInner {
                state: Mutex::new(SubscriptionState {
                    queue: VecDeque::new(),
                    error: None,
                    permanent: false,
                }),
                notify: tokio::sync::Notify::new(),
            }),
        }
    }

    fn failed_permanently(&self) -> bool {
        self.inner.state.lock().unwrap().permanent
    }

    fn enqueue(&self, message: Message) {
        let mut state = self.inner.state.lock().unwrap();
        if state.error.is_some() {
            return;
        }
        if message.method.as_deref() == Some(DELTA_METHOD)
            && let Some(params) = message
                .params
                .as_ref()
                .and_then(|params| serde_json::from_str::<NotificationParams>(params.get()).ok())
            && !params.item_id.is_empty()
        {
            if let Some(last) = state.queue.back_mut()
                && last.item == params.item_id
            {
                // Continuing the run: the queue was already nonempty, so no
                // waiter needs waking.
                last.text.push_str(&params.delta);
                return;
            }
            state.queue.push_back(QueuedEvent {
                message,
                item: params.item_id,
                text: params.delta,
            });
            self.wake_locked(&mut state);
            self.fail_on_backlog_locked(&mut state);
            return;
        }
        state.queue.push_back(QueuedEvent {
            item: String::new(),
            text: String::new(),
            message,
        });
        self.wake_locked(&mut state);
        self.fail_on_backlog_locked(&mut state);
    }

    /// Abandon the turn once the queue outgrows its cap. Merging keeps a
    /// streamed turn far below it, so reaching the cap means the consumer is not
    /// draining at all; an error it can report beats a queue that grows until
    /// the process dies.
    fn fail_on_backlog_locked(&self, state: &mut SubscriptionState) {
        if state.queue.len() <= MAX_QUEUED_EVENTS {
            return;
        }
        state.error = Some(ClientError::message(format!(
            "codex app-server event backlog exceeded {MAX_QUEUED_EVENTS} notifications: \
             the consumer stopped draining, so this subscription is closed; its events \
             are no longer delivered and the turn cannot continue"
        )));
        state.permanent = true;
        state.queue.clear();
        self.inner.notify.notify_waiters();
    }

    /// Wake waiters only on the empty-to-nonempty transition. No consumer can be
    /// asleep with a nonempty queue, except one already woken by this signal.
    fn wake_locked(&self, state: &mut SubscriptionState) {
        if state.queue.len() == 1 {
            self.inner.notify.notify_waiters();
        }
    }

    fn close(&self, error: ClientError) -> Vec<Message> {
        let mut state = self.inner.state.lock().unwrap();
        if state.error.is_some() {
            return Vec::new();
        }
        state.error = Some(error);
        let queued = state.queue.drain(..).map(|event| event.message()).collect();
        self.inner.notify.notify_waiters();
        queued
    }

    /// Wait for the next event, context cancellation, or the queue's closure.
    pub async fn next(&self, ctx: &RunContext) -> Result<Message, ClientError> {
        loop {
            if let Some(error) = ctx_error(ctx) {
                return Err(error);
            }
            let notified = self.inner.notify.notified();
            {
                let mut state = self.inner.state.lock().unwrap();
                if let Some(error) = &state.error {
                    return Err(error.clone());
                }
                if let Some(event) = state.queue.pop_front() {
                    return Ok(event.message());
                }
            }
            tokio::select! {
                _ = notified => {}
                error = wait_ctx(ctx) => return Err(error),
            }
        }
    }
}
