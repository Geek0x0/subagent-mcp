//! MCP stdio server core: frame dispatch, the mcp-go-compatible worker pool,
//! cancellation, and the pinned mcp-go protocol surface.
//!
//! The Go 0.13.1 binary (mcp-go v0.44.0) is normative. `tests/fixtures/mcp/protocol.jsonl`
//! pins the wire surface captured from it before cutover.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::mcp::approval::ElicitationApprover;
use crate::mcp::jsonrpc::{
    Frame, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, JsonRpcError, METHOD_NOT_FOUND,
    PARSE_ERROR, RequestId, canonical_request_id, parse_frame,
};
use crate::mcp::progress::ProgressSink;
use crate::util::approval::{ApprovalRequest, Approver};
use crate::util::run::RunContext;

/// mcp-go's stdio worker pool size (`WithWorkerPoolSize(32)`).
pub const WORKER_POOL_SIZE: usize = 32;
/// mcp-go's stdio tool-call queue size (`WithQueueSize` default).
pub const QUEUE_CAPACITY: usize = 100;

/// The `_meta` field the before-call hook uses to hand the canonical JSON-RPC
/// request ID to the handler.
const REQUEST_ID_META_KEY: &str = "subagent-mcp/requestId";

/// mcp-go v0.44.0 protocol versions. An empty client version means
/// `2025-03-26`; an unknown version means the latest one.
const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-03-26";
const VALID_PROTOCOL_VERSIONS: [&str; 4] = [
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// A tool advertised by `tools/list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub annotations: Option<Value>,
}

/// Per-call state handed to a [`ToolHandler`].
pub struct CallContext {
    pub cancel: CancellationToken,
    pub request_id: Option<RequestId>,
    /// The `tools/call` params `_meta` object, with the canonical JSON-RPC
    /// request ID injected under `subagent-mcp/requestId`.
    ///
    /// This mirrors mcp-go's before-call-tool hook, which writes
    /// `request.Params.Meta.AdditionalFields` — a sibling of `arguments`, never
    /// inside it. `None` when the client sent no `_meta` and the ID cannot be
    /// canonicalized.
    pub meta: Option<Value>,
    pub progress: Option<ProgressSink>,
    pub approver: Arc<dyn Approver>,
}

/// A tool error that becomes a JSON-RPC error response. mcp-go maps handler
/// errors to `-32603`; an unknown tool is `-32602`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub code: i32,
    pub message: String,
}

impl ToolError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message)
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(crate::mcp::jsonrpc::INVALID_PARAMS, message)
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

/// The server's tool table and dispatch entry point.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;

    async fn call(&self, name: &str, args: Value, ctx: CallContext) -> Result<Value, ToolError>;
}

/// A serve-loop failure. EOF and shutdown are clean and return `Ok(())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpError {
    message: String,
}

impl McpError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for McpError {}

struct CallHandle {
    cancel: CancellationToken,
}

#[derive(Default)]
struct Calls {
    by_key: HashMap<String, Arc<CallHandle>>,
    active: Vec<Arc<CallHandle>>,
}

struct ServerState {
    definitions: Vec<ToolDefinition>,
    calls: Mutex<Calls>,
    closing: AtomicBool,
    idle: Notify,
    serve_stop: Mutex<Option<CancellationToken>>,
    approver: Mutex<Arc<dyn Approver>>,
    elicitation: Mutex<Option<Arc<ElicitationApprover>>>,
    dropped: Arc<AtomicU64>,
}

impl ServerState {
    fn begin(&self, key: Option<String>, handle: Arc<CallHandle>) {
        let closing = {
            let mut calls = self.calls.lock().unwrap();
            calls.active.push(handle.clone());
            if let Some(key) = key {
                calls.by_key.insert(key, handle.clone());
            }
            self.closing.load(Ordering::SeqCst)
        };
        // A call that starts while the server is closing is cancelled at once:
        // mcp-go's workers can dequeue a queued tools/call while the queue
        // drains, and such a call must not run past the client that asked for it.
        if closing {
            handle.cancel.cancel();
        }
    }

    fn finish(&self, key: Option<&str>, handle: &Arc<CallHandle>) {
        let empty = {
            let mut calls = self.calls.lock().unwrap();
            if let Some(key) = key
                && calls
                    .by_key
                    .get(key)
                    .is_some_and(|current| Arc::ptr_eq(current, handle))
            {
                // A client that reuses a request ID while the first call runs
                // must not have its second call's registration removed by the
                // first call's cleanup.
                calls.by_key.remove(key);
            }
            if let Some(index) = calls
                .active
                .iter()
                .position(|current| Arc::ptr_eq(current, handle))
            {
                calls.active.remove(index);
            }
            calls.active.is_empty()
        };
        if empty {
            self.idle.notify_one();
        }
    }

    fn cancel_all(&self) {
        let active = self.calls.lock().unwrap().active.clone();
        for handle in active {
            handle.cancel.cancel();
        }
    }

    fn cancel_request(&self, key: &str) {
        let handle = self.calls.lock().unwrap().by_key.get(key).cloned();
        if let Some(handle) = handle {
            handle.cancel.cancel();
        }
    }

    fn active_count(&self) -> usize {
        self.calls.lock().unwrap().active.len()
    }

    async fn wait_idle(&self, deadline: Duration) -> bool {
        let end = tokio::time::Instant::now() + deadline;
        loop {
            if self.active_count() == 0 {
                return true;
            }
            let now = tokio::time::Instant::now();
            if now >= end {
                return self.active_count() == 0;
            }
            // Create the future before re-checking so a completion between the
            // check and the await is not lost (`notify_one` stores a permit).
            let notified = self.idle.notified();
            if self.active_count() == 0 {
                return true;
            }
            if tokio::time::timeout(end - now, notified).await.is_err() {
                return self.active_count() == 0;
            }
        }
    }

    fn set_approver(&self, approver: Arc<dyn Approver>, elicitation: Arc<ElicitationApprover>) {
        *self.approver.lock().unwrap() = approver;
        *self.elicitation.lock().unwrap() = Some(elicitation);
    }

    fn approver(&self) -> Arc<dyn Approver> {
        self.approver.lock().unwrap().clone()
    }

    fn elicitation(&self) -> Option<Arc<ElicitationApprover>> {
        self.elicitation.lock().unwrap().clone()
    }
}

/// Default approver until a transport is attached; it never grants.
struct DenyApprover;

#[async_trait]
impl Approver for DenyApprover {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        false
    }
}

/// The MCP server. `serve` accepts frames and `close` bounds shutdown.
#[derive(Clone)]
pub struct McpServer {
    name: String,
    version: String,
    handler: Arc<dyn ToolHandler>,
    state: Arc<ServerState>,
}

struct WorkItem {
    id: Option<RequestId>,
    params: Option<Value>,
}

struct PreparedCall {
    name: String,
    args: Value,
    meta: Option<Value>,
    key: Option<String>,
    progress_token: Option<Value>,
}

impl PreparedCall {
    fn new(id: &RequestId, params: Option<Value>) -> Result<Self, JsonRpcError> {
        let params = params.unwrap_or_else(|| json!({}));
        let Some(params) = params.as_object() else {
            return Err(unparsable("params must be an object"));
        };
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        // The handler receives the client's arguments exactly as sent; the
        // server never injects bookkeeping into the tool's argument object.
        let mut args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if args.is_null() {
            args = json!({});
        }
        if !args.is_object() {
            return Err(unparsable("arguments must be an object"));
        }

        // Go's hook writes `request.Params.Meta`, so `_meta` is a sibling of
        // `arguments`. A `null` `_meta` behaves like an absent one.
        let mut meta = match params.get("_meta") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(meta)) => meta.clone(),
            Some(_) => return Err(unparsable("_meta must be an object")),
        };
        let progress_token = meta
            .get("progressToken")
            .filter(|token| !token.is_null())
            .cloned();

        // The canonical key always comes from the JSON-RPC request id, never
        // from a client-supplied `_meta` (neither `params._meta` nor
        // `arguments._meta`). A non-scalar id cannot be keyed, so nothing is
        // injected, matching Go's early return.
        let key = id.canonical.clone().filter(|key| !key.is_empty());
        if let Some(canonical) = &key {
            meta.insert(REQUEST_ID_META_KEY.to_string(), json!(canonical));
        }
        let meta = if meta.is_empty() {
            None
        } else {
            Some(Value::Object(meta))
        };

        Ok(Self {
            name,
            args,
            meta,
            key,
            progress_token,
        })
    }
}

fn unparsable(detail: &str) -> JsonRpcError {
    JsonRpcError::new(
        INVALID_REQUEST,
        format!("unparsable tools/call request: {detail}"),
    )
}

impl McpServer {
    pub fn new(name: String, version: String, handler: Arc<dyn ToolHandler>) -> Self {
        let definitions = handler.definitions();
        Self {
            name,
            version,
            handler,
            state: Arc::new(ServerState {
                definitions,
                calls: Mutex::new(Calls::default()),
                closing: AtomicBool::new(false),
                idle: Notify::new(),
                serve_stop: Mutex::new(None),
                approver: Mutex::new(Arc::new(DenyApprover)),
                elicitation: Mutex::new(None),
                dropped: Arc::new(AtomicU64::new(0)),
            }),
        }
    }

    /// Serves frames until stdin EOF, `close`, or a channel failure.
    ///
    /// EOF returns without waiting for the worker pool: the caller enters the
    /// same bounded cleanup path as a signal and calls [`Self::close`].
    pub async fn serve(
        &self,
        frames: mpsc::Receiver<String>,
        out: mpsc::Sender<String>,
    ) -> Result<(), McpError> {
        let stop = CancellationToken::new();
        {
            let mut slot = self.state.serve_stop.lock().unwrap();
            *slot = Some(stop.clone());
            if self.state.closing.load(Ordering::SeqCst) {
                stop.cancel();
            }
        }
        // Task 7 implements the approval behavior; the transport is attached
        // here so every call sees the connection that can answer elicitations.
        let elicitation = Arc::new(ElicitationApprover::new(out.clone()));
        self.state.set_approver(elicitation.clone(), elicitation);

        let result = self.serve_loop(frames, out, stop).await;
        *self.state.serve_stop.lock().unwrap() = None;
        result
    }

    async fn serve_loop(
        &self,
        mut frames: mpsc::Receiver<String>,
        out: mpsc::Sender<String>,
        stop: CancellationToken,
    ) -> Result<(), McpError> {
        let (queue_tx, queue_rx) = mpsc::channel::<WorkItem>(QUEUE_CAPACITY);
        let queue_rx = Arc::new(tokio::sync::Mutex::new(queue_rx));
        for _ in 0..WORKER_POOL_SIZE {
            let server = self.clone();
            let receiver = queue_rx.clone();
            let writer = out.clone();
            tokio::spawn(async move {
                loop {
                    let item = {
                        let mut receiver = receiver.lock().await;
                        receiver.recv().await
                    };
                    let Some(item) = item else {
                        break;
                    };
                    // A tools/call notification occupies a worker slot exactly
                    // like mcp-go's stdio queue, but HandleMessage treats it as
                    // a notification: no handler runs and no response is sent.
                    if let Some(id) = item.id {
                        server.run_tool_call(id, item.params, &writer).await;
                    }
                }
            });
        }

        loop {
            let line = tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(()),
                line = frames.recv() => match line {
                    Some(line) => line,
                    None => return Ok(()),
                },
            };

            // mcp-go's stdio layer pre-parses the line into json.RawMessage and
            // reports a bare "Parse error" when that fails; a valid JSON frame
            // that cannot decode into its message struct is HandleMessage's
            // "Failed to parse message" instead.
            if serde_json::from_str::<Value>(&line).is_err() {
                self.send_error(&out, None, PARSE_ERROR, "Parse error")
                    .await;
                continue;
            }

            match parse_frame(&line) {
                // Responses to server-initiated requests are not requests. Task
                // 5 normalizes error-only frames into this variant; they must be
                // ignored rather than answered with `Method  not found`. An
                // elicitation response is routed to the pending approval instead.
                Ok(Frame::Response(response)) => {
                    if let Some(approver) = self.state.elicitation() {
                        approver.handle_response(&response).await;
                    }
                }
                Ok(Frame::Notification(notification)) => {
                    match notification.method.as_str() {
                        "notifications/cancelled" => {
                            self.handle_cancelled(notification.params);
                        }
                        "tools/call" => {
                            self.dispatch_work(None, notification.params, &queue_tx, &out)
                                .await;
                        }
                        // notifications/initialized and unknown notifications
                        // are accepted and never answered.
                        _ => {}
                    }
                }
                Ok(Frame::Request(request)) => match request.method.as_str() {
                    "initialize" => {
                        let result = self.initialize_result(request.params.as_ref());
                        self.send_result(&out, request.id, result).await;
                    }
                    "ping" => {
                        self.send_result(&out, request.id, json!({})).await;
                    }
                    "tools/list" => {
                        let result = self.tools_list_result();
                        self.send_result(&out, request.id, result).await;
                    }
                    "tools/call" => {
                        self.dispatch_work(Some(request.id), request.params, &queue_tx, &out)
                            .await;
                    }
                    "logging/setLevel" => {
                        self.send_error(
                            &out,
                            Some(request.id),
                            METHOD_NOT_FOUND,
                            "logging not supported",
                        )
                        .await;
                    }
                    "resources/list" | "resources/templates/list" | "resources/read" => {
                        self.send_error(
                            &out,
                            Some(request.id),
                            METHOD_NOT_FOUND,
                            "resources not supported",
                        )
                        .await;
                    }
                    "prompts/list" | "prompts/get" => {
                        self.send_error(
                            &out,
                            Some(request.id),
                            METHOD_NOT_FOUND,
                            "prompts not supported",
                        )
                        .await;
                    }
                    "tasks/get" | "tasks/list" | "tasks/result" | "tasks/cancel" => {
                        self.send_error(
                            &out,
                            Some(request.id),
                            METHOD_NOT_FOUND,
                            "tasks not supported",
                        )
                        .await;
                    }
                    "completion/complete" => {
                        self.send_error(
                            &out,
                            Some(request.id),
                            METHOD_NOT_FOUND,
                            "completions not supported",
                        )
                        .await;
                    }
                    method => {
                        self.send_error(
                            &out,
                            Some(request.id),
                            METHOD_NOT_FOUND,
                            format!("Method {method} not found"),
                        )
                        .await;
                    }
                },
                Err(error) => {
                    self.send_error(&out, error.id, error.code, error.message)
                        .await;
                }
            }
        }
    }

    /// Cancels every active call and waits up to `deadline` for their handlers.
    /// Queued calls are cancelled as they begin because `closing` is set first.
    pub async fn close(&self, deadline: Duration) {
        self.state.closing.store(true, Ordering::SeqCst);
        if let Some(stop) = self.state.serve_stop.lock().unwrap().clone() {
            stop.cancel();
        }
        self.state.cancel_all();
        let _ = self.state.wait_idle(deadline).await;
    }

    fn initialize_result(&self, params: Option<&Value>) -> Value {
        let requested = params
            .and_then(|params| params.get("protocolVersion"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let protocol_version = if requested.is_empty() {
            DEFAULT_PROTOCOL_VERSION
        } else if VALID_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            LATEST_PROTOCOL_VERSION
        };
        json!({
            "protocolVersion": protocol_version,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": self.name, "version": self.version},
        })
    }

    fn tools_list_result(&self) -> Value {
        let tools: Vec<Value> = self
            .state
            .definitions
            .iter()
            .map(|definition| {
                let mut tool = Map::new();
                tool.insert("name".into(), json!(definition.name));
                if !definition.description.is_empty() {
                    tool.insert("description".into(), json!(definition.description));
                }
                tool.insert("inputSchema".into(), definition.input_schema.clone());
                // mcp-go only emits outputSchema when the schema has a type.
                if definition
                    .output_schema
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| !kind.is_empty())
                {
                    tool.insert("outputSchema".into(), definition.output_schema.clone());
                }
                // mcp.Tool.MarshalJSON always emits annotations, even `{}`.
                tool.insert(
                    "annotations".into(),
                    definition.annotations.clone().unwrap_or_else(|| json!({})),
                );
                Value::Object(tool)
            })
            .collect();
        json!({"tools": tools})
    }

    async fn dispatch_work(
        &self,
        id: Option<RequestId>,
        params: Option<Value>,
        queue: &mpsc::Sender<WorkItem>,
        out: &mpsc::Sender<String>,
    ) {
        let item = WorkItem { id, params };
        match queue.try_send(item) {
            Ok(()) => {}
            // mcp-go processes the call inline when its queue is full, so up to
            // 33 handlers may be active and the read loop stops meanwhile.
            Err(TrySendError::Full(item) | TrySendError::Closed(item)) => {
                if let Some(id) = item.id {
                    self.run_tool_call(id, item.params, out).await;
                }
            }
        }
    }

    async fn run_tool_call(
        &self,
        id: RequestId,
        params: Option<Value>,
        out: &mpsc::Sender<String>,
    ) {
        let mut prepared = match PreparedCall::new(&id, params) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.send_error(out, Some(id), error.code, error.message)
                    .await;
                return;
            }
        };

        if !self
            .state
            .definitions
            .iter()
            .any(|definition| definition.name == prepared.name)
        {
            self.send_error(
                out,
                Some(id),
                INVALID_PARAMS,
                format!("tool '{}' not found: tool not found", prepared.name),
            )
            .await;
            return;
        }

        let handle = Arc::new(CallHandle {
            cancel: CancellationToken::new(),
        });
        self.state.begin(prepared.key.clone(), handle.clone());

        // Go emits `subagent/event` for every call; the progress token only
        // controls whether `notifications/progress` is sent alongside it.
        let progress = Some(ProgressSink::new(
            prepared.progress_token,
            out.clone(),
            self.state.dropped.clone(),
        ));
        let ctx = CallContext {
            cancel: handle.cancel.clone(),
            request_id: Some(id.clone()),
            meta: prepared.meta.take(),
            progress,
            approver: self.state.approver(),
        };

        // Catch panics in the handler *future*, so a panic after the first
        // `.await` still becomes -32603 and the server keeps serving.
        let future = self.handler.call(&prepared.name, prepared.args, ctx);
        let outcome = AssertUnwindSafe(future).catch_unwind().await;

        self.state.finish(prepared.key.as_deref(), &handle);

        match outcome {
            Ok(Ok(result)) => self.send_result(out, id, result).await,
            Ok(Err(error)) => {
                self.send_error(out, Some(id), error.code, error.message)
                    .await;
            }
            Err(payload) => {
                let message = format!(
                    "panic recovered in {} tool handler: {}",
                    prepared.name,
                    panic_value(payload)
                );
                self.send_error(out, Some(id), INTERNAL_ERROR, message)
                    .await;
            }
        }
    }

    fn handle_cancelled(&self, params: Option<Value>) {
        let Some(request_id) = params.as_ref().and_then(|params| params.get("requestId")) else {
            return;
        };
        let Some(key) = canonical_request_id(request_id) else {
            return;
        };
        self.state.cancel_request(&key);
    }

    async fn send_result(&self, out: &mpsc::Sender<String>, id: RequestId, result: Value) {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id.raw,
            "result": result,
        });
        let _ = out.send(frame.to_string()).await;
    }

    async fn send_error(
        &self,
        out: &mpsc::Sender<String>,
        id: Option<RequestId>,
        code: i32,
        message: impl Into<String>,
    ) {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id.map(|id| id.raw).unwrap_or(Value::Null),
            "error": {"code": code, "message": message.into()},
        });
        let _ = out.send(frame.to_string()).await;
    }
}

fn panic_value(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(value) = payload.downcast_ref::<&'static str>() {
        (*value).to_string()
    } else if let Some(value) = payload.downcast_ref::<String>() {
        value.clone()
    } else {
        "Box<dyn Any>".to_string()
    }
}
