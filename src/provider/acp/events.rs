//! ACP `session/update` to rollout-event mapping, permission decisions and
//! stop-reason handling.
//!
//! Every function is pure and lenient: input is accessed through `Value`
//! lookups and anything malformed maps to "ignore" rather than an error.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::policy::Sandbox;
use crate::util::approval::ApprovalRequest;

/// A mapped `session/update`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MappedUpdate {
    /// The rollout event to emit, if any.
    pub event: Option<Value>,
    /// An agent message chunk to accumulate into the final answer.
    pub text: Option<String>,
}

/// `kind` and `title` per `toolCallId`, remembered from the `tool_call` so a
/// partial `tool_call_update` can still name the tool.
#[derive(Clone, Debug, Default)]
pub struct ToolCallCache(pub HashMap<String, (String, String)>);

/// How a `session/prompt` turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    Ok,
    Cancelled,
    Partial(String),
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The text of a single ACP content block; non-text blocks become placeholders.
fn content_text(block: &Value) -> String {
    match str_at(block, "type") {
        Some("text") => str_at(block, "text").unwrap_or("").to_string(),
        Some("image") => "[image]".to_string(),
        _ => {
            let uri = str_at(block, "uri")
                .or_else(|| block.pointer("/resource/uri").and_then(Value::as_str));
            match uri {
                Some(uri) => format!("[resource: {uri}]"),
                None => "[image]".to_string(),
            }
        }
    }
}

/// Map one `session/update` notification for `session_id`.
pub fn map_update(
    session_id: &str,
    params: &Value,
    cache: &mut ToolCallCache,
) -> Option<MappedUpdate> {
    if str_at(params, "sessionId") != Some(session_id) {
        return None;
    }
    let update = params.get("update")?;
    match str_at(update, "sessionUpdate")? {
        "agent_message_chunk" => {
            let text = content_text(update.get("content")?);
            Some(MappedUpdate {
                event: Some(json!({"type": "agent_message_delta", "delta": text})),
                text: Some(text),
            })
        }
        "agent_thought_chunk" => {
            let text = content_text(update.get("content")?);
            Some(MappedUpdate {
                event: Some(json!({"type": "agent_reasoning_delta", "delta": text})),
                text: None,
            })
        }
        "tool_call" => {
            let id = str_at(update, "toolCallId").unwrap_or("").to_string();
            let kind = str_at(update, "kind").unwrap_or("").to_string();
            let title = str_at(update, "title").unwrap_or("").to_string();
            cache.0.insert(id.clone(), (kind.clone(), title.clone()));
            let mut event = Map::new();
            event.insert("type".into(), json!("exec_command_begin"));
            event.insert("call_id".into(), json!(id));
            event.insert("tool".into(), json!(kind));
            event.insert("command".into(), json!(title));
            if let Some(raw) = update.get("rawInput").filter(|v| !v.is_null()) {
                event.insert("raw_input".into(), raw.clone());
            }
            Some(MappedUpdate {
                event: Some(Value::Object(event)),
                text: None,
            })
        }
        "tool_call_update" => {
            let id = str_at(update, "toolCallId").unwrap_or("").to_string();
            let entry = cache.0.entry(id.clone()).or_default();
            if let Some(kind) = str_at(update, "kind") {
                entry.0 = kind.to_string();
            }
            if let Some(title) = str_at(update, "title") {
                entry.1 = title.to_string();
            }
            let status = str_at(update, "status")?;
            if status != "completed" && status != "failed" {
                return None;
            }
            let (kind, title) = cache.0.remove(&id).unwrap_or_default();
            let mut event = Map::new();
            event.insert("type".into(), json!("exec_command_end"));
            event.insert("call_id".into(), json!(id));
            event.insert("tool".into(), json!(kind));
            event.insert("command".into(), json!(title));
            event.insert("status".into(), json!(status));
            if let Some(raw) = update.get("rawOutput").filter(|v| !v.is_null()) {
                event.insert("raw_output".into(), raw.clone());
            }
            Some(MappedUpdate {
                event: Some(Value::Object(event)),
                text: None,
            })
        }
        "plan" => {
            let entries = update.get("entries").cloned().unwrap_or_else(|| json!([]));
            Some(MappedUpdate {
                event: Some(json!({"type": "plan_update", "entries": entries})),
                text: None,
            })
        }
        _ => None,
    }
}

fn tool_kind(params: &Value) -> Option<&str> {
    params.pointer("/toolCall/kind").and_then(Value::as_str)
}

/// The approval request for a `session/request_permission`.
pub fn permission_request(thread_id: &str, params: &Value) -> ApprovalRequest {
    let title = params
        .pointer("/toolCall/title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let paths = params
        .pointer("/toolCall/locations")
        .and_then(Value::as_array)
        .map(|locations| {
            locations
                .iter()
                .filter_map(|l| str_at(l, "path").map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ApprovalRequest {
        thread_id: thread_id.to_string(),
        tool: tool_kind(params)
            .filter(|k| !k.is_empty())
            .unwrap_or("acp")
            .to_string(),
        command: Some(title.clone()),
        paths,
        reason: title,
    }
}

/// True when a read-only sandbox must reject the request without asking.
pub fn read_only_auto_reject(sandbox: Sandbox, params: &Value) -> bool {
    sandbox == Sandbox::ReadOnly
        && matches!(
            tool_kind(params),
            Some("edit" | "delete" | "move" | "execute")
        )
}

/// The full `result` object answering a `session/request_permission`.
pub fn pick_option(params: &Value, approved: bool) -> Value {
    let preferred: [&str; 2] = if approved {
        ["allow_once", "allow_always"]
    } else {
        ["reject_once", "reject_always"]
    };
    let options = params
        .get("options")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    preferred
        .iter()
        .find_map(|kind| {
            options
                .iter()
                .find(|o| str_at(o, "kind") == Some(kind))
                .and_then(|o| str_at(o, "optionId"))
        })
        .map(|id| json!({"outcome": {"outcome": "selected", "optionId": id}}))
        .unwrap_or_else(|| json!({"outcome": {"outcome": "cancelled"}}))
}

/// Classify a `session/prompt` result's `stopReason`.
pub fn stop_reason(result: &Value) -> Stop {
    match str_at(result, "stopReason") {
        Some("cancelled") => Stop::Cancelled,
        Some(reason @ ("max_tokens" | "max_turn_requests" | "refusal")) => {
            Stop::Partial(reason.to_string())
        }
        _ => Stop::Ok,
    }
}
