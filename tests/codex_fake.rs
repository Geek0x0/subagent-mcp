//! Codex thread lifecycle, approvals, events, and adapter capabilities.
//!
//! Ports the thread/event half of Go `internal/provider/codexappserver`
//! (`thread_test.go`, `thread_stray_test.go`, `adapter_test.go`, and
//! `events_test.go`). The scripted peer is an in-memory JSON-RPC app-server, so
//! every assertion is about the wire protocol the real child would see. The
//! bounded-cleanup test spawns the fixture app-server script (Unix only).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf, split,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use subagent_mcp::config::{Api, ProviderConfig};
use subagent_mcp::policy::{ApprovalPolicy, Sandbox};
use subagent_mcp::provider::codex::client::{Client, ClientError, Message, RpcError};
use subagent_mcp::provider::codex::pool::{self, Dialed, Pool};
use subagent_mcp::provider::codex::thread::{self, CodexThread};
use subagent_mcp::provider::codex::{CodexAdapter, events};
use subagent_mcp::provider::{
    Agent, AuthChecker, AuthStatus, ModelLister, Provider, Thread, ThreadCallbacks, ThreadOptions,
};
use subagent_mcp::util::approval::{ApprovalRequest, Approver};
use subagent_mcp::util::run::RunContext;

const PEER_BUF: usize = 1 << 20;
const EXACT_UNSUPPORTED: &str = "not supported by subagent-mcp";
/// The codex-cli version the scripted fixtures emulate.
const PINNED_CODEX_VERSION: &str = include_str!("fixtures/codex/version.txt");

fn pinned_version() -> &'static str {
    PINNED_CODEX_VERSION.trim()
}

// ---------------------------------------------------------------------------
// Scripted app-server peer
// ---------------------------------------------------------------------------

type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Handler = Arc<dyn Fn(FakeCodex, Value) -> BoxFut<Result<Value, String>> + Send + Sync>;
type Script = Arc<dyn Fn(FakeCodex) -> BoxFut<()> + Send + Sync>;

struct FakeInner {
    writer: tokio::sync::Mutex<Option<WriteHalf<DuplexStream>>>,
    received: Mutex<HashMap<String, Vec<Value>>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Message>>>,
    handlers: Mutex<HashMap<String, Handler>>,
    next_id: AtomicU64,
    done: CancellationToken,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    client: std::sync::OnceLock<Arc<Client>>,
}

/// A scriptable in-memory Codex app-server.
#[derive(Clone)]
struct FakeCodex {
    inner: Arc<FakeInner>,
}

impl FakeCodex {
    fn new() -> Self {
        let (client_side, server_side) = tokio::io::duplex(PEER_BUF);
        let (client_read, client_write) = split(client_side);
        let (server_read, server_write) = split(server_side);
        let client = Client::new(client_read, client_write);
        let inner = Arc::new(FakeInner {
            writer: tokio::sync::Mutex::new(Some(server_write)),
            received: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            handlers: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            done: CancellationToken::new(),
            reader: Mutex::new(None),
            client: std::sync::OnceLock::new(),
        });
        let _ = inner.client.set(client);
        let fake = Self {
            inner: inner.clone(),
        };
        fake.install_defaults();
        let handle = tokio::spawn(read_loop(inner, server_read));
        *fake.inner.reader.lock().unwrap() = Some(handle);
        fake
    }

    fn install_defaults(&self) {
        let user_agent = format!("subagent-mcp/{} (fake)", pinned_version());
        self.handle("initialize", move |_fake, _params| {
            let user_agent = user_agent.clone();
            async move { Ok::<Value, String>(json!({"userAgent": user_agent})) }
        });
        self.handle("thread/start", |_fake, _params| async move {
            Ok::<Value, String>(json!({"thread": {"id": "thr-1"}}))
        });
        self.handle("thread/unsubscribe", |_fake, _params| async move {
            Ok::<Value, String>(json!({}))
        });
        self.handle("turn/interrupt", |_fake, _params| async move {
            Ok::<Value, String>(json!({}))
        });
    }

    fn handle<F, Fut>(&self, method: &str, handler: F)
    where
        F: Fn(FakeCodex, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |fake, params| Box::pin(handler(fake, params)));
        self.inner
            .handlers
            .lock()
            .unwrap()
            .insert(method.to_string(), handler);
    }

    fn client(&self) -> Arc<Client> {
        self.inner.client.get().unwrap().clone()
    }

    fn dialed(&self) -> Dialed {
        Dialed {
            client: self.client(),
            closer: Arc::new(FakeCloser { fake: self.clone() }),
            stderr_tail: Some(Arc::new(|| "boom".to_string())),
        }
    }

    async fn send(&self, message: Message) {
        if self.inner.done.is_cancelled() {
            return;
        }
        let mut writer = self.inner.writer.lock().await;
        let Some(writer) = writer.as_mut() else {
            return;
        };
        let mut line = serde_json::to_string(&message).expect("encode fake message");
        line.push('\n');
        let _ = writer.write_all(line.as_bytes()).await;
        let _ = writer.flush().await;
    }

    async fn notify(&self, method: &str, params: Value) {
        self.send(Message {
            method: Some(method.to_string()),
            params: Some(to_raw(&params)),
            ..Message::default()
        })
        .await;
    }

    /// Send a server-to-client request and wait for the whole reply object.
    async fn request(&self, method: &str, params: Value) -> Option<Message> {
        let number = self.inner.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("server-{number}");
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(id.clone(), sender);
        self.send(Message {
            id: Some(RawValue::from_string(json!(id).to_string()).unwrap()),
            method: Some(method.to_string()),
            params: Some(to_raw(&params)),
            ..Message::default()
        })
        .await;
        tokio::select! {
            reply = receiver => reply.ok(),
            _ = self.inner.done.cancelled() => None,
        }
    }

    fn received(&self, method: &str) -> Vec<Value> {
        self.inner
            .received
            .lock()
            .unwrap()
            .get(method)
            .cloned()
            .unwrap_or_default()
    }

    fn assert_params(&self, method: &str, want: Value) {
        let got = self.received(method);
        assert_eq!(got.len(), 1, "Received({method}) = {got:?}, want one call");
        assert_eq!(got[0], want, "params for {method}");
    }

    fn assert_no(&self, method: &str) {
        assert!(
            self.received(method).is_empty(),
            "unexpected {method}: {:?}",
            self.received(method)
        );
    }

    fn crash(&self) {
        self.inner.done.cancel();
        let inner = self.inner.clone();
        tokio::spawn(async move {
            inner.writer.lock().await.take();
        });
        if let Ok(mut reader) = self.inner.reader.lock()
            && let Some(handle) = reader.take()
        {
            handle.abort();
        }
    }
}

struct FakeCloser {
    fake: FakeCodex,
}

#[async_trait]
impl pool::Closer for FakeCloser {
    async fn close(&self) {
        self.fake.crash();
    }

    fn close_blocking(&self) {
        self.fake.crash();
    }
}

async fn read_loop(inner: Arc<FakeInner>, reader: ReadHalf<DuplexStream>) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let message: Message = match serde_json::from_str(&line) {
                    Ok(message) => message,
                    Err(_) => continue,
                };
                dispatch(&inner, message);
            }
            _ => {
                inner.done.cancel();
                inner.writer.lock().await.take();
                return;
            }
        }
    }
}

fn dispatch(inner: &Arc<FakeInner>, message: Message) {
    let Some(method) = message.method.clone() else {
        // Server-request ids are strings; pending keys are stored unquoted.
        let id = raw_id(&message).trim_matches('"').to_string();
        if let Some(sender) = inner.pending.lock().unwrap().remove(&id) {
            let _ = sender.send(message);
        }
        return;
    };
    let params = message
        .params
        .as_ref()
        .and_then(|params| serde_json::from_str(params.get()).ok())
        .unwrap_or(Value::Null);
    inner
        .received
        .lock()
        .unwrap()
        .entry(method.clone())
        .or_default()
        .push(params.clone());
    let handler = inner.handlers.lock().unwrap().get(&method).cloned();
    let fake = FakeCodex {
        inner: inner.clone(),
    };
    let Some(handler) = handler else {
        if message.id.is_some() {
            tokio::spawn(async move {
                fake.respond_error(message.id.unwrap(), -32601, "method not found")
                    .await;
            });
        }
        return;
    };
    tokio::spawn(async move {
        let result = handler(fake.clone(), params).await;
        let Some(id) = message.id else {
            return;
        };
        match result {
            Ok(result) => fake.respond(id, result).await,
            Err(error) => fake.respond_error(id, -32000, &error).await,
        }
    });
}

impl FakeCodex {
    async fn respond(&self, id: Box<RawValue>, result: Value) {
        self.send(Message {
            id: Some(id),
            result: Some(to_raw(&result)),
            ..Message::default()
        })
        .await;
    }

    async fn respond_error(&self, id: Box<RawValue>, code: i64, message: &str) {
        self.send(Message {
            id: Some(id),
            error: Some(RpcError {
                code,
                message: message.to_string(),
            }),
            ..Message::default()
        })
        .await;
    }
}

fn raw_id(message: &Message) -> &str {
    message.id.as_ref().map(|id| id.get()).unwrap_or("")
}

fn to_raw(value: &Value) -> Box<RawValue> {
    serde_json::value::to_raw_value(value).unwrap()
}

async fn crash_and_wait(fake: &FakeCodex) {
    fake.crash();
    let _ = tokio::time::timeout(Duration::from_secs(2), fake.client().done().cancelled()).await;
}

fn fake_pool(fakes: &[FakeCodex]) -> Arc<Pool> {
    let queue: Arc<Mutex<VecDeque<FakeCodex>>> =
        Arc::new(Mutex::new(fakes.iter().cloned().collect()));
    Arc::new(Pool::new(Arc::new(move || {
        let fake = queue.lock().unwrap().pop_front();
        Box::pin(async move {
            let fake = fake.ok_or_else(|| ClientError::message("unexpected dial"))?;
            Ok(fake.dialed())
        })
    })))
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

fn options() -> ThreadOptions {
    ThreadOptions {
        model: String::new(),
        cwd: PathBuf::new(),
        sandbox: Sandbox::ReadOnly,
        approval_policy: ApprovalPolicy::Never,
        base_instructions: None,
        developer_instructions: None,
        writable_roots: Vec::new(),
        ephemeral: false,
    }
}

async fn start(pool: &Arc<Pool>, opts: ThreadOptions) -> CodexThread {
    thread::start_thread(&ctx(), pool.clone(), opts)
        .await
        .expect("start thread")
}

fn script<F, Fut>(script: F) -> Script
where
    F: Fn(FakeCodex) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    Arc::new(move |fake| Box::pin(script(fake)))
}

fn script_turn(fake: &FakeCodex, thread_id: &str, turn_id: &str, body: Script) {
    let thread_id = thread_id.to_string();
    let turn_id = turn_id.to_string();
    fake.handle("turn/start", move |fake, _params| {
        let thread_id = thread_id.clone();
        let turn_id = turn_id.clone();
        let body = body.clone();
        async move {
            // Notifications preceding the response must not be lost.
            fake.notify(
                "turn/started",
                json!({"threadId": thread_id, "turn": {"id": turn_id}}),
            )
            .await;
            let scripted = fake.clone();
            tokio::spawn(async move { body(scripted).await });
            Ok::<Value, String>(json!({"turn": {"id": turn_id}}))
        }
    });
}

async fn agent_message(fake: &FakeCodex, thread_id: &str, turn_id: &str, phase: &str, text: &str) {
    fake.notify(
        "item/completed",
        json!({
            "threadId": thread_id,
            "turnId": turn_id,
            "item": {"type": "agentMessage", "phase": phase, "text": text},
        }),
    )
    .await;
}

async fn complete_turn(fake: &FakeCodex, thread_id: &str, turn_id: &str, status: &str) {
    fake.notify(
        "turn/completed",
        json!({"threadId": thread_id, "turn": {"id": turn_id, "status": status}}),
    )
    .await;
}

type Emit = Arc<dyn Fn(&str, Value) + Send + Sync>;

fn emit_sink(events: Arc<Mutex<Vec<Value>>>) -> Emit {
    Arc::new(move |kind, event| {
        assert_eq!(Some(kind), event.get("type").and_then(Value::as_str));
        events.lock().unwrap().push(event);
    })
}

fn no_emit() -> Emit {
    Arc::new(|_, _| {})
}

fn event_types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|event| event["type"].as_str().unwrap_or("").to_string())
        .collect()
}

struct Always(bool);

#[async_trait]
impl Approver for Always {
    async fn approve(&self, _ctx: &RunContext, _req: ApprovalRequest) -> bool {
        self.0
    }
}

struct Recording {
    decision: bool,
    got: Arc<Mutex<Vec<ApprovalRequest>>>,
}

#[async_trait]
impl Approver for Recording {
    async fn approve(&self, _ctx: &RunContext, req: ApprovalRequest) -> bool {
        self.got.lock().unwrap().push(req);
        self.decision
    }
}

/// Blocks until its context is cancelled, recording that it observed it.
struct Blocking {
    entered: mpsc::UnboundedSender<()>,
    cancelled: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl Approver for Blocking {
    async fn approve(&self, ctx: &RunContext, _req: ApprovalRequest) -> bool {
        let _ = self.entered.send(());
        match ctx.deadline {
            Some(deadline) => {
                tokio::select! {
                    _ = ctx.cancel.cancelled() => {}
                    _ = tokio::time::sleep_until(deadline) => {}
                }
            }
            None => ctx.cancel.cancelled().await,
        }
        let _ = self.cancelled.send(());
        false
    }
}

fn decision(reply: &Message) -> Value {
    let result = reply.result.as_ref().expect("approval reply result");
    serde_json::from_str(result.get()).expect("decode approval result")
}

fn assert_rejected(reply: &Message) {
    let error = reply.error.as_ref().expect("unsupported reply error");
    assert_eq!(error.code, -32601);
    assert_eq!(error.message, EXACT_UNSUPPORTED);
    assert!(reply.id.is_some());
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

async fn with_timeout<F, T>(future: F) -> T
where
    F: Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .expect("operation timed out")
}

// ---------------------------------------------------------------------------
// Event mapping
// ---------------------------------------------------------------------------

#[test]
fn map_notification_matches_go_table() {
    for (name, method, params, want) in [
        (
            "started",
            "turn/started",
            json!({"turn": {"id": "t"}}),
            Some(json!({"type": "task_started"})),
        ),
        (
            "delta",
            "item/agentMessage/delta",
            json!({"delta": "hi"}),
            Some(json!({"type": "agent_message_delta", "delta": "hi"})),
        ),
        (
            "message",
            "item/completed",
            json!({"item": {"type": "agentMessage", "text": "42", "phase": "final_answer"}}),
            Some(json!({"type": "agent_message", "message": "42"})),
        ),
        (
            "command begin",
            "item/started",
            json!({"item": {"id": "i", "type": "commandExecution", "command": "pwd"}}),
            Some(
                json!({"type": "exec_command_begin", "call_id": "i", "tool": "shell", "command": "pwd"}),
            ),
        ),
        (
            "command end",
            "item/completed",
            json!({"item": {"id": "i", "type": "commandExecution", "exitCode": 7}}),
            Some(
                json!({"type": "exec_command_end", "call_id": "i", "tool": "shell", "exit_code": 7}),
            ),
        ),
        (
            "command no exit code",
            "item/completed",
            json!({"item": {"id": "i", "type": "commandExecution", "exitCode": null}}),
            Some(json!({"type": "exec_command_end", "call_id": "i", "tool": "shell"})),
        ),
        (
            "patch begin",
            "item/started",
            json!({"item": {"id": "i", "type": "fileChange", "changes": [{"path": "/a"}, {"path": "/b"}]}}),
            Some(
                json!({"type": "exec_command_begin", "call_id": "i", "tool": "apply_patch", "paths": ["/a", "/b"]}),
            ),
        ),
        (
            "patch end",
            "item/completed",
            json!({"item": {"id": "i", "type": "fileChange", "changes": [{"path": "/a"}]}}),
            Some(
                json!({"type": "exec_command_end", "call_id": "i", "tool": "apply_patch", "paths": ["/a"]}),
            ),
        ),
        (
            "usage from last",
            "thread/tokenUsage/updated",
            json!({"tokenUsage": {"last": {"inputTokens": 10, "outputTokens": 3, "totalTokens": 13}, "total": {"inputTokens": 100, "outputTokens": 30, "totalTokens": 130}}}),
            Some(
                json!({"type": "token_count", "prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}),
            ),
        ),
        (
            "error",
            "error",
            json!({"error": {"message": "quota"}, "willRetry": false}),
            Some(json!({"type": "error", "message": "quota"})),
        ),
        (
            "completed",
            "turn/completed",
            json!({"turn": {"status": "completed"}}),
            Some(json!({"type": "task_complete"})),
        ),
        (
            "failed",
            "turn/completed",
            json!({"turn": {"status": "failed"}}),
            None,
        ),
        (
            "interrupted",
            "turn/completed",
            json!({"turn": {"status": "interrupted"}}),
            None,
        ),
        (
            "agent begin",
            "item/started",
            json!({"item": {"type": "agentMessage"}}),
            None,
        ),
        (
            "reasoning begin",
            "item/started",
            json!({"item": {"type": "reasoning"}}),
            None,
        ),
        (
            "reasoning end",
            "item/completed",
            json!({"item": {"type": "reasoning"}}),
            None,
        ),
        (
            "unknown item",
            "item/completed",
            json!({"item": {"type": "futureItem"}}),
            None,
        ),
        ("global", "mcpServer/startupStatus/updated", json!({}), None),
        (
            "retryable error",
            "error",
            json!({"error": {"message": "retrying"}, "willRetry": true}),
            None,
        ),
        ("unknown method", "future/event", json!({}), None),
        ("malformed", "item/completed", json!("not an object"), None),
    ] {
        let got = events::map_notification(method, &params);
        assert_eq!(got, want, "case {name}");
    }
}

// ---------------------------------------------------------------------------
// Thread start and turns
// ---------------------------------------------------------------------------

#[tokio::test]
async fn start_thread_params() {
    for instructions in [false, true] {
        let fake = FakeCodex::new();
        let pool = fake_pool(std::slice::from_ref(&fake));
        let mut opts = options();
        opts.cwd = PathBuf::from("/work");
        opts.model = "model".to_string();
        opts.sandbox = Sandbox::ReadOnly;
        opts.approval_policy = ApprovalPolicy::OnFailure;
        opts.ephemeral = true;
        let mut want = json!({
            "cwd": "/work",
            "model": "model",
            "sandbox": "read-only",
            "approvalPolicy": "on-request",
            "ephemeral": true,
        });
        if instructions {
            opts.base_instructions = Some("base".to_string());
            opts.developer_instructions = Some("developer".to_string());
            want["baseInstructions"] = json!("base");
            want["developerInstructions"] = json!("developer");
        }
        let thread = start(&pool, opts).await;
        fake.assert_params("thread/start", want);
        thread.close().await.unwrap();
        pool.close().await;
    }
}

#[tokio::test]
async fn run_final_answer_by_phase() {
    for (name, messages, want) in [
        (
            "final answer",
            vec![("commentary", "working"), ("final_answer", "42")],
            "42",
        ),
        (
            "last final wins",
            vec![
                ("final_answer", "41"),
                ("final_answer", "42"),
                ("commentary", "done"),
            ],
            "42",
        ),
        (
            "fallback",
            vec![("commentary", "first"), ("", "last")],
            "last",
        ),
        (
            "empty final wins",
            vec![("final_answer", ""), ("commentary", "done")],
            "",
        ),
        ("no messages", vec![], ""),
    ] {
        let fake = FakeCodex::new();
        let pool = fake_pool(std::slice::from_ref(&fake));
        let body = script(move |fake| {
            let messages = messages.clone();
            async move {
                for (phase, text) in messages {
                    agent_message(&fake, "thr-1", "turn-1", phase, text).await;
                }
                complete_turn(&fake, "thr-1", "turn-1", "completed").await;
            }
        });
        script_turn(&fake, "thr-1", "turn-1", body);
        let thread = start(&pool, options()).await;
        let events = Arc::new(Mutex::new(Vec::new()));
        let text = with_timeout(thread.run(
            ctx(),
            "question".to_string(),
            Some("high".to_string()),
            ThreadCallbacks {
                emit: emit_sink(events.clone()),
                approve: Arc::new(Always(true)),
            },
        ))
        .await
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(text, want, "{name}");
        fake.assert_params(
            "turn/start",
            json!({
                "threadId": "thr-1",
                "input": [{"type": "text", "text": "question"}],
                "effort": "high",
            }),
        );
        thread.close().await.unwrap();
        pool.close().await;
    }
}

#[tokio::test]
async fn run_writable_roots() {
    for (sandbox, roots) in [
        (Sandbox::WorkspaceWrite, vec!["/a", "/b"]),
        (Sandbox::WorkspaceWrite, vec![]),
        (Sandbox::ReadOnly, vec!["/a"]),
        (Sandbox::DangerFullAccess, vec!["/a"]),
    ] {
        let fake = FakeCodex::new();
        let pool = fake_pool(std::slice::from_ref(&fake));
        script_turn(
            &fake,
            "thr-1",
            "turn-1",
            script(|fake| async move {
                complete_turn(&fake, "thr-1", "turn-1", "completed").await;
            }),
        );
        let mut opts = options();
        opts.sandbox = sandbox;
        opts.writable_roots = roots.iter().map(PathBuf::from).collect();
        let thread = start(&pool, opts).await;
        with_timeout(thread.run(
            ctx(),
            "hi".to_string(),
            Some("low".to_string()),
            ThreadCallbacks {
                emit: no_emit(),
                approve: Arc::new(Always(true)),
            },
        ))
        .await
        .unwrap();
        let mut want = json!({
            "threadId": "thr-1",
            "input": [{"type": "text", "text": "hi"}],
            "effort": "low",
        });
        if sandbox == Sandbox::WorkspaceWrite && !roots.is_empty() {
            want["sandboxPolicy"] = json!({
                "type": "workspaceWrite",
                "writableRoots": roots,
                "networkAccess": false,
            });
        }
        fake.assert_params("turn/start", want);
        thread.close().await.unwrap();
        pool.close().await;
    }
}

#[tokio::test]
async fn run_deltas_and_events() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(|fake| async move {
            for delta in ["Hel", "lo"] {
                fake.notify(
                    "item/agentMessage/delta",
                    json!({"threadId": "thr-1", "turnId": "turn-1", "itemId": "i1", "delta": delta}),
                )
                .await;
            }
            fake.notify(
                "item/started",
                json!({"threadId": "thr-1", "turnId": "turn-1", "item": {"id": "c1", "type": "commandExecution", "command": "pwd"}}),
            )
            .await;
            fake.notify(
                "item/completed",
                json!({"threadId": "thr-1", "turnId": "turn-1", "item": {"id": "c1", "type": "commandExecution", "exitCode": 0}}),
            )
            .await;
            fake.notify(
                "thread/tokenUsage/updated",
                json!({"threadId": "thr-1", "tokenUsage": {"last": {"inputTokens": 2, "outputTokens": 1, "totalTokens": 3}}}),
            )
            .await;
            agent_message(&fake, "thr-1", "turn-1", "final_answer", "Hello").await;
            complete_turn(&fake, "thr-1", "turn-1", "completed").await;
        }),
    );
    let thread = start(&pool, options()).await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let text = with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: emit_sink(events.clone()),
            approve: Arc::new(Always(true)),
        },
    ))
    .await
    .unwrap();
    assert_eq!(text, "Hello");
    let events = events.lock().unwrap().clone();
    assert_eq!(
        event_types(&events),
        [
            "task_started",
            "agent_message_delta",
            "exec_command_begin",
            "exec_command_end",
            "token_count",
            "agent_message",
            "task_complete",
        ]
    );
    assert_eq!(events[1]["delta"], json!("Hello"));
    assert_eq!(events[3]["exit_code"], json!(0));
    thread.close().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn tolerance_cases_are_non_fatal() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(|fake| async move {
            fake.notify(
                "future/event",
                json!({"threadId": "thr-1", "turnId": "turn-1", "unknown": true}),
            )
            .await;
            fake.notify(
                "mcpServer/startupStatus/updated",
                json!({"status": "ready"}),
            )
            .await;
            fake.notify(
                "item/completed",
                json!({
                    "threadId": "thr-1",
                    "turnId": "turn-1",
                    "unknownField": {"nested": 1},
                    "item": {"id": "f1", "type": "futureItem", "payload": [1, 2]},
                }),
            )
            .await;
            fake.notify(
                "item/completed",
                json!({"threadId": "thr-1", "turnId": "turn-1", "item": "malformed"}),
            )
            .await;
            fake.notify(
                "error",
                json!({"threadId": "thr-1", "turnId": "turn-1", "willRetry": true, "error": {"message": "retry"}}),
            )
            .await;
            agent_message(&fake, "thr-1", "turn-1", "final_answer", "ok").await;
            complete_turn(&fake, "thr-1", "turn-1", "completed").await;
        }),
    );
    let thread = start(&pool, options()).await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let text = with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: emit_sink(events.clone()),
            approve: Arc::new(Always(true)),
        },
    ))
    .await
    .unwrap();
    assert_eq!(text, "ok");
    assert_eq!(
        event_types(&events.lock().unwrap()),
        ["task_started", "agent_message", "task_complete"]
    );
    thread.close().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn failed_turn_returns_error() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(|fake| async move {
            fake.notify(
                "turn/completed",
                json!({"threadId": "thr-1", "turn": {"id": "turn-1", "status": "failed", "error": {"message": "quota"}}}),
            )
            .await;
        }),
    );
    let thread = start(&pool, options()).await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let error = with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: emit_sink(events.clone()),
            approve: Arc::new(Always(true)),
        },
    ))
    .await
    .expect_err("failed turn");
    assert!(error.message.contains("quota"), "error = {error}");
    assert_eq!(event_types(&events.lock().unwrap()), ["task_started"]);
    thread.close().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn retryable_error_not_terminal() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(|fake| async move {
            fake.notify(
                "error",
                json!({"threadId": "thr-1", "turnId": "turn-1", "willRetry": true, "error": {"message": "retry"}}),
            )
            .await;
            fake.notify(
                "error",
                json!({"threadId": "thr-1", "turnId": "turn-1", "willRetry": false, "error": {"message": "informational"}}),
            )
            .await;
            agent_message(&fake, "thr-1", "turn-1", "final_answer", "ok").await;
            complete_turn(&fake, "thr-1", "turn-1", "completed").await;
        }),
    );
    let thread = start(&pool, options()).await;
    let messages = Arc::new(Mutex::new(Vec::new()));
    let sink = messages.clone();
    let text = with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: Arc::new(move |_kind, event| {
                if event["type"] == "error" {
                    sink.lock()
                        .unwrap()
                        .push(event["message"].as_str().unwrap().to_string());
                }
            }),
            approve: Arc::new(Always(true)),
        },
    ))
    .await
    .unwrap();
    assert_eq!(text, "ok");
    assert_eq!(*messages.lock().unwrap(), ["informational"]);
    thread.close().await.unwrap();
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Approvals and unsupported requests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_command_approval() {
    for accept in [true, false] {
        let fake = FakeCodex::new();
        let pool = fake_pool(std::slice::from_ref(&fake));
        let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
        script_turn(
            &fake,
            "thr-1",
            "turn-1",
            script(move |fake| {
                let reply_tx = reply_tx.clone();
                async move {
                    let reply = fake
                        .request(
                            "item/commandExecution/requestApproval",
                            json!({"threadId": "thr-1", "turnId": "turn-1", "command": "rm x", "reason": "cleanup"}),
                        )
                        .await
                        .expect("approval reply");
                    let _ = reply_tx.send(reply);
                    complete_turn(&fake, "thr-1", "turn-1", "completed").await;
                }
            }),
        );
        let thread = start(&pool, options()).await;
        let got = Arc::new(Mutex::new(Vec::new()));
        with_timeout(thread.run(
            ctx(),
            "hi".to_string(),
            None,
            ThreadCallbacks {
                emit: no_emit(),
                approve: Arc::new(Recording {
                    decision: accept,
                    got: got.clone(),
                }),
            },
        ))
        .await
        .unwrap();
        {
            let got = got.lock().unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].tool, "shell");
            assert_eq!(got[0].command.as_deref(), Some("rm x"));
            assert_eq!(got[0].reason, "cleanup");
            assert_eq!(got[0].path, None);
        }
        let reply = reply_rx.recv().await.expect("approval reply");
        assert_eq!(
            decision(&reply),
            json!({"decision": if accept { "accept" } else { "decline" }})
        );
        thread.close().await.unwrap();
        pool.close().await;
    }
}

#[tokio::test]
async fn run_network_approval_uses_host() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(move |fake| {
            let reply_tx = reply_tx.clone();
            async move {
                let reply = fake
                    .request(
                        "item/commandExecution/requestApproval",
                        json!({
                            "threadId": "thr-1",
                            "turnId": "turn-1",
                            "command": null,
                            "networkApprovalContext": {"host": "example.com", "protocol": "https"},
                        }),
                    )
                    .await
                    .expect("approval reply");
                let _ = reply_tx.send(reply);
                complete_turn(&fake, "thr-1", "turn-1", "completed").await;
            }
        }),
    );
    let thread = start(&pool, options()).await;
    let got = Arc::new(Mutex::new(Vec::new()));
    with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: no_emit(),
            approve: Arc::new(Recording {
                decision: true,
                got: got.clone(),
            }),
        },
    ))
    .await
    .unwrap();
    assert_eq!(
        got.lock().unwrap()[0].command.as_deref(),
        Some("example.com")
    );
    assert_eq!(
        decision(&reply_rx.recv().await.expect("approval reply")),
        json!({"decision": "accept"})
    );
    thread.close().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn run_file_change_approval_paths() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(move |fake| {
            let reply_tx = reply_tx.clone();
            async move {
                fake.notify(
                    "item/started",
                    json!({
                        "threadId": "thr-1",
                        "turnId": "turn-1",
                        "item": {"id": "i1", "type": "fileChange", "changes": [{"path": "/a"}, {"path": "/b"}]},
                    }),
                )
                .await;
                let reply = fake
                    .request(
                        "item/fileChange/requestApproval",
                        json!({"threadId": "thr-1", "turnId": "turn-1", "itemId": "i1", "reason": "edit"}),
                    )
                    .await
                    .expect("approval reply");
                let _ = reply_tx.send(reply);
                complete_turn(&fake, "thr-1", "turn-1", "completed").await;
            }
        }),
    );
    let thread = start(&pool, options()).await;
    let got = Arc::new(Mutex::new(Vec::new()));
    with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: no_emit(),
            approve: Arc::new(Recording {
                decision: true,
                got: got.clone(),
            }),
        },
    ))
    .await
    .unwrap();
    {
        let got = got.lock().unwrap();
        assert_eq!(got[0].tool, "apply_patch");
        assert_eq!(got[0].path.as_deref(), Some("/a, /b"));
        assert_eq!(got[0].command, None);
    }
    assert_eq!(
        decision(&reply_rx.recv().await.expect("approval reply")),
        json!({"decision": "accept"})
    );
    thread.close().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn unsupported_and_stale_requests_rejected() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(move |fake| {
            let reply_tx = reply_tx.clone();
            async move {
                for method in [
                    "item/permissions/requestApproval",
                    "item/tool/requestUserInput",
                    "mcpServer/elicitation/request",
                    "item/tool/call",
                    "account/chatgptAuthTokens/refresh",
                    "attestation/generate",
                    "applyPatchApproval",
                    "execCommandApproval",
                ] {
                    let reply = fake
                        .request(method, json!({"threadId": "thr-1", "turnId": "turn-1"}))
                        .await
                        .expect("unsupported reply");
                    let _ = reply_tx.send(reply);
                }
                for turn in ["old", ""] {
                    let reply = fake
                        .request(
                            "item/commandExecution/requestApproval",
                            json!({"threadId": "thr-1", "turnId": turn, "command": "wrong"}),
                        )
                        .await
                        .expect("stale reply");
                    let _ = reply_tx.send(reply);
                }
                agent_message(&fake, "thr-1", "old", "final_answer", "wrong").await;
                complete_turn(&fake, "thr-1", "old", "completed").await;
                agent_message(&fake, "thr-1", "turn-1", "final_answer", "ok").await;
                complete_turn(&fake, "thr-1", "turn-1", "completed").await;
            }
        }),
    );
    let thread = start(&pool, options()).await;
    let text = with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: no_emit(),
            approve: Arc::new(Always(true)),
        },
    ))
    .await
    .unwrap();
    assert_eq!(text, "ok");
    for _ in 0..10 {
        assert_rejected(&reply_rx.recv().await.expect("rejected reply"));
    }
    thread.close().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn requests_between_turns_rejected() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let thread = start(&pool, options()).await;

    let before = fake
        .request(
            "item/commandExecution/requestApproval",
            json!({"threadId": "thr-1", "turnId": "turn-1"}),
        )
        .await
        .expect("before reply");
    assert_rejected(&before);

    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(|fake| async move {
            complete_turn(&fake, "thr-1", "turn-1", "completed").await;
        }),
    );
    with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: no_emit(),
            approve: Arc::new(Always(true)),
        },
    ))
    .await
    .unwrap();

    let after = fake
        .request(
            "item/commandExecution/requestApproval",
            json!({"threadId": "thr-1", "turnId": "turn-1"}),
        )
        .await
        .expect("after reply");
    assert_rejected(&after);
    thread.close().await.unwrap();
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Interrupts and cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_interrupts_and_allows_another_turn() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    script_turn(&fake, "thr-1", "turn-1", script(|_fake| async move {}));
    fake.handle("turn/interrupt", |fake, _params| async move {
        complete_turn(&fake, "thr-1", "turn-1", "interrupted").await;
        Ok::<Value, String>(json!({}))
    });
    let thread = start(&pool, options()).await;
    let run_ctx = ctx();
    let cancel = run_ctx.cancel.clone();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let started_for_emit = started.clone();
    let run = tokio::spawn(async move {
        thread
            .run(
                run_ctx,
                "hi".to_string(),
                None,
                ThreadCallbacks {
                    emit: Arc::new(move |_kind, event| {
                        if event["type"] == "task_started" {
                            started_for_emit.notify_one();
                        }
                        sink.lock().unwrap().push(event);
                    }),
                    approve: Arc::new(Always(true)),
                },
            )
            .await
    });
    with_timeout(started.notified()).await;
    let begin = tokio::time::Instant::now();
    cancel.cancel();
    let error = with_timeout(run).await.unwrap().expect_err("cancelled run");
    assert_eq!(error.message, "context canceled");
    assert!(begin.elapsed() < Duration::from_secs(1));
    assert_eq!(event_types(&events.lock().unwrap()), ["task_started"]);
    fake.assert_params(
        "turn/interrupt",
        json!({"threadId": "thr-1", "turnId": "turn-1"}),
    );
    pool.close().await;
}

#[tokio::test]
async fn cancel_during_approval() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(move |fake| {
            let reply_tx = reply_tx.clone();
            async move {
                let reply = fake
                    .request(
                        "item/commandExecution/requestApproval",
                        json!({"threadId": "thr-1", "turnId": "turn-1", "command": "rm x"}),
                    )
                    .await
                    .expect("approval reply");
                let _ = reply_tx.send(reply);
            }
        }),
    );
    fake.handle("turn/interrupt", |fake, _params| async move {
        complete_turn(&fake, "thr-1", "turn-1", "interrupted").await;
        Ok::<Value, String>(json!({}))
    });
    let thread = start(&pool, options()).await;
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let run_ctx = ctx();
    let cancel = run_ctx.cancel.clone();
    let run = tokio::spawn(async move {
        thread
            .run(
                run_ctx,
                "hi".to_string(),
                None,
                ThreadCallbacks {
                    emit: no_emit(),
                    approve: Arc::new(Blocking {
                        entered: entered_tx,
                        cancelled: cancelled_tx,
                    }),
                },
            )
            .await
    });
    with_timeout(entered_rx.recv())
        .await
        .expect("approval started");
    let begin = tokio::time::Instant::now();
    cancel.cancel();
    let error = with_timeout(run).await.unwrap().expect_err("cancelled run");
    assert_eq!(error.message, "context canceled");
    assert!(begin.elapsed() < Duration::from_secs(2));
    assert!(with_timeout(cancelled_rx.recv()).await.is_some());
    assert_eq!(
        decision(&reply_rx.recv().await.expect("approval reply")),
        json!({"decision": "decline"})
    );
    fake.assert_params(
        "turn/interrupt",
        json!({"threadId": "thr-1", "turnId": "turn-1"}),
    );
    pool.close().await;
}

#[tokio::test]
async fn approval_cancelled_by_crash() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(|fake| async move {
            let _ = fake
                .request(
                    "item/commandExecution/requestApproval",
                    json!({"threadId": "thr-1", "turnId": "turn-1", "command": "rm x"}),
                )
                .await;
        }),
    );
    let thread = start(&pool, options()).await;
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let run = tokio::spawn(async move {
        thread
            .run(
                ctx(),
                "hi".to_string(),
                None,
                ThreadCallbacks {
                    emit: no_emit(),
                    approve: Arc::new(Blocking {
                        entered: entered_tx,
                        cancelled: cancelled_tx,
                    }),
                },
            )
            .await
    });
    with_timeout(entered_rx.recv())
        .await
        .expect("approval started");
    fake.crash();
    let error = with_timeout(run).await.unwrap().expect_err("crashed run");
    assert!(
        error.message.starts_with("codex app-server exited:"),
        "error = {error}"
    );
    assert!(error.message.contains("boom"), "error = {error}");
    assert!(with_timeout(cancelled_rx.recv()).await.is_some());
}

#[tokio::test]
async fn approval_cancelled_by_close() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(move |fake| {
            let reply_tx = reply_tx.clone();
            async move {
                let reply = fake
                    .request(
                        "item/commandExecution/requestApproval",
                        json!({"threadId": "thr-1", "turnId": "turn-1", "command": "rm x"}),
                    )
                    .await
                    .expect("approval reply");
                let _ = reply_tx.send(reply);
            }
        }),
    );
    let thread = Arc::new(start(&pool, options()).await);
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let runner = thread.clone();
    let run = tokio::spawn(async move {
        runner
            .run(
                ctx(),
                "hi".to_string(),
                None,
                ThreadCallbacks {
                    emit: no_emit(),
                    approve: Arc::new(Blocking {
                        entered: entered_tx,
                        cancelled: cancelled_tx,
                    }),
                },
            )
            .await
    });
    with_timeout(entered_rx.recv())
        .await
        .expect("approval started");
    with_timeout(thread.close()).await.unwrap();
    let error = with_timeout(run).await.unwrap().expect_err("closed run");
    assert_eq!(error.message, "codex thread closed; start a new session");
    assert!(with_timeout(cancelled_rx.recv()).await.is_some());
    assert_eq!(
        decision(&reply_rx.recv().await.expect("approval reply")),
        json!({"decision": "decline"})
    );
    pool.close().await;
}

#[tokio::test(start_paused = true)]
async fn cancel_interrupt_timeout_is_thirty_seconds() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    script_turn(&fake, "thr-1", "turn-1", script(|_fake| async move {}));
    let thread = start(&pool, options()).await;
    let run_ctx = ctx();
    let cancel = run_ctx.cancel.clone();
    let begin = tokio::time::Instant::now();
    let error = thread
        .run(
            run_ctx,
            "hi".to_string(),
            None,
            ThreadCallbacks {
                emit: Arc::new(move |_kind, event| {
                    if event["type"] == "task_started" {
                        cancel.cancel();
                    }
                }),
                approve: Arc::new(Always(true)),
            },
        )
        .await
        .expect_err("cancelled run");
    assert_eq!(error.message, "context canceled");
    assert_eq!(tokio::time::Instant::now() - begin, Duration::from_secs(30));
    fake.assert_params(
        "turn/interrupt",
        json!({"threadId": "thr-1", "turnId": "turn-1"}),
    );
    thread.close().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn cancel_before_start_response() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Notify::new());
    let release_for_handler = release.clone();
    fake.handle("turn/start", move |_fake, _params| {
        let entered_tx = entered_tx.lock().unwrap().take();
        let release = release_for_handler.clone();
        async move {
            if let Some(sender) = entered_tx {
                let _ = sender.send(());
            }
            release.notified().await;
            Ok::<Value, String>(json!({"turn": {"id": "turn-1"}}))
        }
    });
    fake.handle("turn/interrupt", |fake, _params| async move {
        complete_turn(&fake, "thr-1", "turn-1", "interrupted").await;
        Ok::<Value, String>(json!({}))
    });
    let thread = start(&pool, options()).await;
    let run_ctx = ctx();
    let cancel = run_ctx.cancel.clone();
    let run = tokio::spawn(async move {
        thread
            .run(
                run_ctx,
                "hi".to_string(),
                None,
                ThreadCallbacks {
                    emit: no_emit(),
                    approve: Arc::new(Always(true)),
                },
            )
            .await
    });
    entered_rx.await.expect("turn/start entered");
    cancel.cancel();
    release.notify_waiters();
    let error = with_timeout(run).await.unwrap().expect_err("cancelled run");
    assert_eq!(error.message, "context canceled");
    fake.assert_params(
        "turn/interrupt",
        json!({"threadId": "thr-1", "turnId": "turn-1"}),
    );
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Crashes, resume, close, and strays
// ---------------------------------------------------------------------------

#[tokio::test]
async fn crash_then_resume() {
    for during_start in [false, true] {
        for instructions in [false, true] {
            let first = FakeCodex::new();
            let second = FakeCodex::new();
            let pool = fake_pool(&[first.clone(), second.clone()]);
            script_turn(
                &first,
                "thr-1",
                "turn-1",
                script(|fake| async move {
                    complete_turn(&fake, "thr-1", "turn-1", "completed").await;
                }),
            );
            let mut opts = options();
            opts.cwd = PathBuf::from("/work");
            opts.model = "model".to_string();
            opts.sandbox = Sandbox::ReadOnly;
            opts.approval_policy = ApprovalPolicy::Never;
            let mut want = json!({
                "threadId": "thr-1",
                "cwd": "/work",
                "model": "model",
                "sandbox": "read-only",
                "approvalPolicy": "never",
            });
            if instructions {
                opts.approval_policy = ApprovalPolicy::OnFailure;
                opts.base_instructions = Some("base".to_string());
                opts.developer_instructions = Some("developer".to_string());
                want["approvalPolicy"] = json!("on-request");
                want["baseInstructions"] = json!("base");
                want["developerInstructions"] = json!("developer");
            }
            let thread = start(&pool, opts).await;
            with_timeout(thread.run(
                ctx(),
                "first".to_string(),
                None,
                ThreadCallbacks {
                    emit: no_emit(),
                    approve: Arc::new(Always(true)),
                },
            ))
            .await
            .unwrap();
            if during_start {
                let crash_fake = first.clone();
                first.handle("turn/start", move |_fake, _params| {
                    let crash_fake = crash_fake.clone();
                    async move {
                        crash_fake.crash();
                        Ok::<Value, String>(Value::Null)
                    }
                });
            } else {
                script_turn(&first, "thr-1", "turn-2", script(|_fake| async move {}));
            }
            let crash_events = Arc::new(Mutex::new(0usize));
            let sink = crash_events.clone();
            let crash_fake = first.clone();
            let error = with_timeout(thread.run(
                ctx(),
                "crash".to_string(),
                None,
                ThreadCallbacks {
                    emit: Arc::new(move |_kind, event| {
                        if event["type"] == "task_started" {
                            *sink.lock().unwrap() += 1;
                            crash_fake.crash();
                        }
                    }),
                    approve: Arc::new(Always(true)),
                },
            ))
            .await
            .expect_err("crash");
            assert!(
                error.message.starts_with("codex app-server exited:"),
                "error = {error}"
            );
            assert!(error.message.contains("boom"), "error = {error}");
            let _ = tokio::time::timeout(Duration::from_secs(2), first.client().done().cancelled())
                .await;

            let resumed = Arc::new(AtomicBool::new(false));
            let resumed_for_handler = resumed.clone();
            let resumed_for_turn = resumed.clone();
            second.handle("thread/resume", move |_fake, _params| {
                let resumed = resumed_for_handler.clone();
                async move {
                    resumed.store(true, Ordering::SeqCst);
                    Ok::<Value, String>(json!({"thread": {"id": "thr-1"}}))
                }
            });
            second.handle("turn/start", move |fake, _params| {
                let resumed = resumed_for_turn.clone();
                async move {
                    if !resumed.load(Ordering::SeqCst) {
                        return Err("turn/start before thread/resume".to_string());
                    }
                    fake.notify(
                        "turn/started",
                        json!({"threadId": "thr-1", "turn": {"id": "turn-3"}}),
                    )
                    .await;
                    agent_message(&fake, "thr-1", "turn-3", "final_answer", "resumed").await;
                    complete_turn(&fake, "thr-1", "turn-3", "completed").await;
                    Ok::<Value, String>(json!({"turn": {"id": "turn-3"}}))
                }
            });
            let text = with_timeout(thread.run(
                ctx(),
                "again".to_string(),
                None,
                ThreadCallbacks {
                    emit: no_emit(),
                    approve: Arc::new(Always(true)),
                },
            ))
            .await
            .unwrap();
            assert_eq!(text, "resumed");
            second.assert_params("thread/resume", want);
            second.assert_no("thread/start");
            pool.close().await;
        }
    }
}

#[tokio::test]
async fn lost_before_first_turn() {
    let first = FakeCodex::new();
    let second = FakeCodex::new();
    let pool = fake_pool(&[first.clone(), second.clone()]);
    let thread = start(&pool, options()).await;
    crash_and_wait(&first).await;
    let error = with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: no_emit(),
            approve: Arc::new(Always(true)),
        },
    ))
    .await
    .expect_err("lost thread");
    assert_eq!(error.message, "codex session lost; start a new session");
    second.assert_no("thread/resume");
    second.assert_no("turn/start");
}

#[tokio::test]
async fn close_unsubscribes() {
    for dead in [false, true] {
        let fake = FakeCodex::new();
        let pool = fake_pool(std::slice::from_ref(&fake));
        let thread = start(&pool, options()).await;
        if dead {
            fake.crash();
        }
        with_timeout(thread.close()).await.unwrap();
        with_timeout(thread.close()).await.unwrap();
        if !dead {
            fake.assert_params("thread/unsubscribe", json!({"threadId": "thr-1"}));
        }
        fake.assert_no("turn/interrupt");
        pool.close().await;
    }
}

#[tokio::test]
async fn close_active_turn_rejects_queued_approval() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let (first_tx, mut first_rx) = mpsc::unbounded_channel();
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(move |fake| {
            let first_tx = first_tx.clone();
            async move {
                let reply = fake
                    .request(
                        "item/commandExecution/requestApproval",
                        json!({"threadId": "thr-1", "turnId": "turn-1", "command": "one"}),
                    )
                    .await
                    .expect("first approval reply");
                let _ = first_tx.send(reply);
            }
        }),
    );
    let thread = Arc::new(start(&pool, options()).await);
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, _cancelled_rx) = mpsc::unbounded_channel();
    let runner = thread.clone();
    let run = tokio::spawn(async move {
        runner
            .run(
                ctx(),
                "hi".to_string(),
                None,
                ThreadCallbacks {
                    emit: no_emit(),
                    approve: Arc::new(Blocking {
                        entered: entered_tx,
                        cancelled: cancelled_tx,
                    }),
                },
            )
            .await
    });
    with_timeout(entered_rx.recv())
        .await
        .expect("approval started");

    let second_fake = fake.clone();
    let second = tokio::spawn(async move {
        second_fake
            .request(
                "item/commandExecution/requestApproval",
                json!({"threadId": "thr-1", "turnId": "turn-1", "command": "two"}),
            )
            .await
    });
    // Let the second request reach the turn queue before closing.
    tokio::time::sleep(Duration::from_millis(50)).await;

    with_timeout(thread.close()).await.unwrap();
    let second = with_timeout(second).await.unwrap().expect("second reply");
    assert_rejected(&second);
    fake.assert_params(
        "turn/interrupt",
        json!({"threadId": "thr-1", "turnId": "turn-1"}),
    );
    fake.assert_params("thread/unsubscribe", json!({"threadId": "thr-1"}));
    let error = with_timeout(run).await.unwrap().expect_err("closed run");
    assert_eq!(error.message, "codex thread closed; start a new session");
    assert_eq!(
        decision(&first_rx.recv().await.expect("first reply")),
        json!({"decision": "decline"})
    );
    pool.close().await;
}

#[tokio::test]
async fn run_after_close() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let thread = start(&pool, options()).await;
    thread.close().await.unwrap();
    let error = with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: no_emit(),
            approve: Arc::new(Always(true)),
        },
    ))
    .await
    .expect_err("run after close");
    assert_eq!(error.message, "codex thread closed; start a new session");
    fake.assert_no("turn/start");
    pool.close().await;
}

#[tokio::test]
async fn close_during_turn_start_interrupts_once_id_known() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    let (entered_tx, entered_rx) = oneshot::channel();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Notify::new());
    let release_for_handler = release.clone();
    fake.handle("turn/start", move |_fake, _params| {
        let entered_tx = entered_tx.lock().unwrap().take();
        let release = release_for_handler.clone();
        async move {
            if let Some(sender) = entered_tx {
                let _ = sender.send(());
            }
            release.notified().await;
            Ok::<Value, String>(json!({"turn": {"id": "turn-1"}}))
        }
    });
    let thread = Arc::new(start(&pool, options()).await);
    let runner = thread.clone();
    let run = tokio::spawn(async move {
        runner
            .run(
                ctx(),
                "hi".to_string(),
                None,
                ThreadCallbacks {
                    emit: no_emit(),
                    approve: Arc::new(Always(true)),
                },
            )
            .await
    });
    entered_rx.await.expect("turn/start entered");
    let closer = thread.clone();
    let close = tokio::spawn(async move { closer.close().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    release.notify_waiters();
    with_timeout(close).await.unwrap().unwrap();
    let error = with_timeout(run).await.unwrap().expect_err("closed run");
    assert_eq!(error.message, "codex thread closed; start a new session");
    fake.assert_params(
        "turn/interrupt",
        json!({"threadId": "thr-1", "turnId": "turn-1"}),
    );
    pool.close().await;
}

#[tokio::test(start_paused = true)]
async fn settle_stray_times_out_after_thirty_seconds() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    script_turn(&fake, "thr-1", "turn-1", script(|_fake| async move {}));
    let thread = start(&pool, options()).await;
    let run_ctx = ctx();
    let cancel = run_ctx.cancel.clone();
    let first = thread
        .run(
            run_ctx,
            "hi".to_string(),
            None,
            ThreadCallbacks {
                emit: Arc::new(move |_kind, event| {
                    if event["type"] == "task_started" {
                        cancel.cancel();
                    }
                }),
                approve: Arc::new(Always(true)),
            },
        )
        .await
        .expect_err("first run");
    assert_eq!(first.message, "context canceled");

    let begin = tokio::time::Instant::now();
    let error = thread
        .run(
            ctx(),
            "again".to_string(),
            None,
            ThreadCallbacks {
                emit: no_emit(),
                approve: Arc::new(Always(true)),
            },
        )
        .await
        .expect_err("stray turn");
    assert!(
        error.message.contains("still running the previous turn"),
        "error = {error}"
    );
    assert_eq!(tokio::time::Instant::now() - begin, Duration::from_secs(30));
    assert_eq!(fake.received("turn/interrupt").len(), 2);
    assert_eq!(fake.received("turn/start").len(), 1);
    thread.close().await.unwrap();
    pool.close().await;
}

#[tokio::test]
async fn completed_turn_leaves_no_stray() {
    let fake = FakeCodex::new();
    let pool = fake_pool(std::slice::from_ref(&fake));
    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(|fake| async move {
            agent_message(&fake, "thr-1", "turn-1", "final_answer", "done").await;
            complete_turn(&fake, "thr-1", "turn-1", "completed").await;
        }),
    );
    script_turn(
        &fake,
        "thr-1",
        "turn-2",
        script(|fake| async move {
            agent_message(&fake, "thr-1", "turn-2", "final_answer", "again").await;
            complete_turn(&fake, "thr-1", "turn-2", "completed").await;
        }),
    );
    let thread = start(&pool, options()).await;
    for prompt in ["hi", "again"] {
        with_timeout(thread.run(
            ctx(),
            prompt.to_string(),
            None,
            ThreadCallbacks {
                emit: no_emit(),
                approve: Arc::new(Always(true)),
            },
        ))
        .await
        .unwrap();
    }
    fake.assert_no("turn/interrupt");
    assert_eq!(fake.received("turn/start").len(), 2);
    thread.close().await.unwrap();
    pool.close().await;
}

// ---------------------------------------------------------------------------
// Adapter capabilities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn adapter_lists_models_and_starts_threads() {
    let command = "codex-fake-models-13b";
    let fake = FakeCodex::new();
    fake.handle("model/list", |_fake, params| async move {
        let cursor = params["cursor"].as_str().unwrap_or("");
        let models = ["a", "b"];
        let start = if cursor.is_empty() {
            0
        } else {
            cursor.parse::<usize>().unwrap_or(0)
        };
        let mut data = Vec::new();
        let mut next = Value::Null;
        if start < models.len() {
            data.push(json!({"id": models[start]}));
            if start + 1 < models.len() {
                next = json!((start + 1).to_string());
            }
        }
        Ok::<Value, String>(json!({"data": data, "nextCursor": next}))
    });
    pool::install_pool(command, fake_pool(std::slice::from_ref(&fake)));
    let adapter = CodexAdapter::new("test-codex", &provider_config(command)).expect("adapter");
    let models = with_timeout(adapter.list_models()).await.unwrap();
    assert_eq!(
        models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(fake.received("model/list").len(), 2);

    script_turn(
        &fake,
        "thr-1",
        "turn-1",
        script(|fake| async move {
            agent_message(&fake, "thr-1", "turn-1", "final_answer", "echo: hi").await;
            complete_turn(&fake, "thr-1", "turn-1", "completed").await;
        }),
    );
    let thread = adapter
        .start_thread(&ctx(), options())
        .await
        .expect("agent start thread");
    let text = with_timeout(thread.run(
        ctx(),
        "hi".to_string(),
        None,
        ThreadCallbacks {
            emit: no_emit(),
            approve: Arc::new(Always(true)),
        },
    ))
    .await
    .unwrap();
    assert_eq!(text, "echo: hi");
    thread.close().await.unwrap();
    pool::remove_pool(command);
}

#[tokio::test]
async fn adapter_checks_auth() {
    let command = "codex-fake-auth-13b";
    let fake = FakeCodex::new();
    fake.handle("account/read", |_fake, _params| async move {
        Ok::<Value, String>(json!({
            "account": {"type": "chatgpt", "planType": "plus"},
            "requiresOpenaiAuth": true,
        }))
    });
    pool::install_pool(command, fake_pool(std::slice::from_ref(&fake)));
    let adapter = CodexAdapter::new("test-codex", &provider_config(command)).expect("adapter");
    let status = with_timeout(adapter.check_auth()).await.unwrap();
    assert_eq!(
        status,
        AuthStatus {
            summary: "chatgpt (plus)".to_string(),
            version: Some(format!("codex-cli {}", pinned_version())),
        }
    );
    pool::remove_pool(command);
}

#[tokio::test]
async fn adapter_reports_logged_out() {
    let command = "codex-fake-auth-out-13b";
    let fake = FakeCodex::new();
    fake.handle("account/read", |_fake, _params| async move {
        Ok::<Value, String>(json!({"account": null, "requiresOpenaiAuth": true}))
    });
    pool::install_pool(command, fake_pool(std::slice::from_ref(&fake)));
    let adapter = CodexAdapter::new("test-codex", &provider_config(command)).expect("adapter");
    let error = with_timeout(adapter.check_auth())
        .await
        .expect_err("logged out");
    assert_eq!(error.message, "not logged in; run codex login");
    pool::remove_pool(command);
}

#[test]
fn adapter_implements_optional_capabilities() {
    fn assert_agent<T: Agent>() {}
    fn assert_models<T: ModelLister>() {}
    fn assert_auth<T: AuthChecker>() {}
    fn assert_provider<T: Provider>() {}
    assert_agent::<CodexAdapter>();
    assert_models::<CodexAdapter>();
    assert_auth::<CodexAdapter>();
    assert_provider::<CodexAdapter>();
}

// ---------------------------------------------------------------------------
// Bounded cleanup with the fixture app-server
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn install_fixture(name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    (dir, path)
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_cleanup_closes_stdin_and_reaps_child() {
    let (_dir, path) = install_fixture(
        "app-server.sh",
        include_str!("fixtures/codex/app-server.sh"),
    );
    let command = path.to_string_lossy().into_owned();
    let pool = Pool::for_command(&command);
    let conn = with_timeout(pool.get(&ctx())).await.expect("acquire");
    let begin = tokio::time::Instant::now();
    with_timeout(pool.close()).await;
    // The fixture exits on stdin EOF, well inside the 5 s grace period.
    assert!(begin.elapsed() < Duration::from_secs(5));
    assert!(conn.client.is_done(), "stdout reader was not released");
    pool::remove_pool(&command);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_cleanup_kills_direct_child_after_grace() {
    let (_dir, path) = install_fixture(
        "app-server-hold.sh",
        include_str!("fixtures/codex/app-server-hold.sh"),
    );
    let command = path.to_string_lossy().into_owned();
    let pool = Pool::for_command(&command);
    let conn = with_timeout(pool.get(&ctx())).await.expect("acquire");
    let begin = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(12), pool.close())
        .await
        .expect("close did not finish");
    let elapsed = begin.elapsed();
    // Stdin EOF is ignored by the fixture, so the direct child is killed after
    // the 5 s grace period. Descendants are not asserted.
    assert!(elapsed >= Duration::from_secs(5), "elapsed = {elapsed:?}");
    assert!(elapsed < Duration::from_secs(10), "elapsed = {elapsed:?}");
    assert!(conn.client.is_done(), "stdout reader was not released");
    pool::remove_pool(&command);
}

#[test]
fn fixture_version_is_pinned() {
    let version = pinned_version();
    assert!(!version.is_empty(), "version.txt is empty");
    for (name, contents) in [
        (
            "app-server.sh",
            include_str!("fixtures/codex/app-server.sh"),
        ),
        (
            "app-server-hold.sh",
            include_str!("fixtures/codex/app-server-hold.sh"),
        ),
    ] {
        assert!(
            contents.contains(&format!("codex-cli {version}")),
            "{name} does not record the pinned codex-cli {version}"
        );
    }
}
