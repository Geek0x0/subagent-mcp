//! Codex-format session rollout writer (Task 21).
//!
//! Ports Go v0.13.1 `internal/rollout/rollout.go`. A recorder appends one
//! `{timestamp, type, payload}` JSON object per line under
//! `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<timestamp>-<threadId>.jsonl`,
//! using UTC, 0700 directories and a 0600 file. The first write failure logs
//! once and disables the recorder; later writes are no-ops.
//!
//! Unlike Go's `Open` (which reopens the file per write so sessions hold no
//! descriptors), the Rust recorder keeps the file descriptor open, as the
//! brief's `RolloutRecorder { file: File, disabled: bool }` shape requires.
//!
//! The recorder NEVER handles provider replay payloads or opaque bytes: its
//! API accepts only plain scalars, strings, `Usage`, `ToolCall`, and
//! `patch::Change` values. `session_meta` is written by the server, not here;
//! the recorder's public API starts at turn records (see `crate::agent::rollout`).

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::util::time::rfc3339_nano;

/// The recorder's error type: a message, `Display`, and `Error`.
#[derive(Clone, Debug)]
pub struct RolloutError {
    pub message: String,
}

impl RolloutError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RolloutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RolloutError {}

/// Appends Codex rollout lines for one session. A disabled recorder is a no-op.
pub struct RolloutRecorder {
    file: fs::File,
    disabled: bool,
}

impl RolloutRecorder {
    /// Create the rollout file for `thread_id`.
    ///
    /// Returns `Err` when `SUBAGENT_MCP_ROLLOUT=off`, when the home directory
    /// cannot be resolved, or when the directory/file cannot be created (Go's
    /// `Open` returns a nil/no-op recorder and logs in these cases). Callers
    /// use [`Self::disabled_by_env`] to tell the intentional opt-out from a
    /// creation failure they should report.
    pub fn create(thread_id: &str) -> Result<Self, RolloutError> {
        create_at(thread_id, SystemTime::now())
    }

    /// Whether rollout writing was disabled on purpose (`SUBAGENT_MCP_ROLLOUT=off`).
    pub fn disabled_by_env() -> bool {
        disabled_by_env()
    }

    /// Append one `{timestamp, type, payload}` line and flush it. On failure,
    /// log once and disable the recorder. Crate-visible so the in-crate server
    /// (Task 24) can write `session_meta` without exposing a public API.
    pub(crate) fn write_line(&mut self, line_type: &str, payload: Value) {
        if self.disabled {
            return;
        }
        let line = json!({
            "timestamp": rfc3339_nano(SystemTime::now()),
            "type": line_type,
            "payload": payload,
        });
        // The write blocks on the filesystem. On the multi-thread runtime used
        // by the server, run it on the blocking pool so a slow disk cannot
        // stall a turn; `block_in_place` panics on the current-thread runtime,
        // so the tests keep the inline path (and stay deterministic).
        let multi_thread = tokio::runtime::Handle::try_current()
            .map(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
            .unwrap_or(false);
        let write = |recorder: &mut Self| recorder.write_line_io(&line);
        let result: io::Result<()> = if multi_thread {
            tokio::task::block_in_place(|| write(self))
        } else {
            write(self)
        };
        if let Err(err) = result {
            eprintln!("rollout disabled: {err}");
            self.disabled = true;
        }
    }

    /// Marshal fully first (like Go's `json.Marshal`), then one write+flush.
    fn write_line_io(&mut self, line: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(line).map_err(io::Error::other)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.flush()
    }
}

fn disabled_by_env() -> bool {
    std::env::var("SUBAGENT_MCP_ROLLOUT").as_deref() == Ok("off")
}

fn create_at(thread_id: &str, created: SystemTime) -> Result<RolloutRecorder, RolloutError> {
    if disabled_by_env() {
        return Err(RolloutError::new(
            "rollout disabled by SUBAGENT_MCP_ROLLOUT=off",
        ));
    }
    match try_create(thread_id, created) {
        Ok(recorder) => Ok(recorder),
        Err(err) => {
            eprintln!("rollout disabled: {err}");
            Err(RolloutError::new(format!("rollout disabled: {err}")))
        }
    }
}

fn try_create(thread_id: &str, created: SystemTime) -> io::Result<RolloutRecorder> {
    let home = codex_home()?;
    let dt: DateTime<Utc> = DateTime::from(created);
    let dir = home
        .join("sessions")
        .join(dt.format("%Y").to_string())
        .join(dt.format("%m").to_string())
        .join(dt.format("%d").to_string());
    mkdir_all_700(&dir)?;
    let filename = format!(
        "rollout-{}-{}.jsonl",
        dt.format("%Y-%m-%dT%H-%M-%S"),
        thread_id
    );
    let path = dir.join(filename);
    let file = open_rollout_file(&path)?;
    Ok(RolloutRecorder {
        file,
        disabled: false,
    })
}

/// Go's `os.Getenv("CODEX_HOME")`, falling back to `os.UserHomeDir()/.codex`.
fn codex_home() -> io::Result<PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME")
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home));
    }
    let user_home =
        crate::config::user_home().ok_or_else(|| io::Error::other("resolve home directory"))?;
    Ok(user_home.join(".codex"))
}

/// Go's `os.MkdirAll(dir, 0o700)`: every newly created component gets mode
/// 0700. On non-Unix platforms the OS ignores these bits (Go does too).
#[cfg(unix)]
fn mkdir_all_700(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if path.is_dir() {
        return Ok(());
    }
    if path.exists() {
        return Err(io::Error::other(
            "rollout path component is not a directory",
        ));
    }
    if let Some(parent) = path.parent() {
        mkdir_all_700(parent)?;
    }
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn mkdir_all_700(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// Go's `os.OpenFile(path, O_CREATE|O_WRONLY|O_APPEND, 0o600)`.
fn open_rollout_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).write(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recorder whose held descriptor cannot be written must disable itself
    /// on the first failure and stay a no-op afterwards (Go's
    /// `TestWriteFailureDisablesRecorder` / `TestNilRecorderIsNoop` behavior,
    /// adapted for the held-descriptor design).
    #[test]
    fn write_failure_disables_and_noops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        fs::write(&path, b"").unwrap();
        // Read-only descriptor: write(2) fails with EBADF.
        let file = fs::OpenOptions::new().read(true).open(&path).unwrap();
        let mut recorder = RolloutRecorder {
            file,
            disabled: false,
        };
        recorder.write_line("a", json!({}));
        assert!(
            recorder.disabled,
            "first failure should disable the recorder"
        );
        recorder.write_line("b", json!({}));
        assert!(recorder.disabled, "later writes stay a no-op");
        assert!(fs::read(&path).unwrap().is_empty(), "nothing was written");
    }
}
