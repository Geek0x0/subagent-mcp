//! In-memory session manager (Go `internal/agent/session.go`, v0.13.1).
//!
//! A [`SessionManager`] owns a map of id -> [`Session`]. Each session holds an
//! `Arc<tokio::sync::Mutex<SessionState>>`; acquiring a session takes that lock
//! and yields a [`SessionLease`] whose `DerefMut` is how the runner mutates
//! messages, history bytes, usage, and rollout. Eviction (24 h idle TTL and a
//! 256-session cap) runs under the manager lock and only ever removes *idle*
//! sessions (ones whose state lock can be taken with `try_lock_owned`), so a
//! busy session is never evicted. No await happens while the manager lock is
//! held.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::prompt::DEFAULT_SYSTEM_PROMPT;
use crate::policy::{ApprovalPolicy, Sandbox};
use crate::provider::{Message, Provider, Thread, Usage};
use crate::rollout::RolloutRecorder;
use crate::sandbox::BoundDirectory;
use crate::util::fs::same_file;

/// Go `DefaultMaxTurns`.
pub const DEFAULT_MAX_TURNS: usize = 50;
/// Go `sessionIdleTTL`.
const SESSION_IDLE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Go `maxSessions`.
const MAX_SESSIONS: usize = 256;
/// Go default model in `Manager.Create`.
const DEFAULT_MODEL: &str = "deepseek-v4-pro";

/// Options used to create a [`Session`] (Go `agent.Options`).
///
/// All fields are public so the MCP handler can assemble them from a request;
/// [`SessionManager::create`] applies the Go defaults for model, effort, max
/// turns, and system prompt.
pub struct SessionOptions {
    pub provider: Arc<dyn Provider>,
    pub thread: Option<Arc<dyn Thread>>,
    pub model: String,
    pub reasoning_effort: String,
    pub effort_sent: Option<String>,
    pub cwd: PathBuf,
    pub sandbox: Sandbox,
    pub approval: ApprovalPolicy,
    pub system_prompt: String,
    pub max_turns: usize,
    pub max_nudges: usize,
    pub writable_roots: Vec<PathBuf>,
    pub max_history_bytes: u64,
    pub max_total_tokens: u64,
    pub run_deadline: Option<Duration>,
}

/// A created session. `state` is behind a tokio mutex guarded by leases.
pub struct Session {
    pub id: String,
    pub options: SessionOptions,
    pub(crate) state: Arc<Mutex<SessionState>>,
}

/// Mutable per-session state. All fields are `pub` so the runner can mutate
/// them through a [`SessionLease`]'s `DerefMut`.
pub struct SessionState {
    pub messages: Vec<Message>,
    pub history_bytes: u64,
    pub total_usage: Usage,
    /// Attached by Task 24, not here.
    pub rollout: Option<RolloutRecorder>,
    pub last_used: Instant,
    pub bound_cwd: Option<BoundDirectory>,
    pub bind_err: Option<String>,
    /// Effective, `os.SameFile`-deduplicated workspace-write roots (Go
    /// `rootPaths`). Computed and bound during creation; consumed by Task 23B
    /// to build the path-based Landlock launcher. This is an intentional
    /// extension of the listed `SessionState` fields needed for the Task 20/21
    /// handoff ("Task 22 owns the real default workspace-write roots").
    pub root_paths: Vec<PathBuf>,
}

/// The manager's session table.
pub struct SessionManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Go `Manager.Create`. Normalizes defaults, binds directories, sweeps idle
    /// sessions, and inserts the new session. Evicted provider threads are
    /// closed on spawned tasks after the manager lock is released.
    pub async fn create(&self, mut opts: SessionOptions) -> Result<Arc<Session>, SessionError> {
        if opts.model.is_empty() {
            opts.model = DEFAULT_MODEL.to_string();
        }
        if opts.reasoning_effort.is_empty() {
            opts.reasoning_effort = "high".to_string();
        }
        if opts.max_turns == 0 {
            opts.max_turns = DEFAULT_MAX_TURNS;
        }
        if opts.system_prompt.is_empty() && opts.thread.is_none() {
            opts.system_prompt = DEFAULT_SYSTEM_PROMPT.to_string();
        }
        if opts.effort_sent.as_deref().unwrap_or("").is_empty() {
            opts.effort_sent = Some(opts.reasoning_effort.clone());
        }

        let (bound_cwd, bind_err, root_paths) = bind_directories(&opts);

        let session = Arc::new(Session {
            id: Uuid::new_v4().to_string(),
            options: opts,
            state: Arc::new(Mutex::new(SessionState {
                messages: Vec::new(),
                history_bytes: 0,
                total_usage: Usage::default(),
                rollout: None,
                last_used: Instant::now(),
                bound_cwd,
                bind_err,
                root_paths,
            })),
        });

        let evicted_threads: Vec<Arc<dyn Thread>> = {
            let mut map = self.sessions.lock().await;
            let threads = evict_locked(&mut map);
            map.insert(session.id.clone(), session.clone());
            threads
        };

        // A `Thread::close` can block on a backend RPC; never stall the caller
        // of `create` behind an eviction sweep, and never hold the manager lock
        // while awaiting.
        for thread in evicted_threads {
            tokio::spawn(async move {
                let _ = thread.close().await;
            });
        }

        Ok(session)
    }

    /// Go `Manager.Acquire`: lookup and `try_lock_owned` happen under the
    /// manager lock, so an evicted session is never handed out and a session
    /// cannot be evicted between lookup and use.
    pub async fn acquire(&self, id: &str) -> Result<SessionLease, SessionError> {
        let lease = {
            let map = self.sessions.lock().await;
            let session = map.get(id).cloned().ok_or(SessionError::UnknownSession)?;
            let guard = session
                .state
                .clone()
                .try_lock_owned()
                .map_err(|_| SessionError::Busy)?;
            SessionLease { session, guard }
        };
        Ok(lease)
    }

    /// Go `Manager.Close`: signal shutdown, drop every session from the table,
    /// release bound directories, and close provider threads.
    pub async fn close(&self, cancel: &CancellationToken) {
        cancel.cancel();
        let sessions: Vec<Arc<Session>> = {
            let mut map = self.sessions.lock().await;
            map.drain().map(|(_, s)| s).collect()
        };
        for session in sessions {
            // Best-effort bound-directory release; a busy session keeps its
            // fd until its lease is dropped (RAII), which is still released.
            if let Ok(mut guard) = session.state.clone().try_lock_owned() {
                guard.bound_cwd = None;
                guard.bind_err = Some("session is closed".to_string());
                guard.root_paths.clear();
            }
            if let Some(thread) = &session.options.thread {
                let _ = thread.close().await;
            }
        }
    }
}

/// Go `Manager.evictLocked` + `evictIfIdleLocked`, adapted to owned tokio
/// guards. `m.mu` (the manager lock) is held by the caller throughout; only
/// sessions that can be `try_lock_owned`'d (idle) are ever removed.
fn evict_locked(map: &mut HashMap<String, Arc<Session>>) -> Vec<Arc<dyn Thread>> {
    let now = Instant::now();
    let mut evicted_threads = Vec::new();

    // Phase 1: idle sessions unused for longer than 24 h.
    let ids: Vec<String> = map.keys().cloned().collect();
    for id in &ids {
        let Some(session) = map.get(id).cloned() else {
            continue;
        };
        let Ok(guard) = session.state.clone().try_lock_owned() else {
            continue; // busy -> never evict
        };
        let expired = now.saturating_duration_since(guard.last_used) > SESSION_IDLE_TTL;
        if expired {
            map.remove(id);
            drop(guard);
            if let Some(thread) = &session.options.thread {
                evicted_threads.push(thread.clone());
            }
        }
        // Not expired: `guard` drops here, releasing the session without ever
        // having made a concurrent acquirer see it as busy (the manager lock
        // serializes acquirers anyway).
    }

    // Phase 2: trim to leave room for one new session (cap 256). Only idle
    // sessions are candidates; busy ones cannot be evicted.
    if map.len() >= MAX_SESSIONS {
        let ids: Vec<String> = map.keys().cloned().collect();
        let mut candidates: Vec<(String, Instant, OwnedMutexGuard<SessionState>, Arc<Session>)> =
            Vec::new();
        for id in ids {
            let session = match map.get(&id) {
                Some(s) => s.clone(),
                None => continue,
            };
            if let Ok(guard) = session.state.clone().try_lock_owned() {
                let last_used = guard.last_used;
                candidates.push((id, last_used, guard, session));
            }
        }
        candidates.sort_by_key(|c| c.1); // oldest first
        for (id, _last_used, guard, session) in candidates {
            if map.len() < MAX_SESSIONS {
                drop(guard);
                break;
            }
            map.remove(&id);
            drop(guard);
            if let Some(thread) = &session.options.thread {
                evicted_threads.push(thread.clone());
            }
        }
    }

    evicted_threads
}

/// Go `Session.bindDirectories` (the Task 20/21 handoff).
///
/// Skips entirely for Codex-style sessions (`thread` present) or an empty cwd.
/// Otherwise binds cwd into `bound_cwd` (via `sandbox::BoundDirectory`), and —
/// only for `workspace-write` — computes the effective default writable roots
/// (`cwd`, `/tmp`, an existing `$TMPDIR`, configured roots), validates each by
/// binding it, deduplicates them with `os.SameFile` (`util::fs::same_file`), and
/// stores the paths in `root_paths`. A bind failure fails closed: it clears the
/// bound cwd and records `bind_err`.
fn bind_directories(
    opts: &SessionOptions,
) -> (Option<BoundDirectory>, Option<String>, Vec<PathBuf>) {
    if opts.thread.is_some() || opts.cwd.as_os_str().is_empty() {
        return (None, None, Vec::new());
    }

    let bound_cwd = match BoundDirectory::open(&opts.cwd) {
        Ok(dir) => dir,
        Err(e) => {
            return (
                None,
                Some(format!("bind working directory {:?}: {e}", opts.cwd)),
                Vec::new(),
            );
        }
    };

    if opts.sandbox != Sandbox::WorkspaceWrite {
        return (Some(bound_cwd), None, Vec::new());
    }

    let candidates = candidate_paths(&opts.cwd, &opts.writable_roots);
    let mut root_paths: Vec<PathBuf> = Vec::new();
    for path in &candidates {
        // Go reuses `boundCwd` for the cwd path (string equality); bind every
        // other root to validate it exists and is a directory.
        if path != &opts.cwd
            && let Err(e) = BoundDirectory::open(path)
        {
            drop(bound_cwd);
            return (
                None,
                Some(format!("bind writable root {:?}: {e}", path)),
                Vec::new(),
            );
        }
        let duplicate = root_paths
            .iter()
            .any(|existing| same_file(path, existing).unwrap_or(false));
        if !duplicate {
            root_paths.push(path.clone());
        }
    }

    (Some(bound_cwd), None, root_paths)
}

/// Go's default writable-root list: `cwd`, `/tmp`, an existing `$TMPDIR`, then
/// configured roots. Returned in first-seen order before dedup.
fn candidate_paths(cwd: &Path, configured: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.push(cwd.to_path_buf());
    paths.push(PathBuf::from("/tmp"));
    if let Ok(tmp) = std::env::var("TMPDIR")
        && !tmp.is_empty()
        && Path::new(&tmp).is_dir()
    {
        paths.push(PathBuf::from(&tmp));
    }
    paths.extend(configured.iter().cloned());
    paths
}

/// Effective, `os.SameFile`-deduplicated workspace-write roots (Go
/// `rootPaths`). Exposed so Task 23B can build the Landlock launcher from the
/// same computation; the session stores the result in `SessionState::root_paths`.
pub fn effective_writable_roots(cwd: &Path, configured: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in candidate_paths(cwd, configured) {
        let duplicate = out
            .iter()
            .any(|existing| same_file(&path, existing).unwrap_or(false));
        if !duplicate {
            out.push(path);
        }
    }
    out
}

/// A held session lock. `Deref`/`DerefMut` expose [`SessionState`]; dropping
/// the lease releases the guard (Go `Manager.Release`).
pub struct SessionLease {
    session: Arc<Session>,
    guard: OwnedMutexGuard<SessionState>,
}

impl Deref for SessionLease {
    type Target = SessionState;

    fn deref(&self) -> &SessionState {
        &self.guard
    }
}

impl DerefMut for SessionLease {
    fn deref_mut(&mut self) -> &mut SessionState {
        &mut self.guard
    }
}

impl std::fmt::Debug for SessionLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLease")
            .field("id", &self.session.id)
            .finish_non_exhaustive()
    }
}

impl SessionLease {
    pub fn session(&self) -> &Session {
        self.session.as_ref()
    }

    pub fn id(&self) -> &str {
        &self.session.id
    }

    pub fn options(&self) -> &SessionOptions {
        &self.session.options
    }
}

/// Progress/notification sink used by the runner (Task 23A).
pub trait Emitter: Send + Sync {
    fn emit(&self, thread_id: &str, event: serde_json::Value);
}

/// Runner error type (Task 23A).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentError {
    pub message: String,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AgentError {}

/// Errors returned by [`SessionManager::acquire`] (Go `ErrUnknownSession`,
/// `ErrBusy`). Display text matches Go's error strings verbatim.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionError {
    #[default]
    UnknownSession,
    Busy,
}

impl SessionError {
    pub fn message(&self) -> &'static str {
        match self {
            SessionError::UnknownSession => "unknown session",
            SessionError::Busy => "thread is busy",
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for SessionError {}
