//! Line-delimited JSON-RPC 2.0 peer used by the ACP provider.
//!
//! The far end is scripted over `tokio::io::duplex`, so every assertion is
//! about bytes on the wire.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};
use tokio_util::sync::CancellationToken;

use subagent_mcp::provider::acp::rpc::{Inbound, Peer};
use subagent_mcp::util::run::RunContext;

struct Agent {
    lines: Lines<BufReader<DuplexStream>>,
    out: DuplexStream,
}

impl Agent {
    async fn recv(&mut self) -> Value {
        let line = self.lines.next_line().await.unwrap().expect("line");
        serde_json::from_str(&line).expect("json line")
    }
    async fn send_raw(&mut self, s: &str) {
        self.out.write_all(s.as_bytes()).await.unwrap();
        self.out.flush().await.unwrap();
    }
    async fn send(&mut self, v: Value) {
        self.send_raw(&format!("{v}\n")).await;
    }
}

fn pair() -> (Peer, tokio::sync::mpsc::Receiver<Inbound>, Agent) {
    let (client_read, agent_write) = tokio::io::duplex(64 * 1024);
    let (client_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (peer, rx) = Peer::spawn(client_read, client_write);
    let agent = Agent {
        lines: BufReader::new(agent_read).lines(),
        out: agent_write,
    };
    (peer, rx, agent)
}

fn ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

#[tokio::test]
async fn call_matches_response_by_id() {
    let (peer, _rx, mut agent) = pair();
    let c = ctx();
    let p1 = peer.clone();
    let c1 = c.clone();
    let a = tokio::spawn(async move { p1.call(&c1, "a", json!({"n":1})).await });
    let first = agent.recv().await;
    let p2 = peer.clone();
    let c2 = c.clone();
    let b = tokio::spawn(async move { p2.call(&c2, "b", json!({"n":2})).await });
    let second = agent.recv().await;
    assert_eq!(first["jsonrpc"], "2.0");
    assert_eq!(first["method"], "a");
    assert_eq!(second["method"], "b");
    assert_ne!(first["id"], second["id"]);
    agent
        .send(json!({"jsonrpc":"2.0","id":second["id"],"result":"B"}))
        .await;
    agent
        .send(json!({"jsonrpc":"2.0","id":first["id"],"result":"A"}))
        .await;
    assert_eq!(a.await.unwrap().unwrap(), json!("A"));
    assert_eq!(b.await.unwrap().unwrap(), json!("B"));
}

#[tokio::test]
async fn interleaved_inbound_delivered() {
    let (peer, mut rx, mut agent) = pair();
    let p = peer.clone();
    let call = tokio::spawn(async move { p.call(&ctx(), "m", json!({})).await });
    let req = agent.recv().await;
    agent
        .send(json!({"jsonrpc":"2.0","method":"note","params":{"x":1}}))
        .await;
    agent
        .send(json!({"jsonrpc":"2.0","id":7,"method":"ask","params":{"y":2}}))
        .await;
    agent
        .send(json!({"jsonrpc":"2.0","id":req["id"],"result":{}}))
        .await;
    assert_eq!(call.await.unwrap().unwrap(), json!({}));
    match rx.recv().await.unwrap() {
        Inbound::Notification { method, params } => {
            assert_eq!(method, "note");
            assert_eq!(params, json!({"x":1}));
        }
        other => panic!("expected notification, got {other:?}"),
    }
    match rx.recv().await.unwrap() {
        Inbound::Request { id, method, params } => {
            assert_eq!(id, json!(7));
            assert_eq!(method, "ask");
            assert_eq!(params, json!({"y":2}));
        }
        other => panic!("expected request, got {other:?}"),
    }
    // Responding to an inbound request writes a well-formed reply.
    peer.respond(json!(7), Ok(json!({"ok":true})))
        .await
        .unwrap();
    let reply = agent.recv().await;
    assert_eq!(reply, json!({"jsonrpc":"2.0","id":7,"result":{"ok":true}}));
}

#[tokio::test]
async fn notify_and_error_respond_on_wire() {
    let (peer, _rx, mut agent) = pair();
    peer.notify("session/cancel", json!({"sessionId":"s"}))
        .await
        .unwrap();
    let n = agent.recv().await;
    assert_eq!(
        n,
        json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}})
    );
    peer.respond(
        json!("r1"),
        Err(subagent_mcp::provider::acp::rpc::RpcError {
            code: -32601,
            message: "method not found".into(),
            data: None,
        }),
    )
    .await
    .unwrap();
    let e = agent.recv().await;
    assert_eq!(e["id"], "r1");
    assert_eq!(e["error"]["code"], -32601);
    assert_eq!(e["error"]["message"], "method not found");
    assert!(e.get("result").is_none());
}

#[tokio::test]
async fn non_json_lines_skipped() {
    let (peer, _rx, mut agent) = pair();
    let p = peer.clone();
    let call = tokio::spawn(async move { p.call(&ctx(), "m", json!({})).await });
    let req = agent.recv().await;
    agent.send_raw("hello banner\n\n").await;
    agent
        .send(json!({"jsonrpc":"2.0","id":req["id"],"result":1}))
        .await;
    assert_eq!(call.await.unwrap().unwrap(), json!(1));
    assert!(!peer.closed());
}

#[tokio::test]
async fn error_response_maps_to_rpc_error() {
    let (peer, _rx, mut agent) = pair();
    let p = peer.clone();
    let call = tokio::spawn(async move { p.call(&ctx(), "m", json!({})).await });
    let req = agent.recv().await;
    agent
        .send(json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32000,"message":"auth","data":{"k":1}}}))
        .await;
    let err = call.await.unwrap().unwrap_err();
    assert_eq!(err.code, -32000);
    assert_eq!(err.message, "auth");
    assert_eq!(err.data, Some(json!({"k":1})));
}

#[tokio::test]
async fn eof_fails_pending() {
    let (peer, _rx, mut agent) = pair();
    let p = peer.clone();
    let call = tokio::spawn(async move { p.call(&ctx(), "m", json!({})).await });
    let _ = agent.recv().await;
    drop(agent);
    let err = tokio::time::timeout(Duration::from_secs(1), call)
        .await
        .expect("pending call fails on EOF")
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code, -32099);
    assert_eq!(err.message, "connection closed");
    assert!(peer.closed());
    // Calls after EOF fail immediately too.
    let err = tokio::time::timeout(Duration::from_secs(1), peer.call(&ctx(), "m", json!({})))
        .await
        .expect("no hang after EOF")
        .unwrap_err();
    assert_eq!(err.code, -32099);
}

#[tokio::test]
async fn cancelled_ctx_returns_promptly() {
    let (peer, _rx, mut agent) = pair();
    let c = ctx();
    let c2 = c.clone();
    let p = peer.clone();
    let call = tokio::spawn(async move { p.call(&c2, "m", json!({})).await });
    let _ = agent.recv().await;
    c.cancel.cancel();
    let err = tokio::time::timeout(Duration::from_secs(1), call)
        .await
        .expect("cancel returns within 1 s")
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code, -32098);
    assert_eq!(err.message, "cancelled");
}

#[tokio::test]
async fn passed_deadline_returns_deadline_reason() {
    let (peer, _rx, mut agent) = pair();
    let c = RunContext {
        cancel: CancellationToken::new(),
        deadline: Some(tokio::time::Instant::now() + Duration::from_millis(50)),
    };
    let p = peer.clone();
    let c2 = c.clone();
    let call = tokio::spawn(async move { p.call(&c2, "m", json!({})).await });
    // The deadline may pass before the frame is written; never hang here.
    let _ = tokio::time::timeout(Duration::from_secs(1), agent.recv()).await;
    let err = tokio::time::timeout(Duration::from_secs(1), call)
        .await
        .expect("deadline returns within 1 s")
        .unwrap()
        .unwrap_err();
    assert_ne!(err.code, -32098);
    assert_eq!(Some(err.message), c.deadline_reason());
}

#[tokio::test]
async fn cancel_mid_write_keeps_frames_whole() {
    // A tiny pipe forces the large request frame to block mid-write.
    let (client_read, _agent_write) = tokio::io::duplex(64 * 1024);
    let (client_write, agent_read) = tokio::io::duplex(64);
    let (peer, _rx) = Peer::spawn(client_read, client_write);
    let c = ctx();
    let c2 = c.clone();
    let p = peer.clone();
    let big = "x".repeat(16 * 1024);
    let call = tokio::spawn(async move { p.call(&c2, "big", json!({"blob": big})).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    c.cancel.cancel();
    let err = tokio::time::timeout(Duration::from_secs(1), call)
        .await
        .expect("cancel returns within 1 s")
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code, -32098);
    let p = peer.clone();
    let notify =
        tokio::spawn(async move { p.notify("session/cancel", json!({"sessionId":"s"})).await });
    let mut lines = BufReader::new(agent_read).lines();
    let first: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .expect("request frame is whole");
    assert_eq!(first["method"], "big");
    let second: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .expect("notify frame is whole");
    assert_eq!(second["method"], "session/cancel");
    notify.await.unwrap().unwrap();
}

#[tokio::test]
async fn non_utf8_lines_skipped() {
    let (peer, _rx, mut agent) = pair();
    let p = peer.clone();
    let call = tokio::spawn(async move { p.call(&ctx(), "m", json!({})).await });
    let req = agent.recv().await;
    agent.out.write_all(b"\xff\xfe\n").await.unwrap();
    agent
        .send(json!({"jsonrpc":"2.0","id":req["id"],"result":2}))
        .await;
    let res = tokio::time::timeout(Duration::from_secs(1), call)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(res.unwrap(), json!(2));
    assert!(!peer.closed());
}
