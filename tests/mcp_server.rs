//! MCP stdio server core: dispatch, worker pool, protocol surface, and
//! cancellation. The protocol goldens in `tests/fixtures/mcp/protocol.jsonl`
//! were captured from the Go 0.13.1 binary (mcp-go v0.44.0) before cutover and
//! are the source of truth for the wire shape.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use subagent_mcp::mcp::server::{
    CallContext, McpError, McpServer, ToolDefinition, ToolError, ToolHandler,
};
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

#[derive(Clone, Debug)]
struct RecordedCall {
    name: String,
    args: Value,
    canonical: Option<String>,
}

struct FakeInner {
    active: AtomicUsize,
    max_active: AtomicUsize,
    started: AtomicUsize,
    calls: Mutex<Vec<RecordedCall>>,
    release: Notify,
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
        ]
    }

    async fn call(&self, name: &str, args: Value, ctx: CallContext) -> Result<Value, ToolError> {
        let _active = ActiveGuard::new(self.inner.clone());
        self.inner.started.fetch_add(1, Ordering::SeqCst);
        self.inner.calls.lock().unwrap().push(RecordedCall {
            name: name.to_string(),
            args: args.clone(),
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
            "slow" | "inline" => {
                self.inner.release.notified().await;
                Ok(json!({"ok": true}))
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
    let mut server = Harness::start(FakeHandler::new());
    let response = server
        .request("tools/call", json!({"name": "echo", "arguments": {"x": 1}}))
        .await;
    assert_eq!(response["result"]["name"], "echo");
    assert_eq!(response["result"]["args"]["x"], 1);
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
async fn request_id_is_injected_into_meta_and_cancels_the_call() {
    let handler = FakeHandler::new();
    let mut server = Harness::start(handler.clone());

    server.send_call_with_raw_id("7", "wait").await;
    server.wait_started(&handler, 1).await;

    let calls = handler.calls();
    assert_eq!(calls[0].name, "wait");
    assert_eq!(
        calls[0].args["_meta"]["subagent-mcp/requestId"], "int64:7",
        "the canonical request id must be injected into _meta"
    );
    assert_eq!(calls[0].canonical.as_deref(), Some("int64:7"));

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
    server.send_call("inline").await;
    server.wait_started(&handler, 33).await;

    assert_eq!(handler.max_active(), 33, "the 133rd call runs inline");
    assert!(
        server
            .recv_within(Duration::from_millis(50))
            .await
            .is_none(),
        "the read loop must stop while the inline call runs"
    );

    handler.release_one();
    assert!(
        server.recv_within(Duration::from_secs(1)).await.is_some(),
        "reads must resume after a worker is released"
    );
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
