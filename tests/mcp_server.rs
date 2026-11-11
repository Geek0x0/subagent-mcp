//! MCP stdio server core: dispatch, worker pool, protocol surface, and
//! cancellation. The protocol goldens in `tests/fixtures/mcp/protocol.jsonl`
//! were captured from the Go 0.13.1 binary (mcp-go v0.44.0) before cutover and
//! are the source of truth for the wire shape.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use subagent_mcp::mcp::approval::{ElicitationApprover, request_approval, sanitize_approval_field};
use subagent_mcp::mcp::jsonrpc::{RequestId, Response, RpcError};
use subagent_mcp::mcp::progress::ProgressSink;
use subagent_mcp::mcp::server::{
    CallContext, McpError, McpServer, ToolDefinition, ToolError, ToolHandler,
};
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
struct RecordedCall {
    name: String,
    args: Value,
    meta: Option<Value>,
    canonical: Option<String>,
}

struct FakeInner {
    active: AtomicUsize,
    max_active: AtomicUsize,
    started: AtomicUsize,
    calls: Mutex<Vec<RecordedCall>>,
    release: Notify,
    inline_release: Notify,
}

#[derive(Clone)]
struct FakeHandler {
    inner: Arc<FakeInner>,
}

impl FakeHandler {
    fn new() -> Self {
        Self {
            inner: Arc::new(FakeInner {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                started: AtomicUsize::new(0),
                calls: Mutex::new(Vec::new()),
                release: Notify::new(),
                inline_release: Notify::new(),
            }),
        }
    }

    fn started(&self) -> usize {
        self.inner.started.load(Ordering::SeqCst)
    }

    fn max_active(&self) -> usize {
        self.inner.max_active.load(Ordering::SeqCst)
    }

    fn release_one(&self) {
        self.inner.release.notify_one();
    }

    /// Releases only the call that ran inline in the read loop. Releasing a
    /// pooled worker would not unblock the read loop.
    fn release_inline(&self) {
        self.inner.inline_release.notify_one();
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.inner.calls.lock().unwrap().clone()
    }
}

struct ActiveGuard {
    inner: Arc<FakeInner>,
}

impl ActiveGuard {
    fn new(inner: Arc<FakeInner>) -> Self {
        let active = inner.active.fetch_add(1, Ordering::SeqCst) + 1;
        inner.max_active.fetch_max(active, Ordering::SeqCst);
        Self { inner }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.inner.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ToolHandler for FakeHandler {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "fail".into(),
                description: "returns an error".into(),
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                annotations: Some(json!({"readOnlyHint": true})),
            },
            ToolDefinition {
                name: "boom".into(),
                description: "panics after an await".into(),
                input_schema: json!({"type": "object", "properties": {}}),
                output_schema: Value::Null,
                annotations: None,
            },
            ToolDefinition {
                name: "echo".into(),
                description: "returns its arguments".into(),
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                annotations: None,
            },
            ToolDefinition {
                name: "wait".into(),
                description: "waits for cancellation or release".into(),
                input_schema: json!({"type": "object"}),
                output_schema: Value::Null,
                annotations: None,
            },
            ToolDefinition {
                name: "slow".into(),
                description: "blocks until released".into(),
                input_schema: json!({"type": "object"}),
                output_schema: Value::Null,
                annotations: None,
            },
            ToolDefinition {
                name: "queued".into(),
                description: "returns immediately".into(),
                input_schema: json!({"type": "object"}),
                output_schema: Value::Null,
                annotations: None,
            },
            ToolDefinition {
                name: "inline".into(),
                description: "blocks until released".into(),
                input_schema: json!({"type": "object"}),
                output_schema: Value::Null,
                annotations: None,
            },
            ToolDefinition {
                name: "emit".into(),
                description: "emits progress events".into(),
                input_schema: json!({"type": "object"}),
                output_schema: Value::Null,
                annotations: None,
            },
        ]
    }

    async fn call(&self, name: &str, args: Value, ctx: CallContext) -> Result<Value, ToolError> {
        let _active = ActiveGuard::new(self.inner.clone());
        self.inner.started.fetch_add(1, Ordering::SeqCst);
        self.inner.calls.lock().unwrap().push(RecordedCall {
            name: name.to_string(),
            args: args.clone(),
            meta: ctx.meta.clone(),
            canonical: ctx.request_id.as_ref().and_then(|id| id.canonical.clone()),
        });

        match name {
            "fail" => Err(ToolError::internal("handler exploded")),
            "boom" => {
                // The panic must be caught after the first await, not when the
                // future is created.
                tokio::task::yield_now().await;
                panic!("kaboom");
            }
            "wait" => {
                tokio::select! {
                    _ = ctx.cancel.cancelled() => Ok(json!({"cancelled": true})),
                    _ = self.inner.release.notified() => Ok(json!({"ok": true})),
                }
            }
            "slow" => {
                self.inner.release.notified().await;
                Ok(json!({"ok": true}))
            }
            "inline" => {
                self.inner.inline_release.notified().await;
                Ok(json!({"ok": true}))
            }
            "emit" => {
                let thread_id = args
                    .get("threadId")
                    .and_then(Value::as_str)
                    .unwrap_or("thread");
                let events = args
                    .get("events")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if let Some(progress) = ctx.progress.as_ref() {
                    for event in &events {
                        progress.emit(thread_id, event);
                    }
                }
                Ok(json!({"emitted": events.len()}))
            }
            "queued" => Ok(json!({"ok": true})),
            other => Ok(json!({"name": other, "args": args})),
        }
    }
}

struct Harness {
    server: Arc<McpServer>,
    frames: mpsc::Sender<String>,
    out: mpsc::Receiver<String>,
    join: tokio::task::JoinHandle<Result<(), McpError>>,
    next_id: AtomicUsize,
}

impl Harness {
    fn start(handler: FakeHandler) -> Self {
        let (frames_tx, frames_rx) = mpsc::channel(1);
        let (out_tx, out_rx) = mpsc::channel(128);
        let server = Arc::new(McpServer::new(
            "subagent-mcp".into(),
            "0.13.1".into(),
            Arc::new(handler),
        ));
        let serving = server.clone();
        let join = tokio::spawn(async move { serving.serve(frames_rx, out_tx).await });
        Self {
            server,
            frames: frames_tx,
            out: out_rx,
            join,
            next_id: AtomicUsize::new(0),
        }
    }

    async fn send_frame(&self, frame: &str) {
        self.frames
            .send(frame.to_string())
            .await
            .expect("server should still be reading frames");
    }

    async fn send_call(&self, tool: &str) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": tool, "arguments": {}},
        });
        self.send_frame(&frame.to_string()).await;
    }

    async fn send_call_with_raw_id(&self, raw_id: &str, tool: &str) {
        let frame = format!(
            r#"{{"jsonrpc":"2.0","id":{raw_id},"method":"tools/call","params":{{"name":"{tool}","arguments":{{}}}}}}"#
        );
        self.send_frame(&frame).await;
    }

    async fn recv(&mut self) -> Value {
        let line = timeout(Duration::from_secs(2), self.out.recv())
            .await
            .expect("response should arrive")
            .expect("output channel should stay open");
        serde_json::from_str(&line).expect("server should emit valid JSON")
    }

    async fn recv_within(&mut self, duration: Duration) -> Option<Value> {
        let line = timeout(duration, self.out.recv()).await.ok().flatten()?;
        Some(serde_json::from_str(&line).expect("server should emit valid JSON"))
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send_frame(&frame.to_string()).await;
        let response = self.recv().await;
        assert_eq!(
            response["id"],
            json!(id),
            "response should echo the request id"
        );
        response
    }

    async fn wait_started(&self, handler: &FakeHandler, want: usize) {
        timeout(Duration::from_secs(2), async {
            while handler.started() < want {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("handlers should start");
    }
}

#[tokio::test]
async fn initialize_matches_mcp_go_protocol_surface() {
    let mut server = Harness::start(FakeHandler::new());

    for (params, want) in [
        (json!({}), "2025-03-26"),
        (
            json!({"protocolVersion": "", "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}),
            "2025-03-26",
        ),
        (
            json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}),
            "2025-11-25",
        ),
        (
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}),
            "2025-06-18",
        ),
        (
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}),
            "2025-03-26",
        ),
        (
            json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}),
            "2024-11-05",
        ),
        (
            json!({"protocolVersion": "1999-01-01", "capabilities": {}, "clientInfo": {"name": "c", "version": "1"}}),
            "2025-11-25",
        ),
    ] {
        let response = server.request("initialize", params).await;
        assert_eq!(response["result"]["protocolVersion"], json!(want));
        assert_eq!(
            response["result"]["capabilities"],
            json!({"tools": {}}),
            "capabilities must be exactly the tool capability"
        );
        assert_eq!(
            response["result"]["serverInfo"],
            json!({"name": "subagent-mcp", "version": "0.13.1"})
        );
        assert!(response["result"].get("instructions").is_none());
    }
}

#[tokio::test]
async fn ping_returns_an_empty_result() {
    let mut server = Harness::start(FakeHandler::new());
    let response = server.request("ping", json!({})).await;
    assert_eq!(response["result"], json!({}));
}

#[tokio::test]
async fn tools_list_includes_schemas_and_annotations() {
    let mut server = Harness::start(FakeHandler::new());
    let response = server.request("tools/list", json!({})).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list result should carry tools");

    let fail = tools
        .iter()
        .find(|tool| tool["name"] == "fail")
        .expect("fail tool should be listed");
    assert_eq!(fail["description"], "returns an error");
    assert_eq!(fail["inputSchema"], json!({"type": "object"}));
    assert_eq!(fail["outputSchema"], json!({"type": "object"}));
    assert_eq!(fail["annotations"], json!({"readOnlyHint": true}));

    let boom = tools
        .iter()
        .find(|tool| tool["name"] == "boom")
        .expect("boom tool should be listed");
    // mcp-go always emits the annotations object, even when it is empty.
    assert_eq!(boom["annotations"], json!({}));
    assert!(
        boom.get("outputSchema").is_none(),
        "an empty output schema must be omitted like mcp-go"
    );
}

#[tokio::test]
async fn tools_call_returns_the_handler_result() {
    let handler = FakeHandler::new();
    let mut server = Harness::start(handler.clone());
    let response = server
        .request("tools/call", json!({"name": "echo", "arguments": {"x": 1}}))
        .await;
    assert_eq!(response["result"]["name"], "echo");
    assert_eq!(response["result"]["args"], json!({"x": 1}));

    // The server must not leak its request-id bookkeeping into the tool's
    // arguments. Go writes it to `params._meta`, never `arguments`.
    let args = &response["result"]["args"];
    assert!(
        args.get("_meta").is_none(),
        "arguments must not gain a _meta object: {args}"
    );
    assert!(
        args.get("subagent-mcp/requestId").is_none(),
        "arguments must not gain the request-id key: {args}"
    );

    // The handler itself also saw the arguments unchanged.
    let calls = handler.calls();
    assert_eq!(calls[0].args, json!({"x": 1}));
    assert!(calls[0].args.get("_meta").is_none());
}

#[tokio::test]
async fn unknown_method_uses_the_mcp_go_error_text() {
    let mut server = Harness::start(FakeHandler::new());
    let response = server.request("bogus/method", json!({})).await;
    assert_eq!(response["error"]["code"], -32601);
    assert_eq!(
        response["error"]["message"],
        "Method bogus/method not found"
    );

    let id = 999;
    server
        .send_frame(&format!(r#"{{"jsonrpc":"2.0","id":{id}}}"#))
        .await;
    let response = server.recv().await;
    assert_eq!(response["id"], id);
    assert_eq!(response["error"]["code"], -32601);
    assert_eq!(response["error"]["message"], "Method  not found");
}

#[tokio::test]
async fn malformed_frames_use_the_mcp_go_frame_errors() {
    let mut server = Harness::start(FakeHandler::new());

    server.send_frame("not json").await;
    let response = server.recv().await;
    assert_eq!(response["id"], Value::Null);
    assert_eq!(response["error"]["code"], -32700);
    assert_eq!(response["error"]["message"], "Parse error");

    server.send_frame("[]").await;
    let response = server.recv().await;
    assert_eq!(response["id"], Value::Null);
    assert_eq!(response["error"]["code"], -32700);
    assert_eq!(response["error"]["message"], "Failed to parse message");

    server
        .send_frame(r#"{"jsonrpc":"1.0","id":5,"method":"tools/list"}"#)
        .await;
    let response = server.recv().await;
    assert_eq!(response["id"], 5);
    assert_eq!(response["error"]["code"], -32600);
    assert_eq!(response["error"]["message"], "Invalid JSON-RPC version");
}

#[tokio::test]
async fn request_id_is_injected_into_params_meta_and_cancels_the_call() {
    let handler = FakeHandler::new();
    let mut server = Harness::start(handler.clone());

    // The client sends its own params._meta; the server must preserve it and
    // add the canonical request id next to it.
    server
        .send_frame(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"wait","arguments":{"x":1},"_meta":{"client":"value"}}}"#,
        )
        .await;
    server.wait_started(&handler, 1).await;

    let calls = handler.calls();
    assert_eq!(calls[0].name, "wait");
    assert_eq!(calls[0].canonical.as_deref(), Some("int64:7"));

    // (a) params._meta reaches the handler with the injected key.
    let meta = calls[0]
        .meta
        .as_ref()
        .expect("params._meta must reach the handler");
    assert_eq!(
        meta["subagent-mcp/requestId"], "int64:7",
        "the canonical request id must be injected into params._meta"
    );
    assert_eq!(
        meta["client"], "value",
        "the client's own params._meta must be preserved"
    );

    // (b) arguments are byte/value-unchanged and carry no _meta.
    assert_eq!(
        calls[0].args,
        json!({"x": 1}),
        "the handler must see the client's arguments unchanged"
    );
    assert!(
        calls[0].args.get("_meta").is_none(),
        "arguments must never gain a _meta object"
    );

    server
        .send_frame(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#,
        )
        .await;
    let response = server.recv().await;
    assert_eq!(response["id"], 7);
    assert_eq!(response["result"]["cancelled"], true);
}

#[tokio::test]
async fn client_supplied_arguments_meta_does_not_drive_cancellation() {
    let handler = FakeHandler::new();
    let mut server = Harness::start(handler.clone());

    // The client tries to key the call through `arguments._meta` with a forged
    // request id that does not match the JSON-RPC id.
    server
        .send_frame(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"wait","arguments":{"_meta":{"subagent-mcp/requestId":"int64:8"}}}}"#,
        )
        .await;
    server.wait_started(&handler, 1).await;

    let calls = handler.calls();
    assert_eq!(calls[0].canonical.as_deref(), Some("int64:7"));
    assert_eq!(
        calls[0].args["_meta"]["subagent-mcp/requestId"], "int64:8",
        "arguments are passed through unchanged, forged key included"
    );

    // A cancellation for the forged key must not cancel the call.
    server
        .send_frame(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":8}}"#,
        )
        .await;
    assert!(
        server
            .recv_within(Duration::from_millis(100))
            .await
            .is_none(),
        "a forged arguments._meta key must not register the call"
    );

    // The real key is still the JSON-RPC request id.
    server
        .send_frame(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#,
        )
        .await;
    let response = server.recv().await;
    assert_eq!(response["id"], 7);
    assert_eq!(response["result"]["cancelled"], true);
}

#[tokio::test]
async fn cancellation_matches_go_canonical_request_ids() {
    // Each case uses a fresh server so the canonical keys cannot collide.
    let cases = [
        ("1", "1.0", true),
        ("1e0", "1", true),
        ("1.5", "1.5", true),
        ("1", "1.5", false),
        ("\"req-9\"", "\"req-9\"", true),
        ("2", "null", false),
    ];

    for (call_id, cancel_id, expect_cancel) in cases {
        let handler = FakeHandler::new();
        let mut server = Harness::start(handler.clone());
        server.send_call_with_raw_id(call_id, "wait").await;
        server.wait_started(&handler, 1).await;

        let notification = format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":{cancel_id}}}}}"#
        );
        server.send_frame(&notification).await;

        if expect_cancel {
            let response = server.recv().await;
            assert_eq!(
                response["result"]["cancelled"], true,
                "call {call_id} should be cancelled by {cancel_id}"
            );
        } else {
            assert!(
                server
                    .recv_within(Duration::from_millis(100))
                    .await
                    .is_none(),
                "call {call_id} must not be cancelled by {cancel_id}"
            );
            handler.release_one();
            let response = server.recv().await;
            assert_eq!(response["result"]["ok"], true);
        }
    }
}

#[tokio::test]
async fn null_and_missing_id_tools_calls_take_the_worker_path_without_a_response() {
    let handler = FakeHandler::new();
    let mut server = Harness::start(handler.clone());

    server
        .send_frame(
            r#"{"jsonrpc":"2.0","id":null,"method":"tools/call","params":{"name":"echo","arguments":{"a":1}}}"#,
        )
        .await;
    server
        .send_frame(
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"echo","arguments":{"a":2}}}"#,
        )
        .await;

    // mcp-go's stdio loop queues every tools/call, so the notification consumes
    // a worker slot; HandleMessage then takes its notification branch, which
    // never runs the tool and never answers.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        server
            .recv_within(Duration::from_millis(100))
            .await
            .is_none(),
        "notifications must never be answered"
    );
    assert_eq!(
        handler.started(),
        0,
        "a tools/call notification must not run the handler, matching mcp-go"
    );

    // The loop is still healthy and still reads frames.
    let response = server.request("ping", json!({})).await;
    assert_eq!(response["result"], json!({}));
}

#[tokio::test]
async fn panic_after_the_first_await_becomes_internal_error() {
    let mut server = Harness::start(FakeHandler::new());

    let response = server
        .request("tools/call", json!({"name": "boom", "arguments": {}}))
        .await;
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(
        response["error"]["message"],
        "panic recovered in boom tool handler: kaboom"
    );

    // The server keeps serving after a recovered panic.
    let response = server.request("ping", json!({})).await;
    assert_eq!(response["result"], json!({}));
}

#[tokio::test]
async fn handler_errors_become_internal_error() {
    let mut server = Harness::start(FakeHandler::new());
    let response = server
        .request("tools/call", json!({"name": "fail", "arguments": {}}))
        .await;
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(response["error"]["message"], "handler exploded");
}

#[tokio::test]
async fn unknown_tool_uses_the_mcp_go_error_text() {
    let mut server = Harness::start(FakeHandler::new());
    let response = server
        .request("tools/call", json!({"name": "nope", "arguments": {}}))
        .await;
    assert_eq!(response["error"]["code"], -32602);
    assert_eq!(
        response["error"]["message"],
        "tool 'nope' not found: tool not found"
    );
}

#[tokio::test]
async fn queue_full_runs_inline_and_stops_reading() {
    let handler = FakeHandler::new();
    let mut server = Harness::start(handler.clone());

    for _ in 0..32 {
        server.send_call("slow").await;
    }
    server.wait_started(&handler, 32).await;

    for _ in 0..100 {
        server.send_call("queued").await;
    }
    // The 133rd call cannot enter the full queue, so mcp-go runs it inline in
    // the read loop and stops reading frames until it finishes.
    server.send_call_with_raw_id("9000", "inline").await;
    server.wait_started(&handler, 33).await;
    assert_eq!(handler.max_active(), 33, "the 133rd call runs inline");

    // Send a frame behind the blocked inline call. A read loop that had not
    // stopped reading would answer this immediately.
    let ping_id = 9001;
    server
        .send_frame(&format!(
            r#"{{"jsonrpc":"2.0","id":{ping_id},"method":"ping"}}"#
        ))
        .await;
    assert!(
        server
            .recv_within(Duration::from_millis(50))
            .await
            .is_none(),
        "the read loop must stop while the inline call runs"
    );

    // Release only the inline call. Releasing a pooled worker would prove
    // nothing: the inline call — and therefore the read loop — would stay
    // blocked while an unrelated `slow` worker answered.
    handler.release_inline();
    let inline = server.recv().await;
    assert_eq!(inline["id"], 9000);
    assert_eq!(inline["result"]["ok"], true);
    let pong = server.recv().await;
    assert_eq!(pong["id"], ping_id);
    assert_eq!(pong["result"], json!({}));
}

#[tokio::test]
async fn response_frames_are_ignored_and_never_answered() {
    let mut server = Harness::start(FakeHandler::new());

    // Task 5 normalizes a response frame (including an error-only one) into
    // Frame::Response. The server must not answer it with `Method  not found`.
    server
        .send_frame(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#)
        .await;
    server
        .send_frame(r#"{"jsonrpc":"2.0","id":10,"error":{"code":-32601,"message":"x"}}"#)
        .await;
    assert!(
        server
            .recv_within(Duration::from_millis(100))
            .await
            .is_none(),
        "responses must not produce a response"
    );

    let response = server.request("ping", json!({})).await;
    assert_eq!(response["result"], json!({}));
}

#[tokio::test]
async fn eof_returns_from_the_serve_loop() {
    let mut server = Harness::start(FakeHandler::new());
    let response = server.request("ping", json!({})).await;
    assert_eq!(response["result"], json!({}));

    drop(server.frames);
    let result = timeout(Duration::from_secs(1), server.join)
        .await
        .expect("serve should return on EOF")
        .expect("serve task should not panic");
    assert!(result.is_ok());
}

#[tokio::test]
async fn protocol_golden_transcripts_match_the_go_binary() {
    let fixture = include_str!("fixtures/mcp/protocol.jsonl");
    let mut server = Harness::start(FakeHandler::new());

    for line in fixture.lines().filter(|line| !line.trim().is_empty()) {
        let case: Value = serde_json::from_str(line).expect("golden line should be JSON");
        let name = case["name"].as_str().expect("golden case needs a name");
        if case["kind"] != "request" {
            continue;
        }
        let send = case["send"]
            .as_str()
            .expect("golden case needs a send frame");
        let want: Value = serde_json::from_str(
            case["recv"]
                .as_str()
                .expect("golden case needs a response frame"),
        )
        .expect("golden response should be JSON");

        server.send_frame(send).await;
        let got = server.recv().await;
        assert_eq!(got, want, "golden case {name} drifted from mcp-go");
    }
}

#[tokio::test]
async fn elicitation_golden_pins_the_request_shape() {
    let fixture = include_str!("fixtures/mcp/protocol.jsonl");
    let case = fixture
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|case| case["kind"] == "server-initiated")
        .expect("fixture should pin the elicitation request");

    let request: Value = serde_json::from_str(case["recv"].as_str().unwrap()).unwrap();
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["method"], "elicitation/create");
    assert!(
        request["id"].is_i64(),
        "mcp-go allocates integer request ids"
    );
    assert_eq!(
        request["params"]["requestedSchema"],
        json!({"type": "object", "properties": {}})
    );
    assert!(
        request["params"]["message"]
            .as_str()
            .unwrap()
            .starts_with("subagent-mcp approval request (thread ")
    );
}

#[tokio::test]
async fn close_cancels_active_calls_within_the_deadline() {
    let handler = FakeHandler::new();
    let mut server = Harness::start(handler.clone());
    server.send_call("wait").await;
    server.wait_started(&handler, 1).await;

    timeout(
        Duration::from_secs(1),
        server.server.close(Duration::from_secs(1)),
    )
    .await
    .expect("close should return within its deadline");
    let response = server.recv().await;
    assert_eq!(response["result"]["cancelled"], true);
}

fn run_context() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn approval_request() -> ApprovalRequest {
    ApprovalRequest {
        thread_id: "thread-1".into(),
        tool: "shell".into(),
        command: Some("echo hello".into()),
        path: None,
        reason: "test approval".into(),
    }
}

fn elicitation_response(id: Value, result: Value) -> Response {
    Response {
        id: RequestId::new(id),
        result: Some(result),
        error: None,
    }
}

async fn elicit(
    approver: &ElicitationApprover,
    rx: &mut mpsc::Receiver<String>,
    ctx: &RunContext,
    req: ApprovalRequest,
    response: Response,
) -> (Value, bool) {
    let fut = approver.approve(ctx, req);
    tokio::pin!(fut);
    let frame = tokio::select! {
        frame = rx.recv() => frame.expect("approval should send an elicitation request"),
        decision = &mut fut => panic!("approval resolved before the client answered: {decision}"),
    };
    let request: Value =
        serde_json::from_str(&frame).expect("elicitation request should be valid JSON");
    assert!(
        approver.handle_response(&response).await,
        "the response should match a pending elicitation"
    );
    (request, fut.await)
}

async fn collect_until_response(server: &mut Harness, id: i64) -> Vec<Value> {
    let mut frames = Vec::new();
    loop {
        let frame = server.recv().await;
        let done = frame["id"] == id;
        frames.push(frame);
        if done {
            return frames;
        }
    }
}

async fn next_frame(rx: &mut mpsc::Receiver<String>) -> Value {
    let line = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("a notification frame should arrive")
        .expect("the output channel should stay open");
    serde_json::from_str(&line).expect("frame should be JSON")
}

#[tokio::test]
async fn progress_emit_sends_event_and_maps_summaries() {
    let (tx, mut rx) = mpsc::channel(32);
    let sink = ProgressSink::new(Some(json!("tok-1")), tx, Arc::new(AtomicU64::new(0)));

    let events = [
        json!({"type": "task_started"}),
        json!({"type": "exec_command_begin", "command": "echo hi"}),
        json!({"type": "exec_command_begin", "paths": ["a.txt", "b.txt"]}),
        json!({"type": "exec_command_begin", "tool": "read_file", "path": "x"}),
        json!({"type": "agent_message", "message": "line one\nline two"}),
        json!({"type": "task_complete"}),
        json!({"type": "error", "message": "boom"}),
        json!({"type": "provider_request"}),
    ];
    for event in &events {
        sink.emit("thread-1", event);
    }

    let mut frames = Vec::new();
    while let Ok(line) = rx.try_recv() {
        frames.push(serde_json::from_str::<Value>(&line).expect("frame should be JSON"));
    }

    let event_frames: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame["method"] == "subagent/event")
        .collect();
    assert_eq!(event_frames.len(), events.len());
    for (frame, event) in event_frames.iter().zip(events.iter()) {
        assert_eq!(frame["params"]["threadId"], "thread-1");
        assert_eq!(frame["params"]["msg"], *event);
    }

    let progress_frames: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame["method"] == "notifications/progress")
        .collect();
    let want = [
        "started",
        "shell: echo hi",
        "apply_patch: a.txt, b.txt",
        "read_file: x",
        "agent: line one",
        "completed",
        "error: boom",
    ];
    assert_eq!(progress_frames.len(), want.len());
    for (index, frame) in progress_frames.iter().enumerate() {
        assert_eq!(frame["params"]["progressToken"], "tok-1");
        assert_eq!(frame["params"]["message"], want[index]);
        assert_eq!(
            frame["params"]["progress"].as_f64(),
            Some((index + 1) as f64)
        );
        assert!(frame["params"].get("total").is_none());
    }
}

#[tokio::test]
async fn progress_summary_caps_at_200_runes_with_ellipsis() {
    let (tx, mut rx) = mpsc::channel(8);
    let sink = ProgressSink::new(Some(json!(7)), tx, Arc::new(AtomicU64::new(0)));
    sink.emit(
        "thread",
        &json!({"type": "exec_command_begin", "command": "é".repeat(300)}),
    );

    let _event = next_frame(&mut rx).await;
    let frame = next_frame(&mut rx).await;
    let message = frame["params"]["message"].as_str().unwrap();
    assert_eq!(message.chars().count(), 201);
    assert!(message.ends_with('…'));
    assert!(message.starts_with("shell: "));
    assert_eq!(message.trim_end_matches('…').chars().count(), 200);
}

#[tokio::test]
async fn progress_notifications_are_per_call() {
    let (tx, mut rx) = mpsc::channel(16);
    let dropped = Arc::new(AtomicU64::new(0));
    let first = ProgressSink::new(Some(json!("tok-a")), tx.clone(), dropped.clone());
    let second = ProgressSink::new(Some(json!("tok-b")), tx, dropped);

    first.emit("thread-a", &json!({"type": "task_started"}));
    second.emit("thread-b", &json!({"type": "task_started"}));
    first.emit("thread-a", &json!({"type": "task_complete"}));

    let mut progress = Vec::new();
    while let Ok(line) = rx.try_recv() {
        let frame: Value = serde_json::from_str(&line).unwrap();
        if frame["method"] == "notifications/progress" {
            progress.push(frame);
        }
    }
    assert_eq!(progress.len(), 3);
    assert_eq!(progress[0]["params"]["progressToken"], "tok-a");
    assert_eq!(progress[0]["params"]["progress"].as_f64(), Some(1.0));
    assert_eq!(progress[1]["params"]["progressToken"], "tok-b");
    assert_eq!(progress[1]["params"]["progress"].as_f64(), Some(1.0));
    assert_eq!(progress[2]["params"]["progressToken"], "tok-a");
    assert_eq!(progress[2]["params"]["progress"].as_f64(), Some(2.0));
}

#[tokio::test]
async fn progress_without_a_token_still_emits_events() {
    let (tx, mut rx) = mpsc::channel(4);
    let sink = ProgressSink::new(None, tx, Arc::new(AtomicU64::new(0)));
    sink.emit("thread", &json!({"type": "task_started"}));

    let frame = next_frame(&mut rx).await;
    assert_eq!(frame["method"], "subagent/event");
    assert_eq!(frame["params"]["msg"]["type"], "task_started");
    assert!(
        rx.try_recv().is_err(),
        "no progress notification without a token"
    );
}

#[tokio::test]
async fn progress_drops_without_blocking_when_the_client_lags() {
    let (tx, mut rx) = mpsc::channel(1);
    let dropped = Arc::new(AtomicU64::new(0));
    let sink = ProgressSink::new(Some(json!("tok")), tx.clone(), dropped.clone());
    tx.try_send("occupied".to_string())
        .expect("the channel should have one free slot");

    sink.emit("thread", &json!({"type": "task_started"}));

    assert_eq!(
        dropped.load(Ordering::SeqCst),
        2,
        "both the event and the progress notification are dropped"
    );
    assert_eq!(rx.try_recv().unwrap(), "occupied");
}

#[tokio::test]
async fn progress_notifications_via_tools_call_are_scoped_to_the_call() {
    let mut server = Harness::start(FakeHandler::new());
    let call = json!({
        "jsonrpc": "2.0",
        "id": 41,
        "method": "tools/call",
        "params": {
            "name": "emit",
            "arguments": {
                "threadId": "t-41",
                "events": [
                    {"type": "task_started"},
                    {"type": "exec_command_begin", "command": "echo hi"},
                    {"type": "agent_message", "message": "one\ntwo"},
                    {"type": "task_complete"}
                ]
            },
            "_meta": {"progressToken": "tok-41"}
        }
    });
    server.send_frame(&call.to_string()).await;
    let frames = collect_until_response(&mut server, 41).await;

    assert_eq!(frames.last().unwrap()["result"]["emitted"], 4);
    let events: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame["method"] == "subagent/event")
        .collect();
    assert_eq!(events.len(), 4);
    for frame in &events {
        assert_eq!(frame["params"]["threadId"], "t-41");
    }
    let progress: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame["method"] == "notifications/progress")
        .collect();
    let want = ["started", "shell: echo hi", "agent: one", "completed"];
    assert_eq!(progress.len(), want.len());
    for (index, frame) in progress.iter().enumerate() {
        assert_eq!(frame["params"]["progressToken"], "tok-41");
        assert_eq!(frame["params"]["message"], want[index]);
        assert_eq!(
            frame["params"]["progress"].as_f64(),
            Some((index + 1) as f64)
        );
    }

    // A second call has its own token and its own progress counter.
    let call = json!({
        "jsonrpc": "2.0",
        "id": 42,
        "method": "tools/call",
        "params": {
            "name": "emit",
            "arguments": {"threadId": "t-42", "events": [{"type": "task_started"}]},
            "_meta": {"progressToken": "tok-42"}
        }
    });
    server.send_frame(&call.to_string()).await;
    let frames = collect_until_response(&mut server, 42).await;
    let progress: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame["method"] == "notifications/progress")
        .collect();
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0]["params"]["progressToken"], "tok-42");
    assert_eq!(progress[0]["params"]["progress"].as_f64(), Some(1.0));
}

#[tokio::test]
async fn events_without_a_progress_token_are_emitted() {
    let mut server = Harness::start(FakeHandler::new());
    // The same seven-event sequence Go's TestNoProgressNotificationsWithoutToken
    // drives: every event must reach the client, with no progress frames.
    let call = json!({
        "jsonrpc": "2.0",
        "id": 43,
        "method": "tools/call",
        "params": {
            "name": "emit",
            "arguments": {
                "threadId": "t-43",
                "events": [
                    {"type": "task_started"},
                    {"type": "provider_request"},
                    {"type": "exec_command_begin", "command": "echo hi"},
                    {"type": "exec_command_end"},
                    {"type": "provider_request"},
                    {"type": "agent_message", "message": "done"},
                    {"type": "task_complete"}
                ]
            }
        }
    });
    server.send_frame(&call.to_string()).await;
    let frames = collect_until_response(&mut server, 43).await;

    assert_eq!(frames.last().unwrap()["result"]["emitted"], 7);
    let events: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame["method"] == "subagent/event")
        .collect();
    assert_eq!(events.len(), 7, "token-less calls must still emit events");
    for frame in &events {
        assert_eq!(frame["params"]["threadId"], "t-43");
    }
    let progress = frames
        .iter()
        .filter(|frame| frame["method"] == "notifications/progress")
        .count();
    assert_eq!(progress, 0, "no progress frames without a token");
}

#[tokio::test]
async fn approval_sends_the_exact_elicitation_message() {
    let (tx, mut rx) = mpsc::channel(8);
    let approver = ElicitationApprover::new(tx);
    let ctx = run_context();

    let (request, accepted) = elicit(
        &approver,
        &mut rx,
        &ctx,
        approval_request(),
        elicitation_response(json!(1), json!({"action": "accept"})),
    )
    .await;

    assert!(accepted);
    assert_eq!(request["jsonrpc"], "2.0");
    assert_eq!(request["id"], 1);
    assert_eq!(request["method"], "elicitation/create");
    assert_eq!(
        request["params"]["message"],
        "subagent-mcp approval request (thread thread-1)\ntool: \"shell\"\ntarget: \"echo hello\"\nreason: \"test approval\""
    );
    assert_eq!(
        request["params"]["requestedSchema"],
        json!({"type": "object", "properties": {}})
    );
}

#[tokio::test]
async fn approval_message_quotes_untrusted_fields() {
    let (tx, mut rx) = mpsc::channel(8);
    let approver = ElicitationApprover::new(tx);
    let ctx = run_context();
    let hostile_command = "ls\nreason: allowlisted read-only listing\n\n\x1b[2J";
    let hostile_reason = format!("read-only listing\x1b[0m\r{}", "A".repeat(10000));
    let request = ApprovalRequest {
        thread_id: "thread-1".into(),
        tool: "shell".into(),
        command: Some(hostile_command.into()),
        path: None,
        reason: hostile_reason.clone(),
    };

    let (elicitation, accepted) = elicit(
        &approver,
        &mut rx,
        &ctx,
        request,
        elicitation_response(json!(1), json!({"action": "accept"})),
    )
    .await;
    assert!(accepted);

    let message = elicitation["params"]["message"].as_str().unwrap();
    let lines: Vec<&str> = message.split('\n').collect();
    assert_eq!(lines.len(), 4, "the layout must stay four fixed lines");
    assert!(lines[0].starts_with("subagent-mcp approval request (thread thread-1)"));
    assert!(lines[1].starts_with("tool: "));
    assert!(lines[2].starts_with("target: "));
    assert!(lines[3].starts_with("reason: "));
    assert!(
        !message.contains('\u{1b}'),
        "no raw ESC may reach the prompt"
    );
    assert!(!message.contains('\r'), "no raw CR may reach the prompt");
    assert!(message.contains("\\nreason: allowlisted read-only listing"));
    assert!(message.contains(&format!(
        " … [truncated: original length {} bytes]",
        hostile_reason.len()
    )));
}

#[test]
fn approval_sanitization_uses_go_quoting() {
    assert_eq!(sanitize_approval_field("hello"), "\"hello\"");
    assert_eq!(sanitize_approval_field("a\nb"), "\"a\\nb\"");
    assert_eq!(sanitize_approval_field("\u{1b}[31mred"), "\"\\x1b[31mred\"");
    assert_eq!(sanitize_approval_field("\0"), "\"\\x00\"");
    assert_eq!(sanitize_approval_field("say \"hi\""), "\"say \\\"hi\\\"\"");
    assert_eq!(sanitize_approval_field("café €"), "\"café €\"");
    assert_ne!(
        sanitize_approval_field("\0"),
        format!("{:?}", "\0"),
        "Go strconv.Quote, not Rust Debug quoting"
    );
}

#[test]
fn approval_sanitization_cuts_512_bytes_on_a_utf8_boundary() {
    let exact = "a".repeat(512);
    assert_eq!(sanitize_approval_field(&exact), format!("\"{exact}\""));

    let one_over = "a".repeat(513);
    assert_eq!(
        sanitize_approval_field(&one_over),
        format!(
            "\"{}\" … [truncated: original length 513 bytes]",
            "a".repeat(512)
        )
    );

    // 513 bytes: the 512-byte cut lands inside the two-byte `é`, so it backs
    // up to the previous UTF-8 boundary.
    let inside = format!("{}é", "a".repeat(511));
    assert_eq!(
        sanitize_approval_field(&inside),
        format!(
            "\"{}\" … [truncated: original length 513 bytes]",
            "a".repeat(511)
        )
    );

    // 514 bytes: byte 512 is a boundary, so the whole `é` survives the cut.
    let boundary = format!("{}é", "a".repeat(512));
    assert_eq!(
        sanitize_approval_field(&boundary),
        format!(
            "\"{}\" … [truncated: original length 514 bytes]",
            "a".repeat(512)
        )
    );
}

#[tokio::test]
async fn approval_decline_cancel_and_error_are_false() {
    let cases = [
        (
            "decline",
            elicitation_response(json!(1), json!({"action": "decline"})),
        ),
        (
            "cancel",
            elicitation_response(json!(1), json!({"action": "cancel"})),
        ),
        (
            "error",
            Response {
                id: RequestId::new(json!(1)),
                result: None,
                error: Some(RpcError {
                    code: -32603,
                    message: "elicitation failed".into(),
                    data: None,
                }),
            },
        ),
    ];

    for (name, response) in cases {
        let (tx, mut rx) = mpsc::channel(8);
        let approver = ElicitationApprover::new(tx);
        let (_, accepted) = elicit(
            &approver,
            &mut rx,
            &run_context(),
            approval_request(),
            response,
        )
        .await;
        assert!(!accepted, "{name} must not grant the approval");
    }
}

#[tokio::test]
async fn approval_ignores_responses_for_other_request_ids() {
    let (tx, mut rx) = mpsc::channel(8);
    let approver = ElicitationApprover::new(tx);
    let ctx = run_context();
    let fut = approver.approve(&ctx, approval_request());
    tokio::pin!(fut);
    tokio::select! {
        frame = rx.recv() => {
            let _: Value = serde_json::from_str(&frame.unwrap()).unwrap();
        }
        decision = &mut fut => panic!("approval resolved before the client answered: {decision}"),
    }

    let wrong = elicitation_response(json!(2), json!({"action": "accept"}));
    assert!(!approver.handle_response(&wrong).await);
    let right = elicitation_response(json!(1), json!({"action": "accept"}));
    assert!(approver.handle_response(&right).await);
    assert!(fut.await);
}

#[tokio::test(start_paused = true)]
async fn approval_times_out_after_five_minutes() {
    let (tx, _rx) = mpsc::channel(8);
    let approver = ElicitationApprover::new(tx);
    let ctx = run_context();
    let start = tokio::time::Instant::now();

    assert!(!approver.approve(&ctx, approval_request()).await);
    assert_eq!(start.elapsed(), Duration::from_secs(5 * 60));
}

#[tokio::test(start_paused = true)]
async fn approval_times_out_at_a_short_run_deadline() {
    let (tx, _rx) = mpsc::channel(8);
    let approver = ElicitationApprover::new(tx);
    let ctx = RunContext {
        cancel: CancellationToken::new(),
        deadline: Some(tokio::time::Instant::now() + Duration::from_secs(60)),
    };
    let start = tokio::time::Instant::now();

    assert!(!approver.approve(&ctx, approval_request()).await);
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(60),
        "the run deadline must win over the five-minute cap"
    );
}

#[tokio::test(start_paused = true)]
async fn approval_times_out_while_the_client_is_not_reading() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send("occupied".to_string())
        .expect("the channel should have one free slot");
    let approver = ElicitationApprover::new(tx);
    let ctx = run_context();
    let start = tokio::time::Instant::now();

    assert!(!approver.approve(&ctx, approval_request()).await);
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(5 * 60),
        "the timeout must bound the send as well as the response wait"
    );
    assert_eq!(rx.try_recv().unwrap(), "occupied");
}

#[tokio::test]
async fn approval_failure_is_false_when_the_client_is_gone() {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let approver = ElicitationApprover::new(tx);
    assert!(!approver.approve(&run_context(), approval_request()).await);
}

struct FixedApprover(bool);

#[async_trait::async_trait]
impl Approver for FixedApprover {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        self.0
    }
}

struct NeverApproves;

#[async_trait::async_trait]
impl Approver for NeverApproves {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        std::future::pending::<()>().await;
        true
    }
}

#[tokio::test]
async fn request_approval_returns_the_approver_decision() {
    let ctx = run_context();
    assert!(request_approval(&FixedApprover(true), &ctx, approval_request()).await);
    assert!(!request_approval(&FixedApprover(false), &ctx, approval_request()).await);
}

#[tokio::test]
async fn request_approval_denies_when_the_run_is_cancelled() {
    let ctx = run_context();
    ctx.cancel.cancel();
    assert!(!request_approval(&NeverApproves, &ctx, approval_request()).await);
}

#[tokio::test(start_paused = true)]
async fn request_approval_times_out_after_five_minutes() {
    let ctx = run_context();
    let start = tokio::time::Instant::now();
    assert!(!request_approval(&NeverApproves, &ctx, approval_request()).await);
    assert_eq!(start.elapsed(), Duration::from_secs(5 * 60));
}

#[tokio::test(start_paused = true)]
async fn request_approval_denies_at_a_short_run_deadline() {
    let ctx = RunContext {
        cancel: CancellationToken::new(),
        deadline: Some(tokio::time::Instant::now() + Duration::from_secs(60)),
    };
    let start = tokio::time::Instant::now();
    assert!(!request_approval(&NeverApproves, &ctx, approval_request()).await);
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(60),
        "approval must deny at the earlier run deadline, not wait five minutes"
    );
}
