//! Codex app-server JSON-RPC client and per-command child pool.
//!
//! Ports the transport half of Go `internal/provider/codexappserver`:
//! `client_test.go`, `pool_test.go`, and `subscription_test.go`. The scripted
//! peer is an in-memory duplex stream, so every assertion is about the wire
//! protocol the app-server sees, not an internal shortcut.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf, split,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::provider::codex::CodexAdapter;
use subagent_mcp::provider::codex::client::{
    Client, ClientError, MAX_QUEUED_EVENTS, Message, RpcError, Subscription,
};
use subagent_mcp::provider::codex::pool::{self, Conn, Dialed, Dialer, Pool};
use subagent_mcp::provider::{Provider, TurnRequest};
use subagent_mcp::util::run::RunContext;

const PEER_BUF: usize = 1 << 20;

fn ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn to_raw(value: &Value) -> Box<RawValue> {
    serde_json::value::to_raw_value(value).unwrap()
}

fn notification(method: &str, params: Value) -> Message {
    Message {
        method: Some(method.to_string()),
        params: Some(to_raw(&params)),
        ..Message::default()
    }
}

fn request(id: i64, method: &str, params: Value) -> Message {
    Message {
        id: Some(to_raw(&json!(id))),
        method: Some(method.to_string()),
        params: Some(to_raw(&params)),
        ..Message::default()
    }
}

fn response(id: Option<Box<RawValue>>, result: Value) -> Message {
    Message {
        id,
        result: Some(to_raw(&result)),
        ..Message::default()
    }
}

fn error_response(id: Option<Box<RawValue>>, code: i64, message: &str) -> Message {
    Message {
        id,
        error: Some(RpcError {
            code,
            message: message.to_string(),
        }),
        ..Message::default()
    }
}

fn raw_id(message: &Message) -> &str {
    message.id.as_ref().map(|id| id.get()).unwrap_or("")
}

struct Peer {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
}

impl Peer {
    async fn recv(&mut self) -> (Message, String) {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(2), self.reader.read_line(&mut line))
            .await
            .expect("peer read timed out")
            .expect("peer read");
        assert!(n > 0, "peer saw EOF");
        let message = serde_json::from_str(&line).expect("peer received invalid JSON");
        (message, line)
    }

    async fn send(&mut self, message: Message) {
        let mut line = serde_json::to_string(&message).expect("encode peer message");
        line.push('\n');
        tokio::time::timeout(
            Duration::from_secs(2),
            self.writer.write_all(line.as_bytes()),
        )
        .await
        .expect("peer write timed out")
        .expect("peer write");
        self.writer.flush().await.expect("peer flush");
    }
}

fn new_peer() -> (Arc<Client>, Peer) {
    let (client_side, peer_side) = tokio::io::duplex(PEER_BUF);
    let (client_read, client_write) = split(client_side);
    let (peer_read, peer_write) = split(peer_side);
    (
        Client::new(client_read, client_write),
        Peer {
            reader: BufReader::new(peer_read),
            writer: peer_write,
        },
    )
}

/// A writer whose first write never completes, like a child that stopped
/// draining its stdin.
struct WedgedWriter {
    started: Arc<Notify>,
}

impl AsyncWrite for WedgedWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.started.notify_one();
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Wait until the reader has consumed every message written before this call.
async fn barrier(client: &Arc<Client>, peer: &mut Peer) {
    let call_client = client.clone();
    let call = tokio::spawn(async move { call_client.call(&ctx(), "barrier", None).await });
    let (message, _) = peer.recv().await;
    peer.send(response(message.id, json!({}))).await;
    call.await.unwrap().expect("barrier call");
}

async fn next(sub: &Subscription, ctx: &RunContext) -> Message {
    tokio::time::timeout(Duration::from_secs(2), sub.next(ctx))
        .await
        .expect("subscription next timed out")
        .expect("subscription next")
}

#[tokio::test]
async fn call_round_trip_and_wire_format() {
    let (client, mut peer) = new_peer();
    let peer_task = tokio::spawn(async move {
        let (message, line) = peer.recv().await;
        assert_eq!(raw_id(&message), "1");
        assert_eq!(message.method.as_deref(), Some("model/list"));
        assert_eq!(message.params.as_ref().unwrap().get(), r#"{"cursor":""}"#);
        let wire: Value = serde_json::from_str(&line).unwrap();
        assert!(wire.get("jsonrpc").is_none());
        assert_eq!(wire["id"], json!(1));
        assert_eq!(wire["method"], json!("model/list"));
        peer.send(response(message.id, json!({"data": [{"id": "m"}]})))
            .await;
    });

    let result = client
        .call(&ctx(), "model/list", Some(json!({"cursor": ""})))
        .await
        .expect("call");
    assert_eq!(result, json!({"data": [{"id": "m"}]}));
    peer_task.await.unwrap();
}

#[tokio::test]
async fn call_into_decodes_typed_result() {
    let (client, mut peer) = new_peer();
    let peer_task = tokio::spawn(async move {
        let (message, _) = peer.recv().await;
        peer.send(response(message.id, json!({"userAgent": "codex/1.2.3"})))
            .await;
    });

    #[derive(serde::Deserialize)]
    struct Initialized {
        #[serde(rename = "userAgent")]
        user_agent: String,
    }
    let initialized: Initialized = client
        .call_into(&ctx(), "initialize", None)
        .await
        .expect("call_into");
    assert_eq!(initialized.user_agent, "codex/1.2.3");
    peer_task.await.unwrap();
}

#[tokio::test]
async fn call_error_returns_rpc_error() {
    let (client, mut peer) = new_peer();
    let peer_task = tokio::spawn(async move {
        let (message, _) = peer.recv().await;
        peer.send(error_response(message.id, -32000, "scripted failure"))
            .await;
        let (message, _) = peer.recv().await;
        peer.send(error_response(message.id, -32601, "method not found"))
            .await;
    });

    let err = client
        .call(&ctx(), "model/list", None)
        .await
        .expect_err("scripted failure");
    match err {
        ClientError::Rpc(rpc) => assert_eq!(rpc.message, "scripted failure"),
        other => panic!("want RPC error, got {other:?}"),
    }
    let err = client
        .call(&ctx(), "unknown", None)
        .await
        .expect_err("unknown method");
    match err {
        ClientError::Rpc(rpc) => assert_eq!(rpc.code, -32601),
        other => panic!("want RPC error, got {other:?}"),
    }
    peer_task.await.unwrap();
}

#[tokio::test]
async fn routes_notifications_by_thread() {
    let (client, mut peer) = new_peer();
    let a = client.subscribe("a");
    let b = client.subscribe("b");
    assert_eq!(a, client.subscribe("a"), "Subscribe is not idempotent");

    peer.send(notification("account/updated", json!({"status": "ok"})))
        .await;
    peer.send(notification("unknown/event", json!({"threadId": "nobody"})))
        .await;
    peer.send(notification(
        "item/delta",
        json!({"threadId": "a", "seq": 1}),
    ))
    .await;
    peer.send(notification(
        "item/delta",
        json!({"threadId": "b", "seq": 2}),
    ))
    .await;
    peer.send(notification(
        "item/delta",
        json!({"threadId": "a", "seq": 3}),
    ))
    .await;

    for (sub, thread, seq) in [(&a, "a", 1), (&a, "a", 3), (&b, "b", 2)] {
        let message = next(sub, &ctx()).await;
        let params: Value = serde_json::from_str(message.params.as_ref().unwrap().get()).unwrap();
        assert_eq!(message.method.as_deref(), Some("item/delta"));
        assert!(message.id.is_none());
        assert_eq!(params["threadId"], json!(thread));
        assert_eq!(params["seq"], json!(seq));
    }
}

#[tokio::test]
async fn reader_never_blocks_on_slow_subscriber() {
    let (client, mut peer) = new_peer();
    let _slow = client.subscribe("slow");
    let fast = client.subscribe("fast");

    for i in 0..10_000 {
        peer.send(notification(
            "item/delta",
            json!({"threadId": "slow", "itemId": "item", "delta": "x", "seq": i}),
        ))
        .await;
    }
    peer.send(notification("item/delta", json!({"threadId": "fast"})))
        .await;

    let message = next(&fast, &ctx()).await;
    assert_eq!(
        message.params.as_ref().unwrap().get(),
        r#"{"threadId":"fast"}"#
    );
}

#[tokio::test]
async fn answers_unrouted_server_requests() {
    let (client, mut peer) = new_peer();
    let _sub = client.subscribe("active");

    peer.send(request(1, "attestation/generate", json!({})))
        .await;
    peer.send(request(
        2,
        "item/tool/requestUserInput",
        json!({"threadId": "nobody"}),
    ))
    .await;
    peer.send(request(3, "unknown/request", json!({"threadId": 123})))
        .await;

    for id in 1..=3 {
        let (reply, line) = peer.recv().await;
        assert_eq!(raw_id(&reply), id.to_string());
        let error = reply.error.expect("unsupported reply error");
        assert_eq!(error.code, -32601);
        assert_eq!(error.message, "not supported by subagent-mcp");
        let wire: Value = serde_json::from_str(&line).unwrap();
        assert!(wire.get("jsonrpc").is_none());
    }
}

#[tokio::test]
async fn delivers_server_request_and_responds() {
    let (client, mut peer) = new_peer();
    let sub = client.subscribe("t");
    peer.send(request(
        7,
        "item/commandExecution/requestApproval",
        json!({"threadId": "t", "turnId": "turn-1", "command": "pwd"}),
    ))
    .await;

    let message = next(&sub, &ctx()).await;
    assert_eq!(raw_id(&message), "7");
    assert_eq!(
        message.method.as_deref(),
        Some("item/commandExecution/requestApproval")
    );
    client
        .respond(message.id.as_ref().unwrap(), json!({"decision": "accept"}))
        .await
        .expect("respond");

    let (reply, _) = peer.recv().await;
    assert_eq!(raw_id(&reply), "7");
    assert_eq!(
        serde_json::from_str::<Value>(reply.result.as_ref().unwrap().get()).unwrap(),
        json!({"decision": "accept"})
    );
    assert!(reply.error.is_none());
}

#[tokio::test]
async fn eof_fails_pending_calls_and_subscriptions() {
    let (client, mut peer) = new_peer();
    let blocked = client.subscribe("blocked");
    let waiting = client.subscribe("waiting");

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let peer_task = tokio::spawn(async move {
        let (message, _) = peer.recv().await;
        assert_eq!(message.method.as_deref(), Some("pending"));
        peer.send(request(1, "approval", json!({"threadId": "blocked"})))
            .await;
        entered_tx.send(()).unwrap();
        drop(peer);
    });

    let call_ctx = ctx();
    let call_client = client.clone();
    let call = tokio::spawn(async move { call_client.call(&call_ctx, "pending", None).await });
    entered_rx.await.unwrap();

    let call_err = call.await.unwrap().expect_err("pending call");
    assert!(call_err.is_eof(), "call error = {call_err}");
    let next_err = waiting.next(&ctx()).await.expect_err("waiting next");
    assert!(next_err.is_eof(), "next error = {next_err}");

    tokio::time::timeout(Duration::from_secs(2), client.done().cancelled())
        .await
        .expect("client done");
    assert!(client.error().is_some_and(|err| err.is_eof()));
    assert!(
        client
            .call(&ctx(), "after/exit", None)
            .await
            .expect_err("after EOF")
            .is_eof()
    );
    assert!(
        client
            .subscribe("after-exit")
            .next(&ctx())
            .await
            .expect_err("next after EOF")
            .is_eof()
    );
    assert!(
        blocked
            .next(&ctx())
            .await
            .expect_err("blocked next")
            .is_eof()
    );
    peer_task.await.unwrap();
}

#[tokio::test]
async fn close_writer_does_not_wait_for_wedged_write() {
    let (client_side, _peer_side) = tokio::io::duplex(64);
    let (client_read, _client_write) = split(client_side);
    let started = Arc::new(Notify::new());
    let client = Client::new(
        client_read,
        WedgedWriter {
            started: started.clone(),
        },
    );

    let writer = client.clone();
    let write = tokio::spawn(async move { writer.notify("wedged", None).await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("write never started");

    // A child that stops reading its stdin leaves the write pending while it
    // holds the writer; close must still return so the caller's kill timer runs.
    tokio::time::timeout(Duration::from_secs(2), client.close_writer())
        .await
        .expect("close_writer waited for an in-flight write");

    write.abort();
}

#[tokio::test]
async fn write_after_close_is_classified_closed_pipe() {
    let (client, _peer) = new_peer();
    client.close_writer().await;

    let err = client
        .notify("after-close", None)
        .await
        .expect_err("write after close");
    assert!(err.is_closed_pipe(), "post-close write error = {err}");
    assert_eq!(err.to_string(), "codex app-server writer closed");
}

#[tokio::test]
async fn concurrent_calls_complete_out_of_order() {
    let (client, mut peer) = new_peer();
    const COUNT: usize = 16;
    let peer_task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..COUNT {
            requests.push(peer.recv().await.0);
        }
        for message in requests.into_iter().rev() {
            let seq: i64 = serde_json::from_str(message.params.as_ref().unwrap().get()).unwrap();
            peer.send(response(message.id, json!(seq))).await;
        }
    });

    let mut calls = Vec::new();
    for i in 0..COUNT {
        let client = client.clone();
        calls.push(tokio::spawn(async move {
            let got = client.call(&ctx(), "echo", Some(json!(i))).await?;
            assert_eq!(got, json!(i));
            Ok::<(), ClientError>(())
        }));
    }
    for call in calls {
        call.await.unwrap().unwrap();
    }
    peer_task.await.unwrap();
}

#[tokio::test]
async fn cancellation_stops_call() {
    let (client, mut peer) = new_peer();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let peer_task = tokio::spawn(async move {
        let (message, _) = peer.recv().await;
        assert_eq!(message.method.as_deref(), Some("wait"));
        entered_tx.send(()).unwrap();
        let (message, _) = peer.recv().await;
        assert_eq!(message.method.as_deref(), Some("initialize"));
        peer.send(response(message.id, json!({}))).await;
    });

    let call_ctx = ctx();
    let cancel = call_ctx.cancel.clone();
    let call_client = client.clone();
    let call = tokio::spawn(async move { call_client.call(&call_ctx, "wait", None).await });
    entered_rx.await.unwrap();
    cancel.cancel();
    let err = call.await.unwrap().expect_err("canceled call");
    assert!(err.is_cancelled(), "call error = {err}");

    let already = ctx();
    already.cancel.cancel();
    let err = client
        .call(&already, "not-sent", None)
        .await
        .expect_err("already canceled");
    assert!(err.is_cancelled());
    client
        .call(&ctx(), "initialize", None)
        .await
        .expect("subsequent call");
    peer_task.await.unwrap();
}

#[tokio::test]
async fn unsubscribe_closes_queue_and_rejects_late_requests() {
    let (client, mut peer) = new_peer();
    let sub = client.subscribe("t");
    let waiter = {
        let sub = sub.clone();
        let ctx = ctx();
        tokio::spawn(async move { sub.next(&ctx).await })
    };
    client.unsubscribe("t").await;
    let err = waiter.await.unwrap().expect_err("unsubscribed next");
    assert_eq!(err.to_string(), "codex app-server subscription closed");

    peer.send(request(1, "approval", json!({"threadId": "t"})))
        .await;
    let (reply, _) = peer.recv().await;
    assert_eq!(raw_id(&reply), "1");
    assert_eq!(reply.error.unwrap().code, -32601);
    assert_ne!(sub, client.subscribe("t"));
}

#[tokio::test]
async fn large_messages() {
    let (client, mut peer) = new_peer();
    let sub = client.subscribe("t");
    let large = "x".repeat(256 * 1024);
    let large_for_peer = large.clone();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let peer_task = tokio::spawn(async move {
        let (message, _) = peer.recv().await;
        peer.send(response(message.id, json!(large_for_peer))).await;
        peer.send(notification(
            "large",
            json!({"threadId": "t", "text": large_for_peer}),
        ))
        .await;
        let _ = release_rx.await;
    });

    let result = client
        .call(&ctx(), "large", None)
        .await
        .expect("large call");
    assert_eq!(result.as_str().unwrap().len(), large.len());

    let message = next(&sub, &ctx()).await;
    let params: Value = serde_json::from_str(message.params.as_ref().unwrap().get()).unwrap();
    assert_eq!(params["text"].as_str().unwrap().len(), large.len());
    release_tx.send(()).unwrap();
    peer_task.await.unwrap();
}

#[tokio::test]
async fn decode_error_stops_reader() {
    let (client, mut peer) = new_peer();
    let sub = client.subscribe("waiting");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let peer_task = tokio::spawn(async move {
        let (message, _) = peer.recv().await;
        assert_eq!(message.method.as_deref(), Some("pending"));
        entered_tx.send(()).unwrap();
        peer.writer.write_all(b"{invalid\n").await.unwrap();
        peer.writer.flush().await.unwrap();
        drop(peer);
    });

    let call_ctx = ctx();
    let call_client = client.clone();
    let call = tokio::spawn(async move { call_client.call(&call_ctx, "pending", None).await });
    entered_rx.await.unwrap();
    let call_err = call.await.unwrap().expect_err("decode error");
    assert!(call_err.is_json(), "call error = {call_err}");
    let next_err = sub.next(&ctx()).await.expect_err("next decode error");
    assert!(next_err.is_json(), "next error = {next_err}");
    tokio::time::timeout(Duration::from_secs(2), client.done().cancelled())
        .await
        .expect("client done");
    assert!(client.error().is_some_and(|err| err.is_json()));
    peer_task.await.unwrap();
}

#[tokio::test]
async fn notify_and_initialize() {
    let (client, mut peer) = new_peer();
    let peer_task = tokio::spawn(async move {
        let (message, _) = peer.recv().await;
        assert_eq!(message.method.as_deref(), Some("initialize"));
        assert_eq!(
            serde_json::from_str::<Value>(message.params.as_ref().unwrap().get()).unwrap(),
            json!({"clientInfo": {"name": "test", "version": "1"}})
        );
        peer.send(response(message.id, json!({}))).await;
        let (message, line) = peer.recv().await;
        assert_eq!(message.method.as_deref(), Some("initialized"));
        assert!(message.params.is_none());
        let wire: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(wire, json!({"method": "initialized"}));
    });

    client
        .call(
            &ctx(),
            "initialize",
            Some(json!({"clientInfo": {"name": "test", "version": "1"}})),
        )
        .await
        .expect("initialize");
    client.notify("initialized", None).await.expect("notify");
    peer_task.await.unwrap();
}

#[tokio::test]
async fn subscription_merges_streamed_deltas() {
    let (client, mut peer) = new_peer();
    let sub = client.subscribe("t");
    for delta in ["Hel", "lo ", "wor", "ld"] {
        peer.send(notification(
            "item/agentMessage/delta",
            json!({"threadId": "t", "itemId": "item-1", "delta": delta}),
        ))
        .await;
    }
    peer.send(notification(
        "turn/completed",
        json!({"threadId": "t", "turn": {"id": "turn-1", "status": "completed"}}),
    ))
    .await;
    barrier(&client, &mut peer).await;

    let message = next(&sub, &ctx()).await;
    let params: Value = serde_json::from_str(message.params.as_ref().unwrap().get()).unwrap();
    assert_eq!(params["delta"], json!("Hello world"));
    assert_eq!(params["itemId"], json!("item-1"));
    let completed = next(&sub, &ctx()).await;
    assert_eq!(completed.method.as_deref(), Some("turn/completed"));
}

#[tokio::test]
async fn subscription_keeps_delta_runs_apart() {
    let (client, mut peer) = new_peer();
    let sub = client.subscribe("t");
    for (item, delta) in [("item-1", "a"), ("item-1", "b"), ("item-2", "c")] {
        peer.send(notification(
            "item/agentMessage/delta",
            json!({"threadId": "t", "itemId": item, "delta": delta}),
        ))
        .await;
    }
    peer.send(notification("turn/completed", json!({"threadId": "t"})))
        .await;
    peer.send(notification(
        "item/agentMessage/delta",
        json!({"threadId": "t", "itemId": "item-1", "delta": "d"}),
    ))
    .await;
    barrier(&client, &mut peer).await;

    for want in ["ab", "c"] {
        let message = next(&sub, &ctx()).await;
        let params: Value = serde_json::from_str(message.params.as_ref().unwrap().get()).unwrap();
        assert_eq!(params["delta"], json!(want));
    }
    assert_eq!(
        next(&sub, &ctx()).await.method.as_deref(),
        Some("turn/completed")
    );
    let last = next(&sub, &ctx()).await;
    let params: Value = serde_json::from_str(last.params.as_ref().unwrap().get()).unwrap();
    assert_eq!(params["delta"], json!("d"));
}

#[tokio::test]
async fn subscription_backlog_fails_and_is_recoverable() {
    let (client, mut peer) = new_peer();
    let sub = client.subscribe("t");
    for i in 0..=MAX_QUEUED_EVENTS {
        peer.send(notification(
            "item/agentMessage/delta",
            json!({"threadId": "t", "itemId": format!("item-{i}"), "delta": "d"}),
        ))
        .await;
    }
    barrier(&client, &mut peer).await;

    let err = sub.next(&ctx()).await.expect_err("backlog");
    assert!(
        err.to_string()
            .contains("backlog exceeded 4096 notifications"),
        "backlog error = {err}"
    );
    assert!(
        err.to_string().contains("no longer delivered"),
        "backlog error = {err}"
    );
    let replacement = client.subscribe("t");
    assert_ne!(sub, replacement);
    peer.send(notification(
        "item/agentMessage/delta",
        json!({"threadId": "t", "itemId": "item-ok", "delta": "ok"}),
    ))
    .await;
    barrier(&client, &mut peer).await;
    let message = next(&replacement, &ctx()).await;
    let params: Value = serde_json::from_str(message.params.as_ref().unwrap().get()).unwrap();
    assert_eq!(params["delta"], json!("ok"));
}

// --- Pool tests -----------------------------------------------------------

type PeerScript =
    Box<dyn FnOnce(Peer, Arc<DialStats>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

struct DialStats {
    dials: AtomicUsize,
    closed: AtomicUsize,
    received: Mutex<Vec<Message>>,
    entered: tokio::sync::Semaphore,
}

impl Default for DialStats {
    fn default() -> Self {
        Self {
            dials: AtomicUsize::new(0),
            closed: AtomicUsize::new(0),
            received: Mutex::new(Vec::new()),
            entered: tokio::sync::Semaphore::new(0),
        }
    }
}

struct FakeCloser {
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stats: Arc<DialStats>,
}

impl FakeCloser {
    fn close_now(&self) {
        if let Some(handle) = self.handle.lock().unwrap().take() {
            handle.abort();
        }
        self.stats.closed.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl pool::Closer for FakeCloser {
    async fn close(&self) {
        self.close_now();
    }

    fn close_blocking(&self) {
        self.close_now();
    }
}

fn fake_dialer(scripts: Vec<PeerScript>, stats: Arc<DialStats>) -> Dialer {
    fake_dialer_with_tail(scripts, stats, None)
}

fn fake_dialer_with_tail(
    scripts: Vec<PeerScript>,
    stats: Arc<DialStats>,
    stderr_tail: Option<&'static str>,
) -> Dialer {
    let scripts = Arc::new(Mutex::new(VecDeque::from(scripts)));
    Arc::new(move || {
        let scripts = scripts.clone();
        let stats = stats.clone();
        Box::pin(async move {
            let script = scripts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ClientError::Message("no scripted peer".to_string()))?;
            stats.dials.fetch_add(1, Ordering::SeqCst);
            let (client, peer) = new_peer();
            let handle = tokio::spawn(script(peer, stats.clone()));
            let stderr_tail = stderr_tail.map(|tail| {
                Arc::new(move || tail.to_string()) as Arc<dyn Fn() -> String + Send + Sync>
            });
            Ok(Dialed {
                client,
                closer: Arc::new(FakeCloser {
                    handle: Mutex::new(Some(handle)),
                    stats,
                }),
                stderr_tail,
            })
        })
    })
}

fn handshake_script(crash: Arc<Notify>, user_agent: &'static str) -> PeerScript {
    Box::new(move |mut peer, stats| {
        Box::pin(async move {
            let (message, _) = peer.recv().await;
            stats.received.lock().unwrap().push(message.clone());
            peer.send(response(message.id, json!({"userAgent": user_agent})))
                .await;
            let (message, _) = peer.recv().await;
            stats.received.lock().unwrap().push(message);
            crash.notified().await;
            drop(peer);
        })
    })
}

fn provider_config(command: &str) -> ProviderConfig {
    ProviderConfig {
        api: Api::CodexAppServer,
        base_url: String::new(),
        env_key: None,
        key: None,
        command: Some(command.to_string()),
        default_model: String::new(),
        models: Vec::new(),
        effort_map: Default::default(),
        max_output_tokens: None,
        max_nudges: None,
        extra_body: None,
    }
}

#[tokio::test]
async fn pool_shares_one_child_and_bumps_generations() {
    let stats = Arc::new(DialStats::default());
    let crash1 = Arc::new(Notify::new());
    let crash2 = Arc::new(Notify::new());
    let dialer = fake_dialer(
        vec![
            handshake_script(crash1.clone(), "fake-codex"),
            handshake_script(crash2.clone(), "fake-codex"),
        ],
        stats.clone(),
    );
    let pool = Arc::new(Pool::new(dialer));
    let ctx = ctx();

    let mut results = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let ctx = ctx.clone();
        results.push(tokio::spawn(async move { pool.get(&ctx).await }));
    }
    let mut first: Option<Arc<Conn>> = None;
    for result in results {
        let conn = result.await.unwrap().expect("acquire");
        if let Some(first) = &first {
            assert!(Arc::ptr_eq(first, &conn), "concurrent acquire dialed twice");
        } else {
            assert_eq!(conn.generation, 1);
            assert_eq!(conn.user_agent, "fake-codex");
            first = Some(conn);
        }
    }
    assert_eq!(stats.dials.load(Ordering::SeqCst), 1);
    {
        let received = stats.received.lock().unwrap();
        assert_eq!(received[0].method.as_deref(), Some("initialize"));
        let params: Value =
            serde_json::from_str(received[0].params.as_ref().unwrap().get()).unwrap();
        assert_eq!(
            params,
            json!({"clientInfo": {"name": "subagent-mcp", "version": subagent_mcp::VERSION}})
        );
        assert_eq!(received[1].method.as_deref(), Some("initialized"));
    }

    let first = first.unwrap();
    crash1.notify_one();
    tokio::time::timeout(Duration::from_secs(2), first.client.done().cancelled())
        .await
        .expect("first client done");

    let second = pool.get(&ctx).await.expect("second acquire");
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(second.generation, 2);
    assert_eq!(second.user_agent, "fake-codex");
    assert_eq!(stats.dials.load(Ordering::SeqCst), 2);
    assert_eq!(pool.generation(), 2);

    crash2.notify_one();
    pool.close().await;
}

#[tokio::test]
async fn pool_waiting_caller_can_cancel() {
    let stats = Arc::new(DialStats::default());
    let dialer = fake_dialer(
        vec![Box::new(|mut peer, stats| {
            Box::pin(async move {
                let (message, _) = peer.recv().await;
                stats.received.lock().unwrap().push(message);
                stats.entered.add_permits(1);
                std::future::pending::<()>().await;
            })
        })],
        stats.clone(),
    );
    let pool = Arc::new(Pool::new(dialer));

    let first_ctx = ctx();
    let first_cancel = first_ctx.cancel.clone();
    let first_pool = pool.clone();
    let first: tokio::task::JoinHandle<Result<Arc<Conn>, ClientError>> =
        tokio::spawn(async move { first_pool.get(&first_ctx).await });
    stats.entered.acquire().await.unwrap().forget();

    let second_ctx = ctx();
    second_ctx.cancel.cancel();
    let err = pool.get(&second_ctx).await.expect_err("canceled waiter");
    assert!(err.is_cancelled(), "waiting acquire error = {err}");

    first_cancel.cancel();
    let err = first.await.unwrap().expect_err("canceled initializer");
    assert!(err.is_cancelled(), "initializing acquire error = {err}");
    pool.close().await;
}

#[tokio::test]
async fn pool_failure_reports_child_exit_with_stderr_tail() {
    let stats = Arc::new(DialStats::default());
    let crash = Arc::new(Notify::new());
    let dialer = fake_dialer_with_tail(
        vec![handshake_script(crash.clone(), "fake-codex")],
        stats.clone(),
        Some("fatal: boom"),
    );
    let pool = Arc::new(Pool::new(dialer));
    let conn = pool.get(&ctx()).await.expect("acquire");

    crash.notify_one();
    tokio::time::timeout(Duration::from_secs(2), conn.client.done().cancelled())
        .await
        .expect("reader saw EOF");

    // EOF is the transport telling us the child went away: report why, not EOF.
    let err = conn
        .failure(ClientError::Io(Arc::new(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "EOF",
        ))))
        .await;
    assert_eq!(err.to_string(), "codex app-server exited: fatal: boom");
    assert_eq!(stats.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pool_failure_reports_child_exit_on_closed_pipe_write() {
    let stats = Arc::new(DialStats::default());
    let crash = Arc::new(Notify::new());
    let dialer = fake_dialer_with_tail(
        vec![handshake_script(crash, "fake-codex")],
        stats.clone(),
        Some("fatal: boom"),
    );
    let pool = Arc::new(Pool::new(dialer));
    let conn = pool.get(&ctx()).await.expect("acquire");

    // A write to a closed stdin can fail before the reader observes EOF.
    conn.client.close_writer().await;
    let write = conn
        .client
        .notify("after-close", None)
        .await
        .expect_err("write after close");
    assert!(write.is_closed_pipe(), "post-close write error = {write}");

    let err = conn.failure(write).await;
    assert_eq!(err.to_string(), "codex app-server exited: fatal: boom");
    assert_eq!(stats.closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pool_initialize_failure_closes_connection() {
    let stats = Arc::new(DialStats::default());
    let dialer = fake_dialer(
        vec![Box::new(|mut peer, stats| {
            Box::pin(async move {
                let (message, _) = peer.recv().await;
                stats.received.lock().unwrap().push(message.clone());
                peer.send(error_response(message.id, -32000, "initialize failed"))
                    .await;
                std::future::pending::<()>().await;
            })
        })],
        stats.clone(),
    );
    let pool = Arc::new(Pool::new(dialer));

    let err = pool.get(&ctx()).await.expect_err("initialize failure");
    assert_eq!(err.to_string(), "initialize failed");
    assert_eq!(stats.closed.load(Ordering::SeqCst), 1);
    let received = stats.received.lock().unwrap();
    assert_eq!(
        received.len(),
        1,
        "initialized must not be sent after failure"
    );
}

#[tokio::test(start_paused = true)]
async fn pool_initialize_timeout() {
    let stats = Arc::new(DialStats::default());
    let dialer = fake_dialer(
        vec![Box::new(|mut peer, stats| {
            Box::pin(async move {
                let (message, _) = peer.recv().await;
                stats.received.lock().unwrap().push(message);
                std::future::pending::<()>().await;
            })
        })],
        stats.clone(),
    );
    let pool = Arc::new(Pool::new(dialer));
    let begin = tokio::time::Instant::now();
    let err = pool.get(&ctx()).await.expect_err("initialize timeout");
    assert!(err.is_deadline_exceeded(), "timeout error = {err}");
    assert_eq!(tokio::time::Instant::now() - begin, Duration::from_secs(30));
    assert_eq!(stats.closed.load(Ordering::SeqCst), 1);
}

static SHUTDOWN_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn shutdown_closes_pooled_child() {
    let _guard = SHUTDOWN_TEST_LOCK.lock().await;
    let command = "codex-shutdown-test-13a";
    let stats = Arc::new(DialStats::default());
    let crash = Arc::new(Notify::new());
    let dialer = fake_dialer(vec![handshake_script(crash, "fake-codex")], stats);
    let pool = Arc::new(Pool::new(dialer));
    let conn = pool.get(&ctx()).await.expect("acquire");
    assert!(pool.current().is_some());
    pool::install_pool(command, pool.clone());

    pool::shutdown();
    assert!(
        pool.current().is_none(),
        "shutdown left the child connected"
    );
    tokio::time::timeout(Duration::from_secs(2), conn.client.done().cancelled())
        .await
        .expect("shutdown client done");
    pool::remove_pool(command);
}

#[tokio::test]
async fn shutdown_leaves_idle_pool_alone() {
    let _guard = SHUTDOWN_TEST_LOCK.lock().await;
    let command = "codex-idle-shutdown-test-13a";
    let stats = Arc::new(DialStats::default());
    let dialer = fake_dialer(Vec::new(), stats.clone());
    let pool = Arc::new(Pool::new(dialer));
    pool::install_pool(command, pool.clone());

    pool::shutdown();
    assert!(pool.current().is_none());
    assert_eq!(stats.dials.load(Ordering::SeqCst), 0);
    pool::remove_pool(command);
}

#[tokio::test]
async fn pool_for_command_is_shared() {
    let same = Pool::for_command("codex-shared-test");
    let same_again = Pool::for_command("codex-shared-test");
    let other = Pool::for_command("codex-other-test");
    assert!(Arc::ptr_eq(&same, &same_again));
    assert!(!Arc::ptr_eq(&same, &other));
    assert!(Arc::ptr_eq(
        &Pool::for_command(""),
        &Pool::for_command("codex")
    ));
}

#[tokio::test]
async fn adapter_name_pool_and_unsupported_turn() {
    let cfg = provider_config("codex-adapter-test-13a");
    let first = CodexAdapter::new("my-codex", &cfg).expect("adapter");
    let second = CodexAdapter::new("other", &cfg).expect("adapter");
    assert_eq!(first.name(), "my-codex");
    assert_eq!(Provider::name(&first), "my-codex");
    assert!(Arc::ptr_eq(first.pool(), second.pool()));
    assert_eq!(
        first.config().command.as_deref(),
        Some("codex-adapter-test-13a")
    );

    let err = first
        .turn(ctx(), TurnRequest::default(), Box::new(|_delta| {}))
        .await
        .expect_err("turn");
    assert_eq!(
        err.message,
        "codex-app-server runs whole threads; Turn is not supported"
    );
}
