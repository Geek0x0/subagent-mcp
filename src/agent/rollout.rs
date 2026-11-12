//! Go `internal/agent/rollout.go` port: turn records built as Codex JSONL
//! payloads and appended through [`RolloutRecorder::write_line`].
//!
//! Every method mirrors the corresponding Go `record*` function's payload
//! exactly. Fields that Go reads from session state but does not put on the
//! wire (e.g. `turn_id` on item payloads, `tool`, `is_error`) are accepted for
//! caller convenience and intentionally not emitted. No provider replay
//! payloads or opaque bytes are ever recorded.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use serde_json::{Map, Value, json};

use crate::patch::{Change, ChangeKind};
use crate::provider::{ToolCall, Usage};
use crate::rollout::RolloutRecorder;

/// Go `provider.Usage` -> the Codex `usagePayload` map.
fn usage_payload(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input,
        "cached_input_tokens": u.cached,
        "output_tokens": u.output,
        "reasoning_output_tokens": u.reasoning,
        "total_tokens": u.total,
    })
}

/// Go `textContent(kind, text)`.
fn text_content(kind: &str, text: &str) -> Value {
    json!([{"type": kind, "text": text}])
}

/// Go `started.Unix()`: whole seconds since the epoch.
fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Go `end.Sub(start).Milliseconds()`.
fn millis_between(start: SystemTime, end: SystemTime) -> i64 {
    match end.duration_since(start) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Go `started.Format("2006-01-02")` on a local `time.Now()` (not UTC).
fn current_date(t: SystemTime) -> String {
    let dt: DateTime<Local> = DateTime::from(t);
    dt.format("%Y-%m-%d").to_string()
}

impl RolloutRecorder {
    /// Go `recordTurnStart`: turn_context, task_started, the user prompt item,
    /// and the user_message event.
    ///
    /// `writable_roots` mirrors Go `shellWritableRoots()`: `None` is Go's nil
    /// slice and serializes as JSON `null` (danger-full-access, or
    /// workspace-write with zero roots); `Some(&[])` is Go's empty non-nil
    /// slice and serializes as `[]` (read-only); `Some(roots)` serializes the
    /// array (workspace-write with roots).
    #[allow(clippy::too_many_arguments)]
    pub fn record_turn_start(
        &mut self,
        turn_id: &str,
        cwd: &Path,
        prompt: &str,
        approval_policy: &str,
        sandbox: &str,
        writable_roots: Option<&[PathBuf]>,
        model: &str,
        effort: &str,
        effort_sent: Option<&str>,
    ) {
        let started = SystemTime::now();
        let roots: Value = match writable_roots {
            None => Value::Null,
            Some(paths) => Value::Array(
                paths
                    .iter()
                    .map(|p| json!(p.display().to_string()))
                    .collect(),
            ),
        };
        self.write_line(
            "turn_context",
            json!({
                "turn_id": turn_id,
                "cwd": cwd.display().to_string(),
                "approval_policy": approval_policy,
                "sandbox_policy": {"type": sandbox, "writable_roots": roots},
                "model": model,
                "effort": effort,
                "effort_sent": effort_sent.unwrap_or(""),
                "current_date": current_date(started),
            }),
        );
        self.write_line(
            "event_msg",
            json!({
                "type": "task_started",
                "turn_id": turn_id,
                "started_at": unix_secs(started),
            }),
        );
        self.write_line(
            "response_item",
            json!({
                "type": "message",
                "role": "user",
                "content": text_content("input_text", prompt),
            }),
        );
        self.write_line(
            "event_msg",
            json!({"type": "user_message", "message": prompt}),
        );
    }

    /// Go `recordModelTurn`: reasoning item, token_count event (last +
    /// cumulative usage), assistant message item, and agent_message event.
    pub fn record_model_turn(
        &mut self,
        turn_id: &str,
        text: &str,
        reasoning: Option<&str>,
        usage: Option<&Usage>,
        total_usage: &Usage,
    ) {
        let _ = turn_id;
        if let Some(reasoning) = reasoning
            && !reasoning.is_empty()
        {
            self.write_line(
                "response_item",
                json!({
                    "type": "reasoning",
                    "summary": [],
                    "content": text_content("reasoning_text", reasoning),
                }),
            );
        }
        if let Some(last) = usage {
            self.write_line(
                "event_msg",
                json!({
                    "type": "token_count",
                    "info": {
                        "total_token_usage": usage_payload(total_usage),
                        "last_token_usage": usage_payload(last),
                    },
                }),
            );
        }
        if !text.is_empty() {
            self.write_line(
                "response_item",
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": text_content("output_text", text),
                }),
            );
            self.write_line(
                "event_msg",
                json!({"type": "agent_message", "message": text}),
            );
        }
    }

    /// Go `recordThreadEvent`'s rollout half: a delegated thread reports usage
    /// through `token_count` events, so its rollout line is written here
    /// instead of from [`Self::record_model_turn`].
    pub fn record_thread_token_count(&mut self, last: &Usage, total: &Usage) {
        self.write_line(
            "event_msg",
            json!({
                "type": "token_count",
                "info": {
                    "total_token_usage": usage_payload(total),
                    "last_token_usage": usage_payload(last),
                },
            }),
        );
    }

    /// Go `recordToolCall`: apply_patch becomes a custom_tool_call whose input
    /// is the `patch` field of the arguments; everything else a function_call.
    pub fn record_tool_call(&mut self, turn_id: &str, call: &ToolCall, tool: &str) {
        let _ = (turn_id, tool);
        if call.name == "apply_patch" {
            let patch_text = serde_json::from_str::<Value>(&call.arguments)
                .ok()
                .and_then(|v| v.get("patch").and_then(|p| p.as_str()).map(String::from))
                .unwrap_or_default();
            self.write_line(
                "response_item",
                json!({
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": call.id,
                    "name": "apply_patch",
                    "input": patch_text,
                }),
            );
            return;
        }
        self.write_line(
            "response_item",
            json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.name,
                "arguments": call.arguments,
            }),
        );
    }

    /// Go `recordToolOutput`: apply_patch -> custom_tool_call_output, else
    /// function_call_output. `is_error` is history state, not a rollout field.
    pub fn record_tool_output(
        &mut self,
        turn_id: &str,
        call: &ToolCall,
        output: &str,
        is_error: bool,
    ) {
        let _ = (turn_id, is_error);
        let item_type = if call.name == "apply_patch" {
            "custom_tool_call_output"
        } else {
            "function_call_output"
        };
        self.write_line(
            "response_item",
            json!({"type": item_type, "call_id": call.id, "output": output}),
        );
    }

    /// Go `recordPatchApplied`: patch_apply_end event with a per-path changes
    /// map (add content / update unified_diff + move_path / delete).
    pub fn record_patch_applied(
        &mut self,
        turn_id: &str,
        call_id: &str,
        success: bool,
        stdout: &str,
        stderr: &str,
        changes: &[Change],
    ) {
        let mut by_path = Map::new();
        for change in changes {
            let mut entry = Map::new();
            entry.insert("type".into(), json!(change.kind.as_str()));
            entry.insert("move_path".into(), Value::Null);
            match change.kind {
                ChangeKind::Add => {
                    let content =
                        String::from_utf8_lossy(change.new_content.as_deref().unwrap_or(&[]))
                            .into_owned();
                    entry.insert("content".into(), json!(content));
                }
                ChangeKind::Update => {
                    entry.insert("unified_diff".into(), json!(change.diff));
                    if let Some(move_to) = &change.move_to
                        && !move_to.as_os_str().is_empty()
                    {
                        entry.insert("move_path".into(), json!(move_to.display().to_string()));
                    }
                }
                ChangeKind::Delete => {}
            }
            by_path.insert(change.path.display().to_string(), Value::Object(entry));
        }
        self.write_line(
            "event_msg",
            json!({
                "type": "patch_apply_end",
                "call_id": call_id,
                "turn_id": turn_id,
                "success": success,
                "stdout": stdout,
                "stderr": stderr,
                "changes": Value::Object(by_path),
            }),
        );
    }

    /// Go `recordTaskComplete`: task_complete event with Unix start/end and
    /// millisecond duration.
    pub fn record_task_complete(
        &mut self,
        turn_id: &str,
        last_agent_message: &str,
        started_at: SystemTime,
        completed_at: SystemTime,
    ) {
        self.write_line(
            "event_msg",
            json!({
                "type": "task_complete",
                "turn_id": turn_id,
                "last_agent_message": last_agent_message,
                "started_at": unix_secs(started_at),
                "completed_at": unix_secs(completed_at),
                "duration_ms": millis_between(started_at, completed_at),
            }),
        );
    }

    /// Go runner.go's `provider_request` event: error suppresses upstream;
    /// ttft_ms is present whenever a first delta arrived.
    pub fn record_provider_request(
        &mut self,
        provider: &str,
        model: &str,
        duration_ms: i64,
        ttft_ms: Option<i64>,
        upstream: Option<&str>,
        error: Option<&str>,
    ) {
        let mut payload = Map::new();
        payload.insert("type".into(), json!("provider_request"));
        payload.insert("provider".into(), json!(provider));
        payload.insert("model".into(), json!(model));
        payload.insert("duration_ms".into(), json!(duration_ms));
        if let Some(ttft) = ttft_ms {
            payload.insert("ttft_ms".into(), json!(ttft));
        }
        if let Some(error) = error {
            payload.insert("error".into(), json!(error));
        } else if let Some(upstream) = upstream {
            payload.insert("upstream".into(), json!(upstream));
        }
        self.write_line("event_msg", Value::Object(payload));
    }

    /// Go `recordError`: a bare error event.
    pub fn record_error(&mut self, message: &str) {
        self.write_line("event_msg", json!({"type": "error", "message": message}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Go `recordTurnStart` formats `started.Format("2006-01-02")` on a local
    /// `time.Now()`, not UTC. Compare `current_date` against `chrono::Local`
    /// for a fixed `SystemTime`; the expected value is computed (never
    /// hardcoded), so the assertion is timezone-robust.
    #[test]
    fn current_date_uses_local_date() {
        let fixed = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let expected: String = {
            let dt: DateTime<Local> = DateTime::from(fixed);
            dt.format("%Y-%m-%d").to_string()
        };
        assert_eq!(current_date(fixed), expected);
        assert_eq!(current_date(fixed).len(), 10, "YYYY-MM-DD format");
    }
}
