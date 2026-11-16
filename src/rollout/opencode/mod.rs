//! OpenCode rollout backend.
//!
//! The recorder here consumes the same Codex-shaped lines the Codex backend
//! writes (`RolloutRecorder::write_line` is the single funnel) and, in later
//! tasks, projects them to OpenCode's session format. For now it is a stub with
//! the final public signatures: the first three methods drop their input and
//! `take_warning` has nothing to report.

pub mod api;
pub mod project;
pub mod transcript;

use std::fmt;
use std::path::PathBuf;

use serde_json::Value;

use crate::config::OpenCodeConfig;

/// Why an OpenCode write failed. Shared by the API client and the database
/// writer; the flusher treats every variant as a failure to degrade from, never
/// as a failure of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenCodeError {
    /// The `opencode` binary or the database file does not exist.
    NotFound,
    /// A call exceeded its timeout.
    Timeout,
    /// The request body does not fit on a command line.
    TooLarge,
    /// The server answered with an HTTP error status.
    Status(u16, String),
    /// The process could not be started.
    Spawn(String),
    /// The database schema is not one we know how to write.
    Schema(String),
    /// The database is locked by another writer.
    Locked,
    /// Anything else (unparseable output, I/O).
    Other(String),
}

impl fmt::Display for OpenCodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("opencode not found"),
            Self::Timeout => f.write_str("opencode call timed out"),
            Self::TooLarge => f.write_str("transcript too large for the opencode command line"),
            Self::Status(code, text) => write!(f, "opencode answered HTTP {code}: {text}"),
            Self::Spawn(msg) => write!(f, "cannot start opencode: {msg}"),
            Self::Schema(msg) => write!(f, "unsupported opencode database schema: {msg}"),
            Self::Locked => f.write_str("opencode database is locked"),
            Self::Other(msg) => write!(f, "opencode: {msg}"),
        }
    }
}

impl std::error::Error for OpenCodeError {}

/// Per-session OpenCode recorder.
pub struct Recorder {
    // Held for the real implementation; unused by the stub.
    #[allow(dead_code)]
    cfg: OpenCodeConfig,
    #[allow(dead_code)]
    cwd: PathBuf,
    #[allow(dead_code)]
    session_id: String,
}

impl Recorder {
    pub fn new(cfg: OpenCodeConfig, cwd: PathBuf, session_id: String) -> Self {
        Self {
            cfg,
            cwd,
            session_id,
        }
    }

    /// Consume one Codex-shaped rollout line.
    pub fn push_line(&mut self, _kind: &str, _payload: &Value) {}

    /// Mark a recorded tool call as an error (or not).
    pub fn mark_tool_error(&mut self, _call_id: &str, _is_error: bool) {}

    /// A run finished: schedule a flush.
    pub fn run_ended(&mut self) {}

    /// A pending warning, once.
    pub fn take_warning(&mut self) -> Option<String> {
        None
    }
}
