//! One ACP agent child process per thread: spawn, `initialize`,
//! `session/new`, prompt turns, cancellation and shutdown.

use std::collections::BTreeMap;
use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::process::{Child, ChildStderr, ChildStdin, Command};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::events::{self, Stop, ToolCallCache};
use super::rpc::{Inbound, Peer, RpcError};
use crate::provider::types::{AuthStatus, ProviderError, Thread, ThreadCallbacks, ThreadOptions};
use crate::util::approval::Approver;
use crate::util::run::RunContext;

/// ACP protocol version sent and accepted.
pub const PROTOCOL_VERSION: i64 = 1;
/// Bound on `initialize`, `session/new` and `session/set_model`.
pub const CAPABILITY_TIMEOUT: Duration = Duration::from_secs(30);
/// Default wait for a prompt result after `session/cancel`.
pub const CANCEL_GRACE: Duration = Duration::from_secs(5);
/// How long `close` waits for the child to exit after stdin EOF.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Bytes of child stderr kept for error messages.
pub const STDERR_TAIL_SIZE: usize = 8 * 1024;

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const AUTH_REQUIRED: i64 = -32000;

/// How to launch one agent.
#[derive(Clone, Debug)]
pub struct Spawn {
    pub provider: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Wait for the prompt result after `session/cancel` before killing.
    pub cancel_grace: Duration,
}

impl Spawn {
    pub fn new(provider: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cancel_grace: CANCEL_GRACE,
        }
    }
}

// --- Child registry --------------------------------------------------------

struct ChildSlot {
    child: Mutex<Option<Child>>,
    pid: Option<u32>,
    /// Set once `try_wait` has reaped the leader. From then on its pid, and
    /// so the process group id, may belong to an unrelated process.
    reaped: AtomicBool,
}

/// Whether the kill path may signal the child's process group. Never once the
/// leader has been reaped: the pgid may have been reused.
fn may_signal_group(pid: Option<u32>, reaped: bool) -> bool {
    pid.is_some() && !reaped
}

impl ChildSlot {
    /// Kill the child (and on Unix its process group) and reap it, waiting at
    /// most `wait`. Synchronous so the shutdown hook can use it.
    fn kill_and_reap(&self, wait: Duration) {
        // Holding the child (taken under the lock) means no other path can
        // reap it between the check and the signal.
        let Some(mut child) = self.child.lock().unwrap().take() else {
            return;
        };
        // Signal the group before any reaping `try_wait` in this path.
        #[cfg(unix)]
        if may_signal_group(self.pid, self.reaped.load(Ordering::SeqCst))
            && let Some(pid) = self.pid
        {
            // SAFETY: killpg has no memory-safety preconditions; the group id
            // is the child's pid because it was spawned with process_group(0),
            // and the leader is unreaped so the id cannot have been reused.
            unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        if self.reaped.load(Ordering::SeqCst) {
            return;
        }
        let _ = child.start_kill();
        let deadline = std::time::Instant::now() + wait;
        while std::time::Instant::now() < deadline {
            if !matches!(child.try_wait(), Ok(None)) {
                self.reaped.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Whether the leader has exited. Reaps it if so, and records that.
    fn exited(&self) -> bool {
        if self.reaped.load(Ordering::SeqCst) {
            return true;
        }
        match self.child.lock().unwrap().as_mut() {
            Some(child) => {
                let gone = !matches!(child.try_wait(), Ok(None));
                if gone {
                    self.reaped.store(true, Ordering::SeqCst);
                }
                gone
            }
            None => true,
        }
    }
}

static REGISTRY: Mutex<Vec<Arc<ChildSlot>>> = Mutex::new(Vec::new());

fn register(slot: &Arc<ChildSlot>) {
    REGISTRY.lock().unwrap().push(Arc::clone(slot));
}

fn unregister(slot: &Arc<ChildSlot>) {
    REGISTRY
        .lock()
        .unwrap()
        .retain(|other| !Arc::ptr_eq(other, slot));
}

/// Kill and reap every live ACP child. For the process shutdown hook.
pub fn kill_all() {
    let slots: Vec<_> = REGISTRY.lock().unwrap().drain(..).collect();
    for slot in slots {
        slot.kill_and_reap(Duration::from_secs(2));
    }
}

// --- Stderr tail -------------------------------------------------------------

/// Retains only the last [`STDERR_TAIL_SIZE`] bytes, including across partial
/// writes. Copied from `codex/pool.rs`.
#[derive(Default)]
struct StderrTail {
    buffer: Vec<u8>,
}

impl StderrTail {
    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= STDERR_TAIL_SIZE {
            self.buffer.clear();
            self.buffer
                .extend_from_slice(&bytes[bytes.len() - STDERR_TAIL_SIZE..]);
            return;
        }
        let overflow = self.buffer.len() + bytes.len();
        if overflow > STDERR_TAIL_SIZE {
            let drop = overflow - STDERR_TAIL_SIZE;
            self.buffer.drain(..drop);
        }
        self.buffer.extend_from_slice(bytes);
    }

    fn string(&self) -> String {
        String::from_utf8_lossy(&self.buffer).into_owned()
    }
}

async fn drain_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<StderrTail>>) {
    let mut buffer = [0u8; 8192];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => return,
            Ok(read) => tail.lock().unwrap().push(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

// --- Closeable stdin -------------------------------------------------------

/// Child stdin that `close` can drop (sending EOF) while the peer's writer
/// task still owns the writer half.
struct SharedStdin(Arc<Mutex<Option<ChildStdin>>>);

fn broken_pipe() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed")
}

impl AsyncWrite for SharedStdin {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.0.lock().unwrap().as_mut() {
            Some(stdin) => Pin::new(stdin).poll_write(cx, buf),
            None => Poll::Ready(Err(broken_pipe())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.0.lock().unwrap().as_mut() {
            Some(stdin) => Pin::new(stdin).poll_flush(cx),
            None => Poll::Ready(Err(broken_pipe())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.0.lock().unwrap().as_mut() {
            Some(stdin) => Pin::new(stdin).poll_shutdown(cx),
            None => Poll::Ready(Ok(())),
        }
    }
}

// --- Process ---------------------------------------------------------------

struct Process {
    name: String,
    peer: Peer,
    inbound: UnboundedReceiver<Inbound>,
    slot: Arc<ChildSlot>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    tail: Arc<Mutex<StderrTail>>,
    stderr_task: Mutex<Option<JoinHandle<()>>>,
}

impl Process {
    async fn spawn(spawn: &Spawn, cwd: Option<&std::path::Path>) -> Result<Self, ProviderError> {
        let name = spawn.provider.clone();
        let mut cmd = Command::new(&spawn.command);
        cmd.args(&spawn.args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        cmd.env_clear()
            .envs(crate::tools::shell::scrubbed_env())
            .envs(&spawn.env);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = {
            // Same reason as codex/pool.rs: do not inherit the managed shell's
            // temporarily inheritable pipe handle.
            #[cfg(windows)]
            let _spawn_guard = crate::tools::process_windows::spawn_guard();
            cmd.spawn()
                .map_err(|e| perr(&name, format!("start {:?} failed: {e}", spawn.command)))?
        };
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
            return Err(perr(&name, "start failed: child has no piped stdio"));
        };
        let slot = Arc::new(ChildSlot {
            child: Mutex::new(Some(child)),
            pid,
            reaped: AtomicBool::new(false),
        });
        register(&slot);
        let tail = Arc::new(Mutex::new(StderrTail::default()));
        let stderr_task = tokio::spawn(drain_stderr(stderr, Arc::clone(&tail)));
        let stdin = Arc::new(Mutex::new(Some(stdin)));
        let (peer, inbound) = Peer::spawn(stdout, SharedStdin(Arc::clone(&stdin)));
        Ok(Self {
            name,
            peer,
            inbound,
            slot,
            stdin,
            tail,
            stderr_task: Mutex::new(Some(stderr_task)),
        })
    }

    /// `acp provider "<name>": <method> failed: <message>`, plus the stderr
    /// tail when the child has died.
    async fn call_error(&self, method: &str, err: &RpcError) -> ProviderError {
        let mut message = format!("{method} failed: {}", err.message);
        if let Some(tail) = self.dead_tail().await {
            message.push_str(&format!(" (stderr: {})", tail.trim_end()));
        }
        perr(&self.name, message)
    }

    /// The stderr tail when the child is gone, after letting the drain task
    /// catch up.
    async fn dead_tail(&self) -> Option<String> {
        if !self.peer.closed() && !self.slot.exited() {
            return None;
        }
        let task = self.stderr_task.lock().unwrap().take();
        if let Some(task) = task {
            let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
        }
        Some(self.tail.lock().unwrap().string())
    }

    async fn call(
        &self,
        ctx: &RunContext,
        method: &str,
        params: Value,
    ) -> Result<Value, ProviderError> {
        match self.peer.call(ctx, method, params).await {
            Ok(v) => Ok(v),
            Err(e) => Err(self.call_error(method, &e).await),
        }
    }

    fn kill(&self) {
        self.slot.kill_and_reap(Duration::from_secs(2));
        unregister(&self.slot);
    }

    /// Drop stdin, wait up to `grace` for exit, then kill.
    async fn shutdown(&self, grace: Duration) {
        self.stdin.lock().unwrap().take();
        let deadline = tokio::time::Instant::now() + grace;
        while tokio::time::Instant::now() < deadline && !self.slot.exited() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.kill();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill();
    }
}

fn perr(name: &str, message: impl std::fmt::Display) -> ProviderError {
    ProviderError {
        message: format!("acp provider {name:?}: {message}"),
    }
}

fn capability_ctx(ctx: &RunContext) -> RunContext {
    let mut child = ctx.child();
    let bound = tokio::time::Instant::now() + CAPABILITY_TIMEOUT;
    child.deadline = Some(child.deadline.map_or(bound, |d| d.min(bound)));
    child
}

fn initialize_params() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {
            "fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false,
        },
        "clientInfo": {"name": "subagent-mcp", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// Send `initialize` and check the version. Returns the result.
async fn initialize(proc: &Process, ctx: &RunContext) -> Result<Value, ProviderError> {
    let result = proc
        .call(&capability_ctx(ctx), "initialize", initialize_params())
        .await?;
    let version = result.get("protocolVersion").and_then(Value::as_i64);
    if version != Some(PROTOCOL_VERSION) {
        let shown = result
            .get("protocolVersion")
            .map(Value::to_string)
            .unwrap_or_else(|| "none".into());
        return Err(perr(
            &proc.name,
            format!("unsupported ACP protocolVersion {shown} (want {PROTOCOL_VERSION})"),
        ));
    }
    Ok(result)
}

fn auth_method_ids(init: &Value) -> Vec<String> {
    init.get("authMethods")
        .and_then(Value::as_array)
        .map(|methods| {
            methods
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Initialize only, for `--check-config`.
pub async fn probe(ctx: &RunContext, spawn: Spawn) -> Result<AuthStatus, ProviderError> {
    let proc = Process::spawn(&spawn, None).await?;
    let result = initialize(&proc, ctx).await;
    proc.kill();
    let init = result?;
    let info = init.get("agentInfo");
    let field = |k: &str| {
        info.and_then(|i| i.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let version = field("version");
    let summary = match (field("name"), &version) {
        (Some(n), Some(v)) => format!("ACP agent {n} {v}"),
        (Some(n), None) => format!("ACP agent {n}"),
        _ => format!("ACP agent ready (protocol {PROTOCOL_VERSION})"),
    };
    Ok(AuthStatus { summary, version })
}

/// Spawn the agent, initialize it and open a session.
pub async fn start(
    ctx: &RunContext,
    spawn: Spawn,
    opts: ThreadOptions,
) -> Result<AcpThread, ProviderError> {
    let proc = Process::spawn(&spawn, Some(&opts.cwd)).await?;
    let init = initialize(&proc, ctx).await?;
    let new = proc
        .peer
        .call(
            &capability_ctx(ctx),
            "session/new",
            json!({"cwd": opts.cwd, "mcpServers": []}),
        )
        .await;
    let session = match new {
        Ok(v) => v,
        Err(e) => {
            let ids = auth_method_ids(&init);
            if e.code == AUTH_REQUIRED
                || (!ids.is_empty() && e.message.to_lowercase().contains("auth"))
            {
                return Err(perr(
                    &proc.name,
                    format!(
                        "authentication required (authMethods: {}); log in with the agent itself",
                        ids.join(", ")
                    ),
                ));
            }
            return Err(proc.call_error("session/new", &e).await);
        }
    };
    let Some(session_id) = session
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Err(perr(
            &proc.name,
            "session/new failed: no sessionId in result",
        ));
    };
    let mut warnings = Vec::new();
    if opts.model != "default" && !opts.model.is_empty() {
        let res = proc
            .peer
            .call(
                &capability_ctx(ctx),
                "session/set_model",
                json!({"sessionId": session_id, "modelId": opts.model}),
            )
            .await;
        match res {
            Ok(_) => {}
            Err(e) if e.code == METHOD_NOT_FOUND || e.code == INVALID_PARAMS => {
                warnings.push(format!(
                    "acp: model {:?} could not be set and was ignored: {}",
                    opts.model, e.message
                ));
            }
            Err(e) => return Err(proc.call_error("session/set_model", &e).await),
        }
    }
    let instructions = [&opts.base_instructions, &opts.developer_instructions]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    let instructions = (!instructions.is_empty()).then(|| instructions.join("\n\n"));
    Ok(AcpThread {
        name: spawn.provider.clone(),
        session_id,
        sandbox: opts.sandbox,
        cancel_grace: spawn.cancel_grace,
        dead: AtomicBool::new(false),
        closing: CancellationToken::new(),
        turn: tokio::sync::Mutex::new(Turn {
            proc: Some(proc),
            cache: ToolCallCache::default(),
            instructions,
            effort_warned: false,
            warnings,
        }),
    })
}

// --- Thread ------------------------------------------------------------------

struct Turn {
    proc: Option<Process>,
    cache: ToolCallCache,
    /// Prepended to the first prompt, then cleared.
    instructions: Option<String>,
    effort_warned: bool,
    /// Start-time warnings emitted on the first run.
    warnings: Vec<String>,
}

/// One ACP session in its own child process.
pub struct AcpThread {
    name: String,
    session_id: String,
    sandbox: crate::policy::Sandbox,
    cancel_grace: Duration,
    dead: AtomicBool,
    closing: CancellationToken,
    turn: tokio::sync::Mutex<Turn>,
}

fn warn(callbacks: &ThreadCallbacks, message: &str) {
    (callbacks.emit)("warning", json!({"type": "warning", "message": message}));
}

async fn wait_ctx(ctx: &RunContext) {
    match ctx.deadline {
        Some(d) => tokio::select! {
            _ = ctx.cancel.cancelled() => {}
            _ = tokio::time::sleep_until(d) => {}
        },
        None => ctx.cancel.cancelled().await,
    }
}

fn ctx_done(ctx: &RunContext) -> bool {
    ctx.cancel.is_cancelled() || ctx.deadline_reason().is_some()
}

enum Outcome {
    Done(Result<Value, RpcError>),
    Cancelled,
}

impl AcpThread {
    fn dead_error(&self) -> ProviderError {
        perr(&self.name, "agent process exited; start a new session")
    }

    fn cancel_error(&self, ctx: &RunContext) -> ProviderError {
        match ctx.deadline_reason() {
            Some(reason) => perr(&self.name, reason),
            None => perr(&self.name, "run cancelled"),
        }
    }

    /// Answer one inbound message during a turn. Returns `true` when the run
    /// was cancelled while an approval was pending.
    async fn handle_inbound(
        &self,
        ctx: &RunContext,
        proc: &Process,
        cache: &mut ToolCallCache,
        text: &mut String,
        callbacks: &ThreadCallbacks,
        msg: Inbound,
    ) -> bool {
        match msg {
            Inbound::Notification { method, params } => {
                if method == "session/update"
                    && let Some(mapped) = events::map_update(&self.session_id, &params, cache)
                {
                    if let Some(t) = mapped.text {
                        text.push_str(&t);
                    }
                    if let Some(event) = mapped.event {
                        let kind = event
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        (callbacks.emit)(&kind, event);
                    }
                }
                false
            }
            Inbound::Request { id, method, params } => {
                if method != "session/request_permission" {
                    let _ = proc
                        .peer
                        .respond(
                            id,
                            Err(RpcError {
                                code: METHOD_NOT_FOUND,
                                message: format!("method not supported: {method}"),
                                data: None,
                            }),
                        )
                        .await;
                    return false;
                }
                let (result, cancelled) = self
                    .decide_permission(ctx, callbacks.approve.as_ref(), &params)
                    .await;
                let _ = proc.peer.respond(id, Ok(result)).await;
                cancelled
            }
        }
    }

    /// The permission answer, and whether the run was cancelled meanwhile.
    async fn decide_permission(
        &self,
        ctx: &RunContext,
        approver: &dyn Approver,
        params: &Value,
    ) -> (Value, bool) {
        if ctx_done(ctx) {
            return (json!({"outcome": {"outcome": "cancelled"}}), true);
        }
        if events::read_only_auto_reject(self.sandbox, params) {
            return (events::pick_option(params, false), false);
        }
        let request = events::permission_request(&self.session_id, params);
        let approve_ctx = ctx.child();
        let mut future = Box::pin(approver.approve(&approve_ctx, request));
        let mut approved = false;
        let cancelled = tokio::select! {
            ok = &mut future => {
                approved = ok && !ctx_done(&approve_ctx);
                false
            }
            _ = wait_ctx(ctx) => true,
        };
        // The approver may finish because the run's cancellation reached its
        // child context first; that is still a cancellation.
        if cancelled || ctx_done(ctx) {
            approve_ctx.cancel.cancel();
            if cancelled {
                let _ = future.await;
            }
            return (json!({"outcome": {"outcome": "cancelled"}}), true);
        }
        (events::pick_option(params, approved), false)
    }
}

#[async_trait::async_trait]
impl Thread for AcpThread {
    async fn run(
        &self,
        ctx: RunContext,
        prompt: String,
        effort: Option<String>,
        callbacks: ThreadCallbacks,
    ) -> Result<String, ProviderError> {
        let mut guard = self.turn.lock().await;
        let turn = &mut *guard;
        if self.dead.load(Ordering::SeqCst) {
            return Err(self.dead_error());
        }
        let Some(proc) = turn.proc.as_mut() else {
            return Err(self.dead_error());
        };
        // A run cancelled by `close` behaves like an explicitly cancelled one.
        let ctx = {
            let child = ctx.child();
            let token = child.cancel.clone();
            let closing = self.closing.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = closing.cancelled() => token.cancel(),
                    _ = token.cancelled() => {}
                }
            });
            child
        };
        // Ends the forwarding task above when this run returns.
        let _forward_guard = ctx.cancel.clone().drop_guard();
        // Updates that arrived between turns are stale.
        while proc.inbound.try_recv().is_ok() {}
        for w in turn.warnings.drain(..) {
            warn(&callbacks, &w);
        }
        if effort.is_some() && !turn.effort_warned {
            turn.effort_warned = true;
            warn(&callbacks, "acp: effort is not supported and was ignored");
        }
        let text_in = match turn.instructions.take() {
            Some(ins) => format!("<instructions>\n{ins}\n</instructions>\n\n{prompt}"),
            None => prompt,
        };
        let params = json!({
            "sessionId": self.session_id,
            "prompt": [{"type": "text", "text": text_in}],
        });
        // The prompt call ignores ctx: after a cancel we still wait for its
        // result within the grace period.
        let prompt_ctx = RunContext {
            cancel: CancellationToken::new(),
            deadline: None,
        };
        let peer = proc.peer.clone();
        let mut prompt_fut =
            Box::pin(async move { peer.call(&prompt_ctx, "session/prompt", params).await });
        let mut text = String::new();
        let mut inbound_open = true;
        let outcome = loop {
            if ctx_done(&ctx) {
                break Outcome::Cancelled;
            }
            tokio::select! {
                biased;
                res = &mut prompt_fut => break Outcome::Done(res),
                _ = wait_ctx(&ctx) => break Outcome::Cancelled,
                msg = proc.inbound.recv(), if inbound_open => match msg {
                    Some(msg) => {
                        let proc_ref: &Process = proc;
                        if self.handle_inbound(&ctx, proc_ref, &mut turn.cache, &mut text, &callbacks, msg).await {
                            break Outcome::Cancelled;
                        }
                    }
                    None => inbound_open = false,
                },
            }
        };
        let result = match outcome {
            Outcome::Done(res) => res,
            Outcome::Cancelled => {
                let _ = proc
                    .peer
                    .notify("session/cancel", json!({"sessionId": self.session_id}))
                    .await;
                let grace = async {
                    loop {
                        tokio::select! {
                            biased;
                            res = &mut prompt_fut => return res,
                            msg = proc.inbound.recv(), if inbound_open => match msg {
                                Some(Inbound::Request { id, method, .. }) => {
                                    let answer = if method == "session/request_permission" {
                                        Ok(json!({"outcome": {"outcome": "cancelled"}}))
                                    } else {
                                        Err(RpcError {
                                            code: METHOD_NOT_FOUND,
                                            message: format!("method not supported: {method}"),
                                            data: None,
                                        })
                                    };
                                    let _ = proc.peer.respond(id, answer).await;
                                }
                                Some(Inbound::Notification { .. }) => {}
                                None => inbound_open = false,
                            },
                        }
                    }
                };
                if tokio::time::timeout(self.cancel_grace, grace)
                    .await
                    .is_err()
                {
                    self.dead.store(true, Ordering::SeqCst);
                    if let Some(p) = turn.proc.take() {
                        p.kill();
                    }
                }
                return Err(self.cancel_error(&ctx));
            }
        };
        let value = match result {
            Ok(v) => v,
            Err(e) => {
                let err = proc.call_error("session/prompt", &e).await;
                if proc.peer.closed() || proc.slot.exited() {
                    self.dead.store(true, Ordering::SeqCst);
                    if let Some(p) = turn.proc.take() {
                        p.kill();
                    }
                }
                return Err(err);
            }
        };
        // Drain updates the agent sent before its prompt response.
        while let Ok(msg) = proc.inbound.try_recv() {
            let proc_ref: &Process = proc;
            self.handle_inbound(&ctx, proc_ref, &mut turn.cache, &mut text, &callbacks, msg)
                .await;
        }
        match events::stop_reason(&value) {
            Stop::Ok => Ok(text),
            Stop::Partial(reason) => {
                warn(&callbacks, &format!("acp: turn stopped: {reason}"));
                Ok(text)
            }
            Stop::Cancelled if ctx_done(&ctx) => Err(self.cancel_error(&ctx)),
            Stop::Cancelled => Err(perr(&self.name, "agent cancelled the turn")),
        }
    }

    async fn close(&self) -> Result<(), ProviderError> {
        self.closing.cancel();
        let mut turn = self.turn.lock().await;
        self.dead.store(true, Ordering::SeqCst);
        if let Some(proc) = turn.proc.take() {
            proc.shutdown(CLOSE_GRACE).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::may_signal_group;

    #[test]
    fn no_group_signal_after_leader_reaped() {
        assert!(may_signal_group(Some(42), false));
        assert!(!may_signal_group(Some(42), true));
        assert!(!may_signal_group(None, false));
        assert!(!may_signal_group(None, true));
    }
}
