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
    /// The held working-directory handle (Go `boundCwd`). Shared with
    /// `bound_roots[0]` for workspace-write sessions so cwd is opened once.
    pub bound_cwd: Option<Arc<BoundDirectory>>,
    /// Held writable-root directory handles (Go `boundRoots`). Each effective
    /// root is opened ONCE at creation and kept open, so Task 23B can pass the
    /// fds to `--rw-fd` instead of re-stat'ing mutable paths (no TOCTOU).
    /// `bound_roots` is 1:1 with `root_paths`; for workspace-write sessions
    /// `bound_roots[0]` is the cwd, reused from `bound_cwd` (no second open,
    /// no double-close). This and `bound_cwd` are `Arc` because
    /// `BoundDirectory` is not `Clone` and Go reuses the same `boundCwd`
    /// pointer inside `boundRoots[0]`.
    pub bound_roots: Vec<Arc<BoundDirectory>>,
    pub bind_err: Option<String>,
    /// Effective, deduplicated workspace-write root display paths (Go
    /// `rootPaths`), 1:1 with `bound_roots`. For Task 23B: prefer `bound_roots`
    /// fds for `--rw-fd`; use `root_paths` only for the path-based `--rw`
    /// fallback and display.
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

        let bound = bind_directories(&opts);

        let session = Arc::new(Session {
            id: Uuid::new_v4().to_string(),
            options: opts,
            state: Arc::new(Mutex::new(SessionState {
                messages: Vec::new(),
                history_bytes: 0,
                total_usage: Usage::default(),
                rollout: None,
                last_used: Instant::now(),
                bound_cwd: bound.bound_cwd,
                bound_roots: bound.bound_roots,
                bind_err: bound.bind_err,
                root_paths: bound.root_paths,
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
                // Go `closeDirectories`: drop every held handle (cwd + roots).
                // `bound_roots[0]` shares the cwd `Arc`, so clearing both
                // fields closes each fd exactly once via RAII (no double-close).
                guard.bound_cwd = None;
                guard.bound_roots.clear();
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
///
/// False-busy invariant (Go `TestManagerSweepDoesNotCauseFalseBusy`): this
/// sweep runs ONLY while the caller holds the manager lock, and every
/// `try_lock_owned` probe guard is dropped before `evict_locked` returns —
/// hence before that manager lock is released. `acquire` takes the manager lock
/// before its own `try_lock_owned`, so the two are serialized: an acquirer can
/// never observe a probe guard the sweep left behind, because by the time the
/// acquirer gets the manager lock the sweep has already dropped all of them.
/// A busy session (lease held) simply fails the probe and is never evicted.
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

/// Result of [`bind_directories`]: the held cwd, the held writable roots, their
/// matching display paths, and any bind error. On any failure everything is
/// dropped (fail closed), mirroring Go `closeDirectories`.
struct BoundState {
    bound_cwd: Option<Arc<BoundDirectory>>,
    bound_roots: Vec<Arc<BoundDirectory>>,
    root_paths: Vec<PathBuf>,
    bind_err: Option<String>,
}

impl BoundState {
    fn empty() -> Self {
        Self {
            bound_cwd: None,
            bound_roots: Vec::new(),
            root_paths: Vec::new(),
            bind_err: None,
        }
    }

    fn error(message: String) -> Self {
        Self {
            bind_err: Some(message),
            ..Self::empty()
        }
    }
}

/// Inode identity (device, inode) of a HELD `BoundDirectory`, read from the
/// held descriptor — never from the original path name. On Linux
/// [`BoundDirectory::path`] is `/proc/self/fd/<fd>`, a magic symlink that
/// resolves to the inode the fd points at (verified: `stat /proc/self/fd/N`
/// equals `fstat(N)` for `O_PATH` fds, before AND after a rename/symlink swap).
/// On non-Linux Unix the open `File` handle is stat'd directly. This is Go's
/// `dir.File().Stat()` and is TOCTOU-free.
#[cfg(unix)]
fn held_identity(dir: &BoundDirectory) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    #[cfg(target_os = "linux")]
    let meta = std::fs::metadata(dir.path())?;
    #[cfg(all(unix, not(target_os = "linux")))]
    let meta = dir.file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

/// Whether `new` names the same inode as an already-accepted HELD root. All
/// comparisons use held descriptors, never a re-stat of a dropped path's name,
/// so a symlink swap between validation and exec cannot change a dedup
/// decision (Go `os.SameFile(info, boundInfo)` on held `Stat()` results).
#[cfg(unix)]
fn held_duplicate(new: &BoundDirectory, accepted: &[Arc<BoundDirectory>]) -> bool {
    let Ok(id) = held_identity(new) else {
        return false;
    };
    accepted.iter().any(|e| held_identity(e).ok() == Some(id))
}

/// Best-effort non-Unix fallback (no kernel sandbox off Linux): compare the
/// held display paths with `same_file`.
#[cfg(not(unix))]
fn held_duplicate(new: &BoundDirectory, accepted: &[Arc<BoundDirectory>]) -> bool {
    accepted
        .iter()
        .any(|e| same_file(&e.path(), &new.path()).unwrap_or(false))
}

/// Go `Session.bindDirectories` (the Task 20/21 handoff), now with HELD fds.
///
/// Skips entirely for Codex-style sessions (`thread` present) or an empty cwd.
/// Otherwise opens cwd ONCE into `bound_cwd`. For `workspace-write` it builds
/// the effective default roots (`cwd`, `/tmp`, existing `$TMPDIR`, configured
/// roots) and opens each ONCE into a held `BoundDirectory`. The cwd path reuses
/// the already-held `bound_cwd` (Go `dir = s.boundCwd`) — no second open, no
/// double-close. Roots are deduplicated by the HELD inodes (not path re-stats),
/// and `root_paths` stays 1:1 with `bound_roots`. A bind failure fails closed:
/// every held handle is dropped and `bind_err` is set.
///
/// Task 23B contract: pass `bound_roots` fds as `--rw-fd` (kernel-enforced,
/// immune to path swaps) and `bound_cwd`'s fd as `--cwd-fd`; use `root_paths`
/// only for the path-based `--rw` fallback and display.
fn bind_directories(opts: &SessionOptions) -> BoundState {
    if opts.thread.is_some() || opts.cwd.as_os_str().is_empty() {
        return BoundState::empty();
    }

    let cwd_dir = match BoundDirectory::open(&opts.cwd) {
        Ok(dir) => Arc::new(dir),
        Err(e) => {
            return BoundState::error(format!("bind working directory {:?}: {e}", opts.cwd));
        }
    };

    if opts.sandbox != Sandbox::WorkspaceWrite {
        // Go returns early: cwd is bound, but no writable roots are collected.
        return BoundState {
            bound_cwd: Some(cwd_dir),
            ..BoundState::empty()
        };
    }

    let candidates = candidate_paths(&opts.cwd, &opts.writable_roots);
    let mut bound_roots: Vec<Arc<BoundDirectory>> = Vec::new();
    let mut root_paths: Vec<PathBuf> = Vec::new();
    for path in &candidates {
        // Go: `dir := s.boundCwd` when `path == s.cwd` (reuse the held cwd),
        // otherwise `BindDirectory(path)` once. Either way `dir` is HELD.
        let dir: Arc<BoundDirectory> = if path == &opts.cwd {
            cwd_dir.clone()
        } else {
            match BoundDirectory::open(path) {
                Ok(dir) => Arc::new(dir),
                Err(e) => {
                    return BoundState::error(format!("bind writable root {:?}: {e}", path));
                }
            }
        };

        // Go `dir.File().Stat()` — validate the held handle is inspectable.
        #[cfg(unix)]
        if let Err(e) = held_identity(&dir) {
            return BoundState::error(format!("inspect writable root {:?}: {e}", path));
        }

        // Dedup by HELD inode identity. A duplicate drops only this local
        // handle: for a cwd reuse it is just an `Arc` clone, so `bound_cwd`
        // (and `bound_roots[0]`) is never closed out from under us.
        if held_duplicate(&dir, &bound_roots) {
            continue;
        }
        bound_roots.push(dir);
        root_paths.push(path.clone());
    }

    BoundState {
        bound_cwd: Some(cwd_dir),
        bound_roots,
        root_paths,
        bind_err: None,
    }
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

/// Effective, deduplicated workspace-write root display paths (Go
/// `rootPaths`). This is the pure path-based helper for the `--rw` fallback and
/// display only: it dedups by path `same_file`. The authoritative, TOCTOU-free
/// computation is [`bind_directories`], which HOLDS each root in
/// `SessionState::bound_roots` and dedups by held inode identity. At creation
/// time (no swaps) both produce the same list; Task 23B must prefer the held
/// `bound_roots` fds for `--rw-fd` over these paths.
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
