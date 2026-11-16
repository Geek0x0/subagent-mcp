//! OpenCode rollout backend.
//!
//! The recorder here consumes the same Codex-shaped lines the Codex backend
//! writes (`RolloutRecorder::write_line` is the single funnel), keeps them as a
//! transcript, and at the end of every run hands a snapshot to a background
//! flusher that writes the session into OpenCode (the official API first, the
//! SQLite database as the fallback).

pub mod api;
#[cfg(unix)]
pub mod db;
pub mod flush;
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

/// The OpenCode release line the API shapes and the database layout were
/// verified against.
pub const TESTED_VERSION_PREFIX: &str = "2.3.";

/// Whether `version` (`2.3.0`, `v2.3.5`, ` 2.3.1 `) is in the tested range: a
/// single optional leading `v`, then `2.3.` followed by a digit.
pub fn in_tested_range(version: &str) -> bool {
    let release = version.trim();
    let release = release.strip_prefix('v').unwrap_or(release);
    release
        .strip_prefix(TESTED_VERSION_PREFIX)
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
}

/// Per-session OpenCode recorder: accumulates the transcript from the Codex
/// lines the session produces and flushes it in the background at the end of
/// every run.
pub struct Recorder {
    transcript: transcript::Transcript,
    flusher: flush::Flusher,
}

impl Recorder {
    pub fn new(cfg: OpenCodeConfig, cwd: PathBuf, _session_id: String) -> Self {
        // OpenCode gets its own session id; the subagent-mcp thread id is not used.
        let opencode_id = transcript::new_session_id(now_ms());
        Self {
            transcript: transcript::Transcript::new(),
            flusher: flush::Flusher::new(cfg, cwd, opencode_id),
        }
    }

    /// Consume one Codex-shaped rollout line.
    pub fn push_line(&mut self, kind: &str, payload: &Value) {
        self.transcript.push_line_at(kind, payload, now_ms());
    }

    /// Mark a recorded tool call as an error (a success needs no mark).
    pub fn mark_tool_error(&mut self, call_id: &str, is_error: bool) {
        if is_error {
            self.transcript.mark_tool_error(call_id);
        }
    }

    /// A run finished: hand a snapshot to the background flusher.
    pub fn run_ended(&mut self) {
        if !self.transcript.is_empty() {
            self.flusher.schedule(self.transcript.clone());
        }
    }

    /// A pending warning, once.
    pub fn take_warning(&mut self) -> Option<String> {
        self.flusher.take_warning()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
