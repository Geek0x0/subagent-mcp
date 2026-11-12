//! Codex app-server notification to rollout-event mapping.
//!
//! Ports Go `internal/provider/codexappserver/events.go`. Notifications that do
//! not map to a rollout event (unknown methods, retryable errors, non-final
//! turn completions, unknown item types) are ignored: the adapter tolerates a
//! newer app-server as long as a required response still parses.

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

/// Go `json.Unmarshal` treats an explicit `null` for a non-pointer struct field
/// as the zero value, while serde's `#[serde(default)]` only covers a *missing*
/// field. Route non-optional fields through this helper so `null` collapses to
/// `T::default()` and a notification the app-server padded with nulls is not
/// dropped.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// The subset of notification params the thread and event mapper consume.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct NotificationParams {
    #[serde(rename = "threadId", deserialize_with = "null_default")]
    pub thread_id: String,
    #[serde(rename = "turnId", deserialize_with = "null_default")]
    pub turn_id: String,
    #[serde(rename = "itemId", deserialize_with = "null_default")]
    pub item_id: String,
    #[serde(deserialize_with = "null_default")]
    pub delta: String,
    /// Approval requests carry the command to run; `null` means absent.
    pub command: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub item: CodexItem,
    #[serde(deserialize_with = "null_default")]
    pub turn: TurnParams,
    #[serde(rename = "tokenUsage", deserialize_with = "null_default")]
    pub token_usage: TokenUsage,
    #[serde(deserialize_with = "null_default")]
    pub error: ErrorParams,
    /// Approval requests carry a human-readable reason.
    #[serde(deserialize_with = "null_default")]
    pub reason: String,
    #[serde(rename = "willRetry", deserialize_with = "null_default")]
    pub will_retry: bool,
    #[serde(rename = "networkApprovalContext", deserialize_with = "null_default")]
    pub network_approval_context: NetworkApprovalContext,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CodexItem {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    #[serde(rename = "type", deserialize_with = "null_default")]
    pub kind: String,
    #[serde(deserialize_with = "null_default")]
    pub text: String,
    #[serde(deserialize_with = "null_default")]
    pub phase: String,
    /// `null` in a network-approval request; Go's `string` zero-values it.
    pub command: Option<String>,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i64>,
    pub changes: Option<Vec<Change>>,
}

impl CodexItem {
    /// The paths a file-change item touched, as `Vec` (never `null`).
    pub fn paths(&self) -> Vec<String> {
        self.changes
            .as_ref()
            .map(|changes| changes.iter().map(|change| change.path.clone()).collect())
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Change {
    #[serde(deserialize_with = "null_default")]
    pub path: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TurnParams {
    #[serde(deserialize_with = "null_default")]
    pub id: String,
    #[serde(deserialize_with = "null_default")]
    pub status: String,
    #[serde(deserialize_with = "null_default")]
    pub error: ErrorParams,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ErrorParams {
    #[serde(deserialize_with = "null_default")]
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TokenUsage {
    #[serde(deserialize_with = "null_default")]
    pub last: TokenLast,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TokenLast {
    #[serde(rename = "inputTokens", deserialize_with = "null_default")]
    pub input_tokens: i64,
    #[serde(rename = "outputTokens", deserialize_with = "null_default")]
    pub output_tokens: i64,
    #[serde(rename = "totalTokens", deserialize_with = "null_default")]
    pub total_tokens: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct NetworkApprovalContext {
    #[serde(deserialize_with = "null_default")]
    pub host: String,
    #[serde(deserialize_with = "null_default")]
    pub protocol: String,
}

/// The narrow shape of an approval request, matching Go `approvalParams`.
///
/// Approval params are parsed on their own so an unrelated malformed field
/// (`item`, `turn`, `tokenUsage`, ...) cannot reject an otherwise valid request.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ApprovalParams {
    #[serde(rename = "turnId", deserialize_with = "null_default")]
    pub turn_id: String,
    #[serde(rename = "itemId", deserialize_with = "null_default")]
    pub item_id: String,
    /// `null` means absent, like Go's zero-valued string.
    pub command: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub reason: String,
    #[serde(rename = "networkApprovalContext", deserialize_with = "null_default")]
    pub network_approval_context: NetworkApprovalContext,
}

/// Parse raw notification params, tolerating unknown and null fields.
pub(crate) fn parse_params(params: Option<&RawValue>) -> Option<NotificationParams> {
    serde_json::from_str(params?.get()).ok()
}

/// Parse raw approval params, ignoring unrelated notification fields.
pub(crate) fn parse_approval(params: Option<&RawValue>) -> Option<ApprovalParams> {
    serde_json::from_str(params?.get()).ok()
}

/// Map one app-server notification to a rollout event.
///
/// Returns `None` for notifications the rollout does not surface. `params` may
/// be any JSON value; a value that is not an object is treated like malformed
/// params and ignored rather than failing the turn.
pub fn map_notification(method: &str, params: &Value) -> Option<Value> {
    let params = NotificationParams::deserialize(params).ok()?;
    match method {
        "turn/started" => Some(json!({"type": "task_started"})),
        "turn/completed" => {
            (params.turn.status == "completed").then(|| json!({"type": "task_complete"}))
        }
        "item/agentMessage/delta" => {
            Some(json!({"type": "agent_message_delta", "delta": params.delta}))
        }
        "item/started" | "item/completed" => {
            let item = &params.item;
            if item.kind == "agentMessage" && method == "item/completed" {
                return Some(json!({"type": "agent_message", "message": item.text}));
            }
            if item.kind != "commandExecution" && item.kind != "fileChange" {
                return None;
            }
            let mut event = Map::new();
            event.insert(
                "type".to_string(),
                json!(if method == "item/completed" {
                    "exec_command_end"
                } else {
                    "exec_command_begin"
                }),
            );
            event.insert("call_id".to_string(), json!(item.id));
            if item.kind == "fileChange" {
                event.insert("tool".to_string(), json!("apply_patch"));
                event.insert("paths".to_string(), json!(item.paths()));
            } else {
                event.insert("tool".to_string(), json!("shell"));
                if method == "item/started" {
                    event.insert(
                        "command".to_string(),
                        json!(item.command.clone().unwrap_or_default()),
                    );
                } else if let Some(exit_code) = item.exit_code {
                    event.insert("exit_code".to_string(), json!(exit_code));
                }
            }
            Some(Value::Object(event))
        }
        "thread/tokenUsage/updated" => {
            let last = &params.token_usage.last;
            Some(json!({
                "type": "token_count",
                "prompt_tokens": last.input_tokens,
                "completion_tokens": last.output_tokens,
                "total_tokens": last.total_tokens,
            }))
        }
        "error" => {
            (!params.will_retry).then(|| json!({"type": "error", "message": params.error.message}))
        }
        _ => None,
    }
}
