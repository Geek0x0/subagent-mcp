//! OpenCode rollout backend.
//!
//! The recorder here consumes the same Codex-shaped lines the Codex backend
//! writes (`RolloutRecorder::write_line` is the single funnel) and, in later
//! tasks, projects them to OpenCode's session format. For now it is a stub with
//! the final public signatures: the first three methods drop their input and
//! `take_warning` has nothing to report.

pub mod transcript;

use std::path::PathBuf;

use serde_json::Value;

use crate::config::OpenCodeConfig;

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
