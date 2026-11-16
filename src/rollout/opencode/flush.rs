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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;

use super::OpenCodeError;
use super::api::{ApiClient, find_opencode};
use super::project::{ProjectRef, pick_project};
use super::transcript::Transcript;
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
    version: Option<String>,
}

/// A failed API write: the error, and whether the old copy was already deleted.
struct ApiFailure {
    error: OpenCodeError,
    session_missing: bool,
}

impl Flusher {
    pub fn new(cfg: OpenCodeConfig, cwd: PathBuf, session_id: String) -> Self {
        Self {
            shared: Arc::new(Shared {
                cfg,
                cwd,
                session_id,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Queue `snapshot` for writing; a snapshot still waiting is replaced.
    pub fn schedule(&self, snapshot: Transcript) {
        let mut state = self.shared.state.lock().unwrap();
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
        handle.spawn(worker(self.shared.clone()));
    }

    /// Warnings recorded since the last call (one `warning:` line each), or `None`.
    pub fn take_warning(&self) -> Option<String> {
        let mut state = self.shared.state.lock().unwrap();
        if state.warnings.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut state.warnings).join("\n"))
        }
    }
}

async fn worker(shared: Arc<Shared>) {
    loop {
        let snapshot = {
            let mut state = shared.state.lock().unwrap();
            match state.pending.take() {
                Some(snapshot) => snapshot,
                None => {
                    state.running = false;
                    PENDING.fetch_sub(1, Ordering::SeqCst);
                    IDLE.notify_waiters();
                    return;
                }
            }
        };
        match write_session(&shared, &snapshot).await {
            Ok(()) => {}
            Err(Failure { cause, disable }) => {
                if disable {
                    shared.state.lock().unwrap().disabled = true;
                }
                shared.note(cause);
            }
        }
    }
}

/// Why a flush did not complete, and whether the flusher should give up.
struct Failure {
    cause: String,
    disable: bool,
}

impl Shared {
    /// Report `cause` once: on stderr now, and as a pending `warning:` line.
    fn note(&self, cause: String) {
        let mut state = self.state.lock().unwrap();
        if state.warned.insert(cause.clone()) {
            eprintln!("opencode rollout: {cause}");
            state
                .warnings
                .push(format!("warning: OpenCode rollout: {cause}"));
        }
    }
}

async fn write_session(shared: &Arc<Shared>, snapshot: &Transcript) -> Result<(), Failure> {
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
            shared.state.lock().unwrap().written = true;
            Ok(())
        }
        OpenCodeMode::Api => match api_write(shared, &body).await {
            Ok(()) => {
                shared.state.lock().unwrap().written = true;
                Ok(())
            }
            Err(failure) if cfg.fallback == OpenCodeFallback::Db => {
                match db_write(shared, &body).await {
                    Ok(()) => {
                        shared.state.lock().unwrap().written = true;
                        // The API failed but the session is safe: report it, keep going.
                        shared.note(format!(
                            "the API write failed ({}); wrote the session to the database instead",
                            failure.error
                        ));
                        Ok(())
                    }
                    Err(db_error) => Err(fail(format!(
                        "the API write failed ({}) and the database fallback failed ({db_error})",
                        failure.error
                    ))),
                }
            }
            Err(failure) => {
                if failure.session_missing {
                    // The old copy is gone and the import failed: nothing exists now.
                    shared.state.lock().unwrap().written = false;
                }
                Err(fail(if failure.session_missing {
                    format!(
                        "the session is missing from OpenCode: the import failed after the old copy was deleted ({})",
                        failure.error
                    )
                } else {
                    format!("the API write failed: {}", failure.error)
                }))
            }
        },
    }
}

/// Resolve (and cache) the OpenCode project id of the session's directory.
async fn resolve_project(shared: &Arc<Shared>) -> Result<Option<String>, String> {
    if let Some(id) = shared.state.lock().unwrap().project_id.clone() {
        return Ok(Some(id));
    }
    let cfg = &shared.cfg;
    let projects = match cfg.mode {
        OpenCodeMode::Db => db_projects(shared)
            .await
            .map_err(|e| format!("project lookup in the database failed: {e}"))?,
        OpenCodeMode::Api => match api_client(cfg) {
            Ok(client) => match client.list_projects().await {
                Ok(projects) => projects,
                Err(api_error) => fallback_projects(shared, api_error).await?,
            },
            Err(api_error) => fallback_projects(shared, api_error).await?,
        },
    };
    let git_root = crate::repo::root(&shared.cwd);
    let found = pick_project(&shared.cwd, git_root.as_deref(), &projects).map(|p| p.id.clone());
    if let Some(id) = &found {
        shared.state.lock().unwrap().project_id = Some(id.clone());
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

fn api_client(cfg: &OpenCodeConfig) -> Result<ApiClient, OpenCodeError> {
    let bin = find_opencode(cfg).ok_or(OpenCodeError::NotFound)?;
    Ok(ApiClient::new(bin, API_TIMEOUT))
}

/// Write through the API: replace the earlier copy of the session when there is one.
async fn api_write(shared: &Arc<Shared>, body: &Value) -> Result<(), ApiFailure> {
    let plain = |error| ApiFailure {
        error,
        session_missing: false,
    };
    let client = api_client(&shared.cfg).map_err(plain)?;
    let id = shared.session_id.as_str();
    let written = shared.state.lock().unwrap().written;
    let mut deleted = false;
    if written {
        match client.delete(id).await {
            Ok(()) | Err(OpenCodeError::Status(404, _)) => deleted = true,
            Err(error) => return Err(plain(error)),
        }
    }
    match client.import(body).await {
        Ok(()) => Ok(()),
        // The first import found the id already present: replace it.
        Err(OpenCodeError::Status(409, _)) if !deleted => {
            client.delete(id).await.map_err(plain)?;
            client.import(body).await.map_err(|error| ApiFailure {
                error,
                session_missing: true,
            })
        }
        Err(error) => Err(ApiFailure {
            error,
            session_missing: deleted,
        }),
    }
}

#[cfg(unix)]
mod database {
    use super::*;
    use crate::rollout::opencode::db::{DbWriter, default_db_path};

    /// The OpenCode version the database writer needs: `opencode --version`
    /// when the API mode may use the binary, otherwise (or when that fails) the
    /// version recorded in the database itself.
    async fn version(
        shared: &Arc<Shared>,
        path: &std::path::Path,
    ) -> Result<String, OpenCodeError> {
        if let Some(version) = shared.state.lock().unwrap().version.clone() {
            return Ok(version);
        }
        let mut found = None;
        if shared.cfg.mode == OpenCodeMode::Api
            && let Ok(client) = api_client(&shared.cfg)
        {
            found = client.version().await.ok();
        }
        let version = match found {
            Some(version) => version,
            None => {
                let path = path.to_path_buf();
                tokio::task::spawn_blocking(move || DbWriter::detect_version(&path))
                    .await
                    .map_err(|e| OpenCodeError::Other(e.to_string()))??
            }
        };
        shared.state.lock().unwrap().version = Some(version.clone());
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
