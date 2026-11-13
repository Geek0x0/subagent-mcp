//! Volatile-field normalization for differential comparisons.
//!
//! Normalization is deliberately field-name driven for JSON values and
//! pattern driven only for UUIDs in identifier fields and the CLI version
//! banner. Semantic fields such as `protocolVersion`, model ids,
//! method names, and error messages are left alone.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

/// Per-comparison normalization state.
///
/// Placeholders are stable within one context: the same UUID, timestamp,
/// duration, PID, or request id always maps to the same placeholder. Each
/// binary gets its own context so two different actual values still compare
/// equal when they occupy the same position.
#[derive(Debug, Default)]
pub struct NormalizeContext {
    temp_roots: Vec<String>,
    placeholders: RefCell<HashMap<String, HashMap<String, String>>>,
}

impl NormalizeContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace this path prefix with `<tmp>` in every string value.
    pub fn with_temp_root(mut self, root: impl AsRef<Path>) -> Self {
        let root = root
            .as_ref()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        if !root.is_empty() && !self.temp_roots.contains(&root) {
            self.temp_roots.push(root);
        }
        self
    }

    fn placeholder(&self, kind: &str, original: &str) -> String {
        let mut all = self.placeholders.borrow_mut();
        let map = all.entry(kind.to_string()).or_default();
        if let Some(value) = map.get(original) {
            return value.clone();
        }
        let value = format!("<{kind}:{}>", map.len() + 1);
        map.insert(original.to_string(), value.clone());
        value
    }

    fn replace_temp_roots(&self, input: &str) -> String {
        let mut out = input.to_string();
        for root in &self.temp_roots {
            out = out.replace(root, "<tmp>");
        }
        out
    }
}

/// Normalize one JSON value recursively.
pub fn value(value: Value, context: &NormalizeContext) -> Value {
    normalize_value(value, context)
}

/// Normalize free-form CLI text (stdout or stderr).
///
/// Text has no field names, so the semver rewrite applies here; JSON values
/// only rewrite `version` by field name.
pub fn text(input: &str, context: &NormalizeContext) -> String {
    normalize_string(input, context, true)
}

fn normalize_value(value: Value, context: &NormalizeContext) -> Value {
    match value {
        Value::Object(object) => {
            let mut normalized = Map::with_capacity(object.len());
            for (key, value) in object {
                // Object keys can carry volatile data too (the rollout
                // `patch_apply_end` payload keys its `changes` map by file
                // path, e.g. `/tmp/<cwd>/hello.txt`), so keys go through the
                // same string normalization as values. Field names like
                // `threadId` match no volatile pattern and survive intact.
                normalized.insert(
                    normalize_string(&key, context, false),
                    normalize_field(&key, value, context),
                );
            }
            Value::Object(normalized)
        }
        Value::Array(array) => Value::Array(
            array
                .into_iter()
                .map(|value| normalize_value(value, context))
                .collect(),
        ),
        Value::String(string) => Value::String(normalize_string(&string, context, false)),
        other => other,
    }
}

fn normalize_field(key: &str, value: Value, context: &NormalizeContext) -> Value {
    if key == "version" {
        return match value {
            Value::String(_) => Value::String("<version>".to_string()),
            other => other,
        };
    }

    if let Some(kind) = volatile_key(key)
        && let Some(raw) = scalar_text(&value)
    {
        return Value::String(context.placeholder(kind, &raw));
    }

    if let Value::String(string) = &value {
        if id_key(key) && uuid_regex().is_match(string) {
            return Value::String(context.placeholder("uuid", string));
        }
        // JSON-RPC `id` echoes are semantic, even when they are strings. Only
        // identifier fields whose values are known to be opaque get a
        // placeholder when they are not UUID-shaped.
        if id_key(key) && key != "id" && key != "uuid" {
            return Value::String(context.placeholder("id", string));
        }
    }

    normalize_value(value, context)
}

fn volatile_key(key: &str) -> Option<&'static str> {
    match key {
        "timestamp" | "time" | "ts" | "date" | "created_at" | "createdAt" | "updated_at"
        | "updatedAt" | "started_at" | "startedAt" | "finished_at" | "finishedAt"
        | "expires_at" | "expiresAt" => Some("timestamp"),
        "duration" | "duration_ms" | "durationMs" | "elapsed" | "elapsed_ms" | "elapsedMs"
        | "latency" | "latency_ms" | "latencyMs" | "took_ms" | "tookMs" => Some("duration"),
        "pid" | "process_id" | "processId" => Some("pid"),
        "request_id"
        | "requestId"
        | "provider_request_id"
        | "providerRequestId"
        | "x_request_id"
        | "xRequestId"
        | "req_id"
        | "reqId" => Some("request-id"),
        _ => None,
    }
}

fn id_key(key: &str) -> bool {
    matches!(
        key,
        "id" | "uuid"
            | "thread_id"
            | "threadId"
            | "session_id"
            | "sessionId"
            | "call_id"
            | "callId"
            | "tool_call_id"
            | "toolCallId"
            | "turn_id"
            | "turnId"
    )
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(string) => Some(string.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(boolean) => Some(boolean.to_string()),
        _ => None,
    }
}

fn normalize_string(input: &str, context: &NormalizeContext, versions: bool) -> String {
    let mut out = context.replace_temp_roots(input);
    if versions {
        out = version_regex()
            .replace_all(&out, "${1}<version>")
            .into_owned();
    }
    out
}

fn uuid_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$")
            .expect("valid UUID regex")
    })
}

fn version_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?m)^(subagent-mcp )\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?\b")
            .expect("valid version regex")
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn normalizes_documented_volatile_json_fields() {
        let context = NormalizeContext::new();
        let input = json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "threadId": "thread-1",
            "created_at": "2026-11-12T22:21:52Z",
            "duration_ms": 12,
            "pid": 4242,
            "request_id": "req_abc123",
            "version": "0.13.1",
            "protocolVersion": "2025-03-26",
            "model": "test-model"
        });
        let output = value(input, &context);
        assert_eq!(output["id"], "<uuid:1>");
        assert_eq!(output["threadId"], "<id:1>");
        assert_eq!(output["created_at"], "<timestamp:1>");
        assert_eq!(output["duration_ms"], "<duration:1>");
        assert_eq!(output["pid"], "<pid:1>");
        assert_eq!(output["request_id"], "<request-id:1>");
        assert_eq!(output["version"], "<version>");
        // Semantic fields must survive.
        assert_eq!(output["protocolVersion"], "2025-03-26");
        assert_eq!(output["model"], "test-model");
    }

    #[test]
    fn placeholders_are_stable_within_one_context() {
        let context = NormalizeContext::new();
        let input = json!({
            "threadId": "thread-1",
            "messages": [
                {"threadId": "thread-1"},
                {"threadId": "thread-2"}
            ]
        });
        let output = value(input, &context);
        assert_eq!(output["threadId"], output["messages"][0]["threadId"]);
        assert_ne!(output["threadId"], output["messages"][1]["threadId"]);
    }

    #[test]
    fn json_version_strings_are_normalized_by_field_name_only() {
        let context = NormalizeContext::new();
        let output = value(
            json!({"version": "0.13.1", "message": "requires 0.13.1"}),
            &context,
        );
        assert_eq!(output["version"], "<version>");
        assert_eq!(output["message"], "requires 0.13.1");
    }

    #[test]
    fn preserves_non_uuid_json_rpc_string_ids() {
        let context = NormalizeContext::new();
        let output = value(json!({"id": "request-1", "threadId": "thread-1"}), &context);
        assert_eq!(output["id"], "request-1");
        assert_eq!(output["threadId"], "<id:1>");
    }

    #[test]
    fn preserves_semantic_differences() {
        let context = NormalizeContext::new();
        let a = value(json!({"model": "model-a", "count": 1}), &context);
        let b = value(json!({"model": "model-b", "count": 2}), &context);
        assert_ne!(a, b);
        assert_ne!(
            value(
                json!({"error":"command timed out after 60s"}),
                &NormalizeContext::new()
            ),
            value(
                json!({"error":"command timed out after 600s"}),
                &NormalizeContext::new()
            )
        );
        let authored = json!({"text":"550e8400-e29b-41d4-a716-446655440000", "output":"2026-11-12T22:21:52Z request_id=req_abc123 pid=4242 60s"});
        assert_eq!(value(authored.clone(), &context), authored);
    }

    #[test]
    fn normalizes_cli_text() {
        let context = NormalizeContext::new().with_temp_root("/tmp/differential-home");
        let input = "subagent-mcp 0.13.1\nconfig   /tmp/differential-home/config.toml   OK\n  live   model   OK (tool call + follow-up succeeded, 1.234ms)\nrequest_id=req_abc123 pid=4242 at 2026-11-12T22:21:52Z\n";
        let output = text(input, &context);
        assert!(output.contains("subagent-mcp <version>"), "{output}");
        assert!(
            output.contains("config   <tmp>/config.toml   OK"),
            "{output}"
        );
        assert!(output.contains("1.234ms"), "{output}");
        assert!(
            output.contains("request_id=req_abc123 pid=4242 at 2026-11-12T22:21:52Z"),
            "{output}"
        );
    }

    #[test]
    fn preserves_uuid_inside_text() {
        let context = NormalizeContext::new();
        let output = text(
            "thread 550e8400-e29b-41d4-a716-446655440000 started",
            &context,
        );
        assert_eq!(
            output,
            "thread 550e8400-e29b-41d4-a716-446655440000 started"
        );
    }
}
