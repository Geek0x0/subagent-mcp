//! Per-command Codex app-server child pool.
//!
//! Ports Go `internal/provider/codexappserver/pool.go` and `process.go`. One
//! live child is shared by every thread built for the same command. A gate
//! serializes initialization so concurrent callers wait for one handshake, and
//! a waiting caller can cancel without waiting for another caller's handshake.
//! Every successful dial increments the pool generation, so a thread can tell
//! that its connection died and a replacement was started.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, Command};
use tokio::task::JoinHandle;

use crate::provider::codex::client::{Client, ClientError, ctx_error, wait_ctx};
use crate::util::run::RunContext;

/// How long one handshake may take, matching Go's `context.WithTimeout`.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a graceful close waits after closing stdin.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long stderr draining may outlive the child (`cmd.WaitDelay`).
pub const STDERR_WAIT_DELAY: Duration = Duration::from_secs(1);

/// Bytes of stderr retained for a crash report.
pub const STDERR_TAIL_SIZE: usize = 4 * 1024;

const DEFAULT_COMMAND: &str = "codex";

/// A future returned by a [`Dialer`].
pub type DialFuture = Pin<Box<dyn Future<Output = Result<Dialed, ClientError>> + Send>>;

/// Starts one app-server connection. The pool applies the handshake timeout and
/// cancellation; the dialer only establishes the transport.
pub type Dialer = Arc<dyn Fn() -> DialFuture + Send + Sync>;

/// Releases one dialed connection. Both an async graceful path and a
/// best-effort synchronous path exist because process-wide shutdown runs from a
/// synchronous hook.
#[async_trait::async_trait]
pub trait Closer: Send + Sync {
    /// Close stdin, wait up to [`CLOSE_TIMEOUT`], then kill the direct child.
    async fn close(&self);

    /// Best-effort shutdown for the synchronous process-exit hook.
    fn close_blocking(&self);
}

/// One freshly dialed connection, before the handshake completes.
pub struct Dialed {
    pub client: Arc<Client>,
    pub closer: Arc<dyn Closer>,
    pub stderr_tail: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}

/// A live pooled generation.
pub struct Conn {
    pub client: Arc<Client>,
    pub generation: u64,
    pub user_agent: String,
    closer: Arc<dyn Closer>,
    stderr_tail: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    closed: AtomicBool,
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("generation", &self.generation)
            .field("user_agent", &self.user_agent)
            .field("done", &self.client.is_done())
            .finish_non_exhaustive()
    }
}

impl Conn {
    /// Close the direct child exactly once.
    pub async fn close(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.closer.close().await;
        }
    }

    /// Best-effort synchronous close for the process-exit hook.
    pub fn close_blocking(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.closer.close_blocking();
        }
    }

    /// Translate a call failure into the error a caller should see.
    ///
    /// A transport that is already dead, or an EOF/closed-pipe write, is
    /// reported as `codex app-server exited: <stderr tail>` so the operator sees
    /// why the child went away. Other errors (a JSON-RPC error, cancellation)
    /// pass through unchanged.
    pub async fn failure(&self, error: ClientError) -> ClientError {
        if self.client.error().is_none() && !error.is_eof() && !error.is_closed_pipe() {
            return error;
        }
        self.close().await;
        let tail = self
            .stderr_tail
            .as_ref()
            .map(|tail| tail())
            .unwrap_or_default();
        ClientError::message(format!("codex app-server exited: {tail}"))
    }
}

#[derive(Default)]
struct PoolState {
    current: Option<Arc<Conn>>,
    generation: u64,
}

/// Serializes initialization and shares one live generation among threads.
pub struct Pool {
    dial: Dialer,
    gate: tokio::sync::Mutex<()>,
    state: Mutex<PoolState>,
}

impl Pool {
    /// Build a pool over an explicit dialer (the seam the transport tests use).
    pub fn new(dial: Dialer) -> Self {
        Self {
            dial,
            gate: tokio::sync::Mutex::new(()),
            state: Mutex::new(PoolState::default()),
        }
    }

    /// Return the process-wide pool for `command`, creating it on first use.
    /// An empty command means `codex`, matching Go `poolForCommand`.
    pub fn for_command(command: &str) -> Arc<Self> {
        let key = pool_key(command);
        let mut pools = POOLS.lock().unwrap();
        if let Some(pool) = pools.get(key) {
            return pool.clone();
        }
        let pool = Arc::new(Self::new(process_dialer(key.to_string())));
        pools.insert(key.to_string(), pool.clone());
        pool
    }

    /// Acquire the live connection, dialing and handshaking when there is none.
    ///
    /// A waiting caller can cancel through `ctx` without waiting for another
    /// caller's handshake. A dead connection is closed and replaced, bumping the
    /// generation.
    pub async fn get(&self, ctx: &RunContext) -> Result<Arc<Conn>, ClientError> {
        let _gate = tokio::select! {
            guard = self.gate.lock() => guard,
            error = wait_ctx(ctx) => return Err(error),
        };
        if let Some(error) = ctx_error(ctx) {
            return Err(error);
        }
        let current = self.state.lock().unwrap().current.clone();
        if let Some(current) = current {
            if current.client.is_done() {
                self.state.lock().unwrap().current = None;
                current.close().await;
            } else {
                return Ok(current);
            }
        }

        let deadline = tokio::time::Instant::now() + DIAL_TIMEOUT;
        let dialed = (self.dial)().await?;
        let mut conn = Arc::new(Conn {
            client: dialed.client,
            generation: 0,
            user_agent: String::new(),
            closer: dialed.closer,
            stderr_tail: dialed.stderr_tail,
            closed: AtomicBool::new(false),
        });
        if let Some(error) = ctx_error(ctx) {
            conn.close().await;
            return Err(error);
        }

        let initialize = async {
            let initialized: InitializeResult = conn
                .client
                .call_into(
                    ctx,
                    "initialize",
                    Some(json!({
                        "clientInfo": {
                            "name": "subagent-mcp",
                            "version": client_version(),
                        },
                    })),
                )
                .await?;
            conn.client.notify("initialized", None).await?;
            Ok::<String, ClientError>(initialized.user_agent)
        };
        let deadline = ctx
            .deadline
            .map_or(deadline, |context_deadline| context_deadline.min(deadline));
        let user_agent = match tokio::time::timeout_at(deadline, initialize).await {
            Ok(Ok(user_agent)) => user_agent,
            Ok(Err(error)) => {
                let error = conn.failure(error).await;
                conn.close().await;
                return Err(error);
            }
            Err(_) => {
                conn.close().await;
                return Err(ClientError::DeadlineExceeded);
            }
        };

        let generation = {
            let mut state = self.state.lock().unwrap();
            state.generation += 1;
            state.generation
        };
        {
            let conn = Arc::get_mut(&mut conn).expect("connection is not shared before publish");
            conn.generation = generation;
            conn.user_agent = user_agent;
        }
        self.state.lock().unwrap().current = Some(conn.clone());
        Ok(conn)
    }

    /// The most recently allocated generation (`0` before the first dial).
    pub fn generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }

    /// The live connection, if one is published.
    pub fn current(&self) -> Option<Arc<Conn>> {
        self.state.lock().unwrap().current.clone()
    }

    /// Close the live connection, waiting for an in-flight handshake to finish
    /// first. The gate is what prevents a concurrent dial's handshake from being
    /// closed underneath it.
    pub async fn close(&self) {
        let _gate = self.gate.lock().await;
        let current = self.state.lock().unwrap().current.take();
        if let Some(current) = current {
            current.close().await;
        }
    }

    /// Best-effort synchronous close for the process-exit hook.
    ///
    /// This cannot await an in-flight handshake. A dial racing process exit
    /// publishes its connection after this returns; the process is exiting, so
    /// the child is torn down with it. The async [`Pool::close`] is the exact
    /// gate-serialized path.
    pub fn close_blocking(&self) {
        let current = self.state.lock().unwrap().current.take();
        if let Some(current) = current {
            current.close_blocking();
        }
    }
}

#[derive(Deserialize)]
struct InitializeResult {
    #[serde(rename = "userAgent", default)]
    user_agent: String,
}

/// The version reported in `clientInfo`.
///
/// Go reports the module version, or `dev` for a `(devel)` build. The Rust
/// binary always has a package version, so it reports that.
pub fn client_version() -> &'static str {
    crate::VERSION
}

// --- Process-wide registry ------------------------------------------------

static POOLS: LazyLock<Mutex<HashMap<String, Arc<Pool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pool_key(command: &str) -> &str {
    if command.is_empty() {
        DEFAULT_COMMAND
    } else {
        command
    }
}

/// Close every pooled app-server child.
///
/// A child outlives the call that dialed it, so without this it would keep
/// running whatever turn it was given until its stdin closed at process death.
/// Registered as a provider shutdown hook, so the server runs it once on the
/// way out.
pub fn shutdown() {
    let pools: Vec<Arc<Pool>> = {
        let pools = POOLS.lock().unwrap();
        pools.values().cloned().collect()
    };
    for pool in pools {
        pool.close_blocking();
    }
}

/// Install a pool in the process-wide registry. Test seam for the shutdown
/// path, which otherwise needs a real `codex` binary.
#[doc(hidden)]
pub fn install_pool(command: &str, pool: Arc<Pool>) {
    POOLS
        .lock()
        .unwrap()
        .insert(pool_key(command).to_string(), pool);
}

/// Remove a pool installed by [`install_pool`].
#[doc(hidden)]
pub fn remove_pool(command: &str) {
    POOLS.lock().unwrap().remove(pool_key(command));
}

// --- Process dialer -------------------------------------------------------

fn process_dialer(command: String) -> Dialer {
    Arc::new(move || {
        let command = command.clone();
        Box::pin(async move { start_process(&command).await })
    })
}

async fn start_process(command: &str) -> Result<Dialed, ClientError> {
    let mut cmd = Command::new(command);
    cmd.arg("app-server");
    cmd.env_clear();
    cmd.envs(crate::tools::shell::scrubbed_env());
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // The pool owns the explicit kill path; this only prevents a leaked child
    // if a connection is dropped without going through `Closer`.
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|error| {
        ClientError::context("start codex app-server", ClientError::from(error))
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ClientError::message("codex app-server stdin: child has no piped stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| {
        ClientError::message("codex app-server stdout: child has no piped stdout")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        ClientError::message("codex app-server stderr: child has no piped stderr")
    })?;

    let tail = Arc::new(Mutex::new(StderrTail::default()));
    let stderr_task = tokio::spawn(drain_stderr(stderr, tail.clone()));
    let client = Client::new(stdout, stdin);
    let closer = Arc::new(ProcessCloser {
        inner: tokio::sync::Mutex::new(Some(ProcessInner {
            child,
            stderr_task,
            client: client.clone(),
        })),
    });
    let stderr_tail = {
        let tail = tail.clone();
        Arc::new(move || tail.lock().unwrap().string())
    };
    Ok(Dialed {
        client,
        closer,
        stderr_tail: Some(stderr_tail),
    })
}

struct ProcessInner {
    child: Child,
    stderr_task: JoinHandle<()>,
    client: Arc<Client>,
}

impl ProcessInner {
    async fn close(mut self) {
        // Closing stdin lets the app-server exit on its own. `close_writer` does
        // not wait for an in-flight write, so a wedged child cannot delay the
        // timer below; it falls through to the kill path.
        self.client.close_writer().await;
        if tokio::time::timeout(CLOSE_TIMEOUT, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = tokio::time::timeout(STDERR_WAIT_DELAY, self.child.wait()).await;
            // Release the reader even if a descendant retained stdout.
            self.client.abort_reader();
        }
        // WaitDelay: bound stderr draining if a descendant inherited the pipe.
        if tokio::time::timeout(STDERR_WAIT_DELAY, &mut self.stderr_task)
            .await
            .is_err()
        {
            self.stderr_task.abort();
        }
    }
}

struct ProcessCloser {
    inner: tokio::sync::Mutex<Option<ProcessInner>>,
}

#[async_trait::async_trait]
impl Closer for ProcessCloser {
    async fn close(&self) {
        let inner = self.inner.lock().await.take();
        if let Some(inner) = inner {
            inner.close().await;
        }
    }

    fn close_blocking(&self) {
        let Ok(mut inner) = self.inner.try_lock() else {
            return;
        };
        let Some(mut inner) = inner.take() else {
            return;
        };
        inner.client.abort_reader();
        let _ = inner.child.start_kill();
        inner.stderr_task.abort();
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

/// Retains only the last [`STDERR_TAIL_SIZE`] bytes, including across partial
/// writes (Go `stderrBuffer`).
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
