//! Progress notifications for a single tool call.
//!
//! Mirrors Go `Server.Emit`: every runner event is forwarded as
//! `subagent/event`, and a call that opted into progress with
//! `_meta.progressToken` also gets a mapped summary as
//! `notifications/progress`. Notifications are sent non-blockingly; a client
//! that cannot keep up loses frames, the sink counts the drops, and only the
//! first drop writes a log line with the running total.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::util::json::truncate_runes;

/// Go's `progressSummaryLimit`: the maximum number of runes a
/// `notifications/progress` message carries before it is truncated.
const PROGRESS_SUMMARY_LIMIT: usize = 200;

/// Per-call progress state. The token is the client's `_meta.progressToken`;
/// `out` is the server's serialized-frame channel and `dropped` counts
/// notifications the bounded channel refused.
pub struct ProgressSink {
    token: Option<Value>,
    out: mpsc::Sender<String>,
    dropped: Arc<AtomicU64>,
    /// Per-call `notifications/progress` sequence number. Go keeps the same
    /// counter in its per-call `progressState`, so concurrent calls each start
    /// at 1 and never share a sequence.
    count: AtomicU64,
}

impl ProgressSink {
    pub fn new(token: Option<Value>, out: mpsc::Sender<String>, dropped: Arc<AtomicU64>) -> Self {
        Self {
            token,
            out,
            dropped,
            count: AtomicU64::new(0),
        }
    }

    /// Emits one runner event, and its progress summary when the call carries
    /// a progress token.
    pub fn emit(&self, thread_id: &str, msg: &Value) {
        self.send(
            "subagent/event",
            json!({
                "jsonrpc": "2.0",
                "method": "subagent/event",
                "params": {"threadId": thread_id, "msg": msg},
            }),
        );

        let Some(token) = self.token.as_ref() else {
            return;
        };
        let Some(summary) = progress_summary(msg) else {
            return;
        };
        // Go's `progressState.next()` increments only when a summary is sent.
        let progress = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(
            "notifications/progress",
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {
                    "progressToken": token,
                    "progress": progress,
                    "message": summary,
                },
            }),
        );
    }

    /// Non-blocking send. A full or closed client channel drops the frame
    /// instead of stalling the run that holds a session lock. Only the first
    /// drop is logged: a log line per dropped delta would itself stall the run
    /// once stderr fills.
    fn send(&self, kind: &str, frame: Value) {
        if let Err(error) = self.out.try_send(frame.to_string()) {
            let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if total == 1 {
                eprintln!(
                    "{kind} dropped (client cannot keep up): {error}; {total} dropped so far, further drops are counted, not logged"
                );
            }
        }
    }
}

/// The short `notifications/progress` message for a runner event, or `None`
/// for events clients should not see progress for.
fn progress_summary(msg: &Value) -> Option<String> {
    match msg.get("type").and_then(Value::as_str)? {
        "task_started" => Some("started".to_string()),
        "exec_command_begin" => {
            if let Some(command) = msg
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| !command.is_empty())
            {
                return Some(capped(format!("shell: {command}")));
            }
            if let Some(paths) = string_paths(msg) {
                return Some(capped(format!("apply_patch: {}", paths.join(", "))));
            }
            let tool = msg.get("tool").and_then(Value::as_str).unwrap_or("");
            let path = msg.get("path").and_then(Value::as_str).unwrap_or("");
            Some(capped(format!("{tool}: {path}")))
        }
        "agent_message" => {
            let message = msg.get("message").and_then(Value::as_str).unwrap_or("");
            let first_line = message
                .split_once('\n')
                .map(|(first, _)| first)
                .unwrap_or(message);
            Some(capped(format!("agent: {first_line}")))
        }
        "task_complete" => Some("completed".to_string()),
        "error" => {
            let message = msg.get("message").and_then(Value::as_str).unwrap_or("");
            Some(capped(format!("error: {message}")))
        }
        _ => None,
    }
}

/// Go asserts `msg["paths"].([]string)`; a non-string entry fails the
/// assertion and falls through to the tool/path branch.
fn string_paths(msg: &Value) -> Option<Vec<&str>> {
    msg.get("paths")?
        .as_array()?
        .iter()
        .map(Value::as_str)
        .collect()
}

fn capped(summary: String) -> String {
    truncate_runes(&summary, PROGRESS_SUMMARY_LIMIT)
}
