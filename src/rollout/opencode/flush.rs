//! Background flushing of an OpenCode transcript.
//!
//! A [`Flusher`] belongs to one session. [`Flusher::schedule`] never blocks and
//! never awaits: it stores the newest snapshot and makes sure one tokio task is
//! working through snapshots (the latest wins, and the flushes of one session are
//! serialized). A flush resolves the OpenCode project, then writes the session
//! through the official API (the default) and, when that fails and the fallback
//! allows it, straight into the database. A failure is reported once per cause
//! and never reaches the run; a flush that failed on every allowed path disables
//! the session's flusher until the session ends.
//!
//! Nothing here may make the run fail: locks tolerate poisoning, the worker's
//! bookkeeping runs from a drop guard (so a panic or a runtime shutdown cannot
//! leave a session "pending" forever), and nothing is printed while a lock is held.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;

use super::api::{ApiClient, check_body_size, find_opencode};
use super::project::{ProjectRef, pick_project};
use super::transcript::Transcript;
use super::{OpenCodeError, in_tested_range};
use crate::config::{OpenCodeConfig, OpenCodeFallback, OpenCodeMode};

/// The timeout of every `opencode` call.
const API_TIMEOUT: Duration = Duration::from_secs(15);

/// Sessions with a flush queued or running, process wide.
static PENDING: AtomicUsize = AtomicUsize::new(0);
static IDLE: Notify = Notify::const_new();

/// Wait until no flush is pending or `timeout` passes, whichever comes first.
pub async fn drain(timeout: Duration) {
    let _ = tokio::time::timeout(timeout, async {
        loop {
            let notified = IDLE.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if PENDING.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    })
    .await;
}

/// Flushes one session's transcripts in the background.
pub struct Flusher {
    shared: Arc<Shared>,
}

struct Shared {
    cfg: OpenCodeConfig,
    cwd: PathBuf,
    session_id: String,
    api_timeout: Duration,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The newest snapshot not yet written.
    pending: Option<Transcript>,
    /// A worker task is alive (queued or flushing).
    running: bool,
    disabled: bool,
    /// Warnings not yet handed out by `take_warning`.
    warnings: Vec<String>,
    /// Causes already warned about (never warned twice).
    warned: HashSet<String>,
    /// The session exists in OpenCode (an earlier flush succeeded).
    written: bool,
    project_id: Option<String>,
    /// The OpenCode version, read at most once.
    version: Option<String>,
    version_checked: bool,
}

/// A failed API write: the error, and whether the earlier copy may be gone.
struct ApiFailure {
    error: OpenCodeError,
    session_missing: bool,
}

/// Why a flush did not complete, and whether the flusher should give up.
struct Failure {
    cause: String,
    disable: bool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Report `cause` once: on stderr now, and as a pending `warning:` line.
    fn note(&self, cause: String) {
        let first = {
            let mut state = self.lock();
            let first = state.warned.insert(cause.clone());
            if first {
                // Queued before anything is printed: a failing stderr must not
                // be able to lose the warning.
                state
                    .warnings
                    .push(format!("warning: OpenCode rollout: {cause}"));
            }
            first
        };
        if first {
            // Printed outside the lock: a blocked or failing stderr must not stall
            // `schedule` or `take_warning` on the run's path.
            eprintln!("opencode rollout: {cause}");
        }
    }
}

impl Flusher {
    pub fn new(cfg: OpenCodeConfig, cwd: PathBuf, session_id: String) -> Self {
        Self {
            shared: Arc::new(Shared {
                cfg,
                cwd,
                session_id,
                api_timeout: API_TIMEOUT,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Override the timeout of every `opencode` call (the default is 15 s); only
    /// meaningful before the first `schedule`.
    pub fn with_api_timeout(mut self, timeout: Duration) -> Self {
        if let Some(shared) = Arc::get_mut(&mut self.shared) {
            shared.api_timeout = timeout;
        }
        self
    }

    /// Queue `snapshot` for writing; a snapshot still waiting is replaced.
    pub fn schedule(&self, snapshot: Transcript) {
        let mut state = self.shared.lock();
        if state.disabled {
            return;
        }
        state.pending = Some(snapshot);
        if state.running {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            state.pending = None;
            drop(state);
            self.shared
                .note("no async runtime to flush the transcript on".to_string());
            return;
        };
        state.running = true;
        PENDING.fetch_add(1, Ordering::SeqCst);
        drop(state);
        // The guard is built here and moved into the task, so even a task that is
        // dropped before its first poll (a runtime shutting down) releases its claim.
        let guard = WorkerGuard {
            shared: self.shared.clone(),
            cleared_running: false,
        };
        handle.spawn(worker(guard));
    }

    /// Warnings recorded since the last call (one `warning:` line each), or `None`.
    pub fn take_warning(&self) -> Option<String> {
        let mut state = self.shared.lock();
        if state.warnings.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut state.warnings).join("\n"))
        }
    }
}

/// Releases a worker's claim on the process-wide pending count when the worker
/// ends for any reason (all snapshots done, a panic, or the runtime dropping the
/// task, even before its first poll).
///
/// `running` is normally cleared by the worker itself, inside the same critical
/// section that saw nothing left to do: `schedule` sets `pending` and reads
/// `running` under that lock, so a snapshot can never be left behind. Only an
/// abnormal end clears it here.
struct WorkerGuard {
    shared: Arc<Shared>,
    cleared_running: bool,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if !self.cleared_running {
            self.shared.lock().running = false;
        }
        PENDING.fetch_sub(1, Ordering::SeqCst);
        IDLE.notify_waiters();
    }
}

async fn worker(mut guard: WorkerGuard) {
    let shared = guard.shared.clone();
    loop {
        let snapshot = {
            let mut state = shared.lock();
            if state.disabled {
                // A flush failed while this snapshot waited: do not retry.
                state.pending = None;
            }
            match state.pending.take() {
                Some(snapshot) => snapshot,
                None => {
                    // Cleared in this critical section (see `WorkerGuard`).
                    state.running = false;
                    guard.cleared_running = true;
                    return;
                }
            }
        };
        if let Err(Failure { cause, disable }) = write_session(&shared, &snapshot).await {
            if disable {
                shared.lock().disabled = true;
            }
            shared.note(cause);
        }
    }
}

async fn write_session(shared: &Arc<Shared>, snapshot: &Transcript) -> Result<(), Failure> {
    if shared.cfg.mode == OpenCodeMode::Api {
        check_version(shared).await;
    }
    let project_id = match resolve_project(shared).await {
        Ok(Some(id)) => id,
        // No project: skip this flush, keep trying on later runs.
        Ok(None) => {
            return Err(Failure {
                cause: format!("no OpenCode project for {}", shared.cwd.display()),
                disable: false,
            });
        }
        Err(cause) => {
            return Err(Failure {
                cause,
                disable: true,
            });
        }
    };
    let body = snapshot.to_import(&shared.session_id, &project_id);
    let cfg = &shared.cfg;
    let fail = |cause: String| Failure {
        cause,
        disable: true,
    };
    match cfg.mode {
        OpenCodeMode::Db => {
            db_write(shared, &body)
                .await
                .map_err(|e| fail(format!("database write failed: {e}")))?;
            shared.lock().written = true;
            Ok(())
        }
        OpenCodeMode::Api => match api_write(shared, &body).await {
            Ok(()) => {
                shared.lock().written = true;
                Ok(())
            }
            Err(failure) if cfg.fallback == OpenCodeFallback::Db && !rejected(&failure.error) => {
                match db_write(shared, &body).await {
                    Ok(()) => {
                        shared.lock().written = true;
                        // The API failed but the session is safe: report it, keep going.
                        shared.note(format!(
                            "the API write failed ({}); wrote the session to the database instead",
                            failure.error
                        ));
                        Ok(())
                    }
                    Err(db_error) => {
                        let missing = failure.session_missing;
                        if missing {
                            shared.lock().written = false;
                        }
                        Err(fail(format!(
                            "the API write failed ({}) and the database fallback failed ({db_error}){}",
                            failure.error,
                            if missing {
                                "; the session is missing from OpenCode"
                            } else {
                                ""
                            }
                        )))
                    }
                }
            }
            Err(failure) => Err(fail(if failure.session_missing {
                format!(
                    "the session is missing from OpenCode: the earlier copy may have been deleted and the import failed ({})",
                    failure.error
                )
            } else {
                format!("the API write failed: {}", failure.error)
            })),
        },
    }
}

/// Read `opencode --version` once and warn once when it is outside the tested
/// range; the API path is still tried (the server validates the request).
async fn check_version(shared: &Arc<Shared>) {
    {
        let mut state = shared.lock();
        if state.version_checked {
            return;
        }
        state.version_checked = true;
    }
    let Ok(client) = api_client(shared) else {
        return;
    };
    if let Ok(version) = client.version().await {
        shared.lock().version = Some(version.clone());
        if !in_tested_range(&version) {
            shared.note(format!(
                "OpenCode {version} is outside the tested range 2.3.x; trying the API anyway"
            ));
        }
    }
}

/// Resolve (and cache) the OpenCode project id of the session's directory.
async fn resolve_project(shared: &Arc<Shared>) -> Result<Option<String>, String> {
    if let Some(id) = shared.lock().project_id.clone() {
        return Ok(Some(id));
    }
    let cfg = &shared.cfg;
    let projects = match cfg.mode {
        OpenCodeMode::Db => db_projects(shared)
            .await
            .map_err(|e| format!("project lookup in the database failed: {e}"))?,
        OpenCodeMode::Api => match api_client(shared) {
            Ok(client) => match client.list_projects().await {
                Ok(projects) => projects,
                Err(api_error) => fallback_projects(shared, api_error).await?,
            },
            Err(api_error) => fallback_projects(shared, api_error).await?,
        },
    };
    let cwd = shared.cwd.clone();
    // Filesystem work (symlinks, the git root): off the async worker thread.
    let found = tokio::task::spawn_blocking(move || {
        let git_root = crate::repo::root(&cwd);
        pick_project(&cwd, git_root.as_deref(), &projects).map(|p| p.id.clone())
    })
    .await
    .map_err(|e| format!("project resolution failed: {e}"))?;
    if let Some(id) = &found {
        shared.lock().project_id = Some(id.clone());
    }
    Ok(found)
}

/// The project list after the API could not provide it: the database when the
/// fallback allows, otherwise the failure.
async fn fallback_projects(
    shared: &Arc<Shared>,
    api_error: OpenCodeError,
) -> Result<Vec<ProjectRef>, String> {
    if shared.cfg.fallback != OpenCodeFallback::Db {
        return Err(format!("project lookup failed: {api_error}"));
    }
    db_projects(shared).await.map_err(|db_error| {
        format!("project lookup failed through the API ({api_error}) and the database ({db_error})")
    })
}

fn api_client(shared: &Shared) -> Result<ApiClient, OpenCodeError> {
    let bin = find_opencode(&shared.cfg).ok_or(OpenCodeError::NotFound)?;
    Ok(ApiClient::new(bin, shared.api_timeout))
}

/// A delete that failed: a timeout or an unreadable failure may still have
/// deleted the session, so it counts as possibly missing (and the next write
/// starts from "nothing exists"); a clear refusal leaves the old copy in place.
fn delete_failure(shared: &Shared, error: OpenCodeError) -> ApiFailure {
    let maybe_deleted = matches!(error, OpenCodeError::Timeout | OpenCodeError::Other(_));
    if maybe_deleted {
        shared.lock().written = false;
    }
    ApiFailure {
        error,
        session_missing: maybe_deleted,
    }
}

/// Write through the API: replace the earlier copy of the session when there is one.
/// A 4xx that says the server judged the body invalid: writing the same body to
/// the database would bypass that check. 401/403/404/408/409/429 are about the
/// request's circumstances, not its content, and still fall back.
fn rejected(error: &OpenCodeError) -> bool {
    matches!(
        error,
        OpenCodeError::Status(400..=499, _)
            if !matches!(error, OpenCodeError::Status(401 | 403 | 404 | 408 | 409 | 429, _))
    )
}

async fn api_write(shared: &Arc<Shared>, body: &Value) -> Result<(), ApiFailure> {
    let plain = |error| ApiFailure {
        error,
        session_missing: false,
    };
    let client = api_client(shared).map_err(plain)?;
    // Before anything is deleted: an import that cannot be sent must not cost the
    // session its earlier copy.
    check_body_size(body).map_err(plain)?;
    let id = shared.session_id.as_str();
    let written = shared.lock().written;
    let mut deleted = false;
    if written {
        match client.delete(id).await {
            Ok(()) | Err(OpenCodeError::Status(404, _)) => deleted = true,
            Err(error) => return Err(delete_failure(shared, error)),
        }
    }
    match client.import(body).await {
        Ok(()) => Ok(()),
        // The first import found the id already present: replace it.
        Err(OpenCodeError::Status(409, _)) if !deleted => {
            match client.delete(id).await {
                Ok(()) | Err(OpenCodeError::Status(404, _)) => {}
                Err(error) => return Err(delete_failure(shared, error)),
            }
            client.import(body).await.map_err(|error| ApiFailure {
                error,
                session_missing: true,
            })
        }
        Err(error) => {
            // A 409 after our own delete says a copy exists, not that it is gone.
            let missing = deleted && !matches!(error, OpenCodeError::Status(409, _));
            if missing {
                shared.lock().written = false;
            }
            Err(ApiFailure {
                error,
                session_missing: missing,
            })
        }
    }
}

#[cfg(unix)]
mod database {
    use super::*;
    use crate::rollout::opencode::db::{DbWriter, default_db_path};

    /// The OpenCode version the database writer needs: the one `check_version`
    /// read (API mode), otherwise the version recorded in the database itself.
    async fn version(
        shared: &Arc<Shared>,
        path: &std::path::Path,
    ) -> Result<String, OpenCodeError> {
        if let Some(version) = shared.lock().version.clone() {
            return Ok(version);
        }
        let path = path.to_path_buf();
        let version = tokio::task::spawn_blocking(move || DbWriter::detect_version(&path))
            .await
            .map_err(|e| OpenCodeError::Other(e.to_string()))??;
        shared.lock().version = Some(version.clone());
        Ok(version)
    }

    fn db_path(shared: &Shared) -> Result<PathBuf, OpenCodeError> {
        shared
            .cfg
            .db_path
            .clone()
            .or_else(default_db_path)
            .ok_or(OpenCodeError::NotFound)
    }

    pub(super) async fn db_projects(
        shared: &Arc<Shared>,
    ) -> Result<Vec<ProjectRef>, OpenCodeError> {
        let path = db_path(shared)?;
        let version = version(shared, &path).await?;
        tokio::task::spawn_blocking(move || DbWriter::open(&path, &version)?.list_projects())
            .await
            .map_err(|e| OpenCodeError::Other(e.to_string()))?
    }

    pub(super) async fn db_write(shared: &Arc<Shared>, body: &Value) -> Result<(), OpenCodeError> {
        let path = db_path(shared)?;
        let version = version(shared, &path).await?;
        let body = body.clone();
        tokio::task::spawn_blocking(move || DbWriter::open(&path, &version)?.replace_session(&body))
            .await
            .map_err(|e| OpenCodeError::Other(e.to_string()))?
    }
}

#[cfg(unix)]
use database::{db_projects, db_write};

#[cfg(not(unix))]
async fn db_projects(_: &Arc<Shared>) -> Result<Vec<ProjectRef>, OpenCodeError> {
    Err(unsupported())
}

#[cfg(not(unix))]
async fn db_write(_: &Arc<Shared>, _: &Value) -> Result<(), OpenCodeError> {
    Err(unsupported())
}

#[cfg(not(unix))]
fn unsupported() -> OpenCodeError {
    OpenCodeError::Other("the OpenCode database fallback is not supported on this platform".into())
}
