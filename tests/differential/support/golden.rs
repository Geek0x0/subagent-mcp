//! Golden-fixture helpers shared by the Task 27 scenario modules.
//!
//! The goldens under `tests/fixtures/` were captured once from the pinned Go
//! 0.13.1 binary by the Task 27 capture driver and are checked in so they stay
//! meaningful after the Go tree is deleted. This module loads them, applies the
//! documented volatile-field normalization (plus the few rollout-only volatile
//! keys the shared normalizer does not own), and provides the per-turn event
//! extraction with the required `agent_message_delta` collapsing.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::support::normalize::NormalizeContext;

/// Absolute path of a checked-in fixture, resolved from the crate root.
pub fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel)
}

/// Read a JSON fixture.
pub fn read_json(rel: &str) -> Value {
    let path = fixture(rel);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read fixture {}: {error}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("parse fixture {}: {error}", path.display()))
}

/// Read a JSONL fixture as parsed values, skipping empty lines.
pub fn read_jsonl(rel: &str) -> Vec<Value> {
    let path = fixture(rel);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read fixture {}: {error}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("golden JSONL line parses"))
        .collect()
}

/// Read a text fixture and assert it ends with a newline (all goldens do).
pub fn read_text(rel: &str) -> String {
    let path = fixture(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read fixture {}: {error}", path.display()))
}

/// Rollout-only volatile keys the shared normalizer does not own: the local
/// date the turn ran on, the epoch bookends of a turn, and the capturing
/// binary's version. `started_at` is written with whole-second precision, so
/// two turns in the same wall-clock second carry the same value and the
/// boundary itself is timing-dependent — it must not be compared for value.
fn rollout_volatile_key(key: &str) -> bool {
    matches!(
        key,
        "current_date" | "completed_at" | "started_at" | "cli_version"
    )
}

/// Timing fields whose *value* is timing-dependent even though their presence
/// is semantic. The shared normalizer gives equal values equal placeholders,
/// which real runs never are (e.g. `ttft_ms: 41` vs `ttft_ms: 1`), so these
/// collapse to one fixed placeholder before normalization.
pub fn timing_key(key: &str) -> bool {
    matches!(
        key,
        "duration_ms" | "durationMs" | "ttft_ms" | "ttftMs" | "elapsed_ms"
    )
}

/// Blank timing fields in one value (recursively), fixed placeholder.
pub fn blank_timing(value: Value) -> Value {
    const PLACEHOLDER: &str = "<timing>";
    match value {
        Value::Object(object) => {
            let mut out = serde_json::Map::with_capacity(object.len());
            for (key, value) in object {
                if timing_key(&key) && (value.is_number() || value.is_string()) {
                    out.insert(key, Value::String(PLACEHOLDER.to_string()));
                } else {
                    out.insert(key, blank_timing(value));
                }
            }
            Value::Object(out)
        }
        Value::Array(array) => Value::Array(array.into_iter().map(blank_timing).collect()),
        other => other,
    }
}

/// Normalize one value with the shared normalizer plus rollout extras. The
/// rollout-only keys collapse to a fixed placeholder: they are volatile by
/// construction, so positional equality between the golden and the Rust output
/// is the comparison, never the actual value.
pub fn normalize_value(value: Value, context: &NormalizeContext) -> Value {
    const PLACEHOLDER: &str = "<rollout-volatile>";
    fn blank(value: Value) -> Value {
        match value {
            Value::Object(object) => {
                let mut out = Map::with_capacity(object.len());
                for (key, value) in object {
                    if rollout_volatile_key(&key)
                        && !value.is_null()
                        && !value.is_object()
                        && !value.is_array()
                    {
                        out.insert(key, Value::String(PLACEHOLDER.to_string()));
                    } else {
                        out.insert(key, blank(value));
                    }
                }
                Value::Object(out)
            }
            Value::Array(array) => Value::Array(array.into_iter().map(blank).collect()),
            other => other,
        }
    }
    crate::support::normalize::value(blank(value), context)
}

/// Extract the `subagent/event` notification payloads of a golden MCP
/// transcript, in arrival order, as `msg` values.
pub fn golden_events(transcript: &[Value]) -> Vec<Value> {
    transcript
        .iter()
        .filter_map(|frame| {
            if frame.get("method")?.as_str()? == "subagent/event" {
                Some(frame["params"]["msg"].clone())
            } else {
                None
            }
        })
        .collect()
}

/// Collapse consecutive `agent_message_delta` events within a turn: their
/// `delta` texts concatenate into one event. This is the global constraint's
/// collapse rule, applied identically to golden and Rust event streams.
pub fn collapse_deltas(events: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(events.len());
    for event in events {
        let is_delta = event.get("type").and_then(Value::as_str) == Some("agent_message_delta");
        if is_delta
            && let Some(last) = out.last_mut()
            && last.get("type").and_then(Value::as_str) == Some("agent_message_delta")
        {
            let text = last["delta"].as_str().unwrap_or_default().to_string();
            let more = event["delta"].as_str().unwrap_or_default().to_string();
            last["delta"] = Value::String(text + &more);
            continue;
        }
        out.push(event);
    }
    out
}

/// The structured result payload of the tools/call response with `id`.
pub fn golden_result(transcript: &[Value], id: i64) -> Value {
    transcript
        .iter()
        .find(|frame| {
            frame.get("id").and_then(Value::as_i64) == Some(id) && frame.get("result").is_some()
        })
        .map(|frame| frame["result"]["structuredContent"].clone())
        .unwrap_or_else(|| panic!("golden transcript has no structured result for id {id}"))
}

/// Headers that differ between the Go HTTP client and the Rust HTTP client for
/// transport reasons only (client identity, connection handling, body length).
/// Also dropped: `x-stainless-*` SDK metadata (Go SDK runtime versions, never
/// subagent-mcp-authored) and an `accept: */*` value (the Go stdlib's default,
/// where the Rust transport sends none); adapter-set `accept` values such as
/// `text/event-stream` still compare.
pub fn volatile_header(name: &str) -> bool {
    if name.starts_with("x-stainless-") {
        return true;
    }
    matches!(
        name,
        "host" | "content-length" | "user-agent" | "accept-encoding" | "connection"
    )
}

/// Header values that are transport defaults, dropped with their header.
pub fn volatile_header_value(name: &str, value: &str) -> bool {
    name == "accept" && value == "*/*"
}

/// Prepare one wire-golden record for comparison: drop volatile headers, then
/// run the value normalizer. `context` should carry BOTH temp roots (the live
/// one and every root extracted from the golden by [`golden_temp_roots`]) so
/// paths inside body strings — such as tool output lines — normalize to `<tmp>`
/// on both sides.
pub fn normalize_wire(record: &Value, context: &NormalizeContext) -> Value {
    let mut out = record.clone();
    if let Some(headers) = out.get_mut("headers").and_then(Value::as_object_mut) {
        headers.retain(|name, value| {
            !volatile_header(name)
                && !value
                    .as_str()
                    .is_some_and(|v| volatile_header_value(name, v))
        });
    }
    crate::support::normalize::value(out, context)
}

/// Every `/tmp/<component>` path prefix appearing anywhere inside a golden
/// record. The capture directories are deleted, so the golden carries its own
/// temp roots; the live side adds its actual cwd. Both then normalize to
/// `<tmp>` in one shared context.
pub fn golden_temp_roots(value: &Value, out: &mut Vec<String>) {
    const TEMP_PATH_COMPONENT: &str = r"/tmp/[A-Za-z0-9][A-Za-z0-9._-]*";
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(TEMP_PATH_COMPONENT).expect("temp path regex"));
    match value {
        Value::String(string) => {
            for matched in re.find_iter(string) {
                let root = matched.as_str().to_string();
                if !out.contains(&root) {
                    out.push(root);
                }
            }
        }
        Value::Array(array) => {
            for item in array {
                golden_temp_roots(item, out);
            }
        }
        Value::Object(object) => {
            // Keys can carry temp-rooted paths too (e.g. the rollout
            // `patch_apply_end` `changes` map is keyed by file path).
            for key in object.keys() {
                golden_temp_roots(&Value::String(key.clone()), out);
            }
            for item in object.values() {
                golden_temp_roots(item, out);
            }
        }
        _ => {}
    }
}

/// Find the one rollout file under `home` whose name ends with
/// `-{thread_id}.jsonl` (the server names them `rollout-<ts>-<thread>.jsonl`).
pub fn find_rollout(home: &Path, thread_id: &str) -> PathBuf {
    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    out.push(path);
                }
            }
        }
    }
    let mut all = Vec::new();
    collect(home, &mut all);
    let suffix = format!("-{thread_id}.jsonl");
    let matches: Vec<_> = all
        .into_iter()
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().ends_with(&suffix))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(matches.len(), 1, "exactly one rollout file for {thread_id}");
    matches[0].clone()
}

/// Read a rollout file as parsed JSON lines, skipping empty lines.
pub fn read_rollout(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read rollout {}: {error}", path.display()))
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("rollout line parses"))
        .collect()
}

/// Compare live rollout lines (parsed from the recorder's file) to the golden
/// rollout JSONL, normalized. The live side gets the live home and cwd as its
/// temp roots; the golden side every `/tmp` root the capture left inside it.
pub fn assert_rollout_matches(got_home: &Path, got_cwd: &Path, got: Vec<Value>, golden_rel: &str) {
    let golden = read_jsonl(golden_rel);
    let mut roots = Vec::new();
    for line in &golden {
        golden_temp_roots(line, &mut roots);
    }
    let mut golden_context = NormalizeContext::new();
    for root in roots {
        golden_context = golden_context.with_temp_root(root);
    }
    let rust_context = NormalizeContext::new()
        .with_temp_root(got_home)
        .with_temp_root(got_cwd);
    let got: Vec<Value> = got
        .into_iter()
        .map(|line| normalize_value(blank_timing(line), &rust_context))
        .collect();
    let golden: Vec<Value> = golden
        .into_iter()
        .map(|line| normalize_value(blank_timing(line), &golden_context))
        .collect();
    assert_eq!(got, golden, "rollout lines differ from {golden_rel}");
}

/// Find the rollout file the server wrote for `thread_id` under `home` and
/// compare it to the golden rollout JSONL.
pub fn assert_rollout_for_thread_matches(
    home: &Path,
    cwd: &Path,
    thread_id: &str,
    golden_rel: &str,
) {
    let path = find_rollout(home, thread_id);
    assert_rollout_matches(home, cwd, read_rollout(&path), golden_rel);
}
