//! Rollout recorder golden tests (Task 21).
//!
//! Port of Go v0.13.1 `internal/rollout/rollout_test.go` and
//! `internal/agent/rollout_test.go`, plus the `session_meta` payload the Go
//! server writes in `internal/server/server.go` and the `provider_request`
//! event built in `internal/agent/runner.go`. The in-repo Go tree is normative.
//!
//! `session_meta` is written by the server in Go, not by the recorder, so this
//! suite constructs that line at the test call site (exactly as Go does) and
//! appends it to the rollout file directly; the recorder's API starts at turn
//! records. No public API is added for `session_meta`.
//!
//! Volatile fields (line timestamps, `current_date`, `task_started.started_at`)
//! are normalized to placeholders before comparing parsed JSON lines. All ids
//! in this suite are deterministic fixed strings, the equivalent of Go's UUIDs
//! after UUID normalization.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use subagent_mcp::VERSION;
use subagent_mcp::patch::{Change, ChangeKind};
use subagent_mcp::provider::{ToolCall, Usage};
use subagent_mcp::repo::git_info;
use subagent_mcp::rollout::RolloutRecorder;
use subagent_mcp::util::time::rfc3339_nano;

// ---------------------------------------------------------------------------
// Environment handling. These tests mutate process-wide env vars, so they are
// serialized under one mutex and each restores the values it changed.
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

const ENV_KEYS: [&str; 4] = ["CODEX_HOME", "SUBAGENT_MCP_ROLLOUT", "HOME", "USERPROFILE"];

struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    _lock: MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn new() -> Self {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved = ENV_KEYS
            .iter()
            .map(|&key| (key, std::env::var(key).ok()))
            .collect();
        EnvGuard { saved, _lock }
    }

    fn set(&self, key: &str, value: &str) {
        // SAFETY: env mutation is serialized under ENV_LOCK for the whole test.
        unsafe { std::env::set_var(key, value) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            // SAFETY: env mutation is serialized under ENV_LOCK.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(*key, v),
                    None => std::env::remove_var(*key),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// File helpers.
// ---------------------------------------------------------------------------

/// Recursively collect every `.jsonl` file beneath `dir`.
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_jsonl(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
}

/// Find the single rollout file for `thread_id` under `home`.
fn find_rollout(home: &Path, thread_id: &str) -> PathBuf {
    let mut all = Vec::new();
    collect_jsonl(&home.join("sessions"), &mut all);
    let suffix = format!("-{}.jsonl", thread_id);
    let matches: Vec<PathBuf> = all
        .into_iter()
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().ends_with(&suffix))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one rollout file for {}, got {:?}",
        thread_id,
        matches
    );
    matches[0].clone()
}

/// Parse every non-empty line of a rollout file as JSON.
fn read_lines(path: &Path) -> Vec<Value> {
    let data = fs::read_to_string(path).unwrap();
    data.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Append one `{timestamp, type, payload}` line directly to the file. Used at
/// the test call site to build `session_meta` exactly as the Go server does,
/// since the recorder itself never writes `session_meta`.
fn append_line(path: &Path, line_type: &str, payload: Value) {
    let line = json!({
        "timestamp": rfc3339_nano(SystemTime::now()),
        "type": line_type,
        "payload": payload,
    });
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", serde_json::to_string(&line).unwrap()).unwrap();
    file.flush().unwrap();
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Go's `lineKinds`: line type, plus `:payload.type` unless it is turn_context.
fn line_kinds(lines: &[Value]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            let t = line["type"].as_str().unwrap_or("").to_string();
            if t != "turn_context"
                && let Some(sub) = line["payload"]["type"].as_str()
            {
                format!("{}:{}", t, sub)
            } else {
                t
            }
        })
        .collect()
}

/// Replace volatile fields with stable placeholders before golden comparison.
fn normalize(lines: &mut [Value]) {
    for line in lines.iter_mut() {
        line["timestamp"] = json!("<TS>");
        match line["type"].as_str().unwrap_or("") {
            "session_meta" => line["payload"]["timestamp"] = json!("<TS>"),
            "turn_context" => line["payload"]["current_date"] = json!("<DATE>"),
            "event_msg" if line["payload"]["type"] == "task_started" => {
                line["payload"]["started_at"] = json!("<START>");
            }
            _ => {}
        }
    }
}

fn ln(line_type: &str, payload: Value) -> Value {
    json!({"timestamp": "<TS>", "type": line_type, "payload": payload})
}

fn usage(input: i64, cached: i64, output: i64, reasoning: i64, total: i64) -> Usage {
    Usage {
        input,
        cached,
        output,
        reasoning,
        total,
    }
}

// ---------------------------------------------------------------------------
// Step 1 golden tests: layout / modes / line shape / disabled / unwritable.
// ---------------------------------------------------------------------------

/// Port of Go `TestOpenLayoutAndWrite`.
#[test]
fn open_layout_modes_and_line_shape() {
    let _guard = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    _guard.set("CODEX_HOME", home.path().to_str().unwrap());
    _guard.set("SUBAGENT_MCP_ROLLOUT", "");

    let mut recorder = RolloutRecorder::create(
        "thread-1",
        Path::new("/ignored/cwd"),
        "ignored system prompt",
        "ignored provider",
        "ignored model",
        "ignored effort",
    )
    .expect("recorder should be created when rollout is enabled");

    let path = find_rollout(home.path(), "thread-1");

    // Path: $CODEX_HOME/sessions/YYYY/MM/DD/rollout-<ts>-thread-1.jsonl.
    let rel = path.strip_prefix(home.path()).unwrap();
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert_eq!(parts.len(), 5, "path = {:?}", rel);
    assert_eq!(parts[0], "sessions");
    for seg in &parts[1..4] {
        assert!(
            seg.chars().all(|c| c.is_ascii_digit()),
            "bad date seg {}",
            seg
        );
    }
    assert_eq!(parts[1].len(), 4, "year");
    assert_eq!(parts[2].len(), 2, "month");
    assert_eq!(parts[3].len(), 2, "day");

    // Filename: rollout-YYYY-MM-DDTHH-MM-SS-thread-1.jsonl, UTC date.
    let name = parts[4].clone();
    assert!(
        name.starts_with("rollout-") && name.ends_with("-thread-1.jsonl"),
        "name = {}",
        name
    );
    let stamp = &name["rollout-".len()..name.len() - "-thread-1.jsonl".len()];
    assert_eq!(stamp.len(), 19, "stamp = {}", stamp);
    let stamp_bytes = stamp.as_bytes();
    assert_eq!(stamp_bytes[4], b'-');
    assert_eq!(stamp_bytes[7], b'-');
    assert_eq!(stamp_bytes[10], b'T');
    assert_eq!(stamp_bytes[13], b'-');
    assert_eq!(stamp_bytes[16], b'-');
    assert_eq!(
        &stamp[..10],
        format!("{}-{}-{}", parts[1], parts[2], parts[3])
    );

    // UTC: the directory date is the UTC date now (allow a midnight rollover).
    let utc_now: chrono::DateTime<chrono::Utc> = SystemTime::now().into();
    let today = utc_now.format("%Y/%m/%d").to_string();
    let dir_date = format!("{}/{}/{}", parts[1], parts[2], parts[3]);
    assert!(
        dir_date == today
            || dir_date
                == utc_now
                    .checked_sub_signed(chrono::Duration::days(1))
                    .unwrap()
                    .format("%Y/%m/%d")
                    .to_string()
            || dir_date
                == utc_now
                    .checked_add_signed(chrono::Duration::days(1))
                    .unwrap()
                    .format("%Y/%m/%d")
                    .to_string(),
        "dir date {} is not UTC today {}",
        dir_date,
        today
    );

    #[cfg(unix)]
    {
        // 0700 directories (sessions, YYYY, MM, DD), 0600 file.
        let mut dir = path.parent().unwrap();
        for _ in 0..4 {
            assert_eq!(mode_of(dir), 0o700, "dir {} mode", dir.display());
            dir = dir.parent().unwrap();
        }
        assert_eq!(mode_of(&path), 0o600, "file mode");
    }

    // session_meta (constructed at the call site, exactly as Go's server does
    // here with the minimal rollout_test.go payload), then one response_item
    // and one event_msg from the recorder.
    append_line(&path, "session_meta", json!({"id": "thread-1"}));
    recorder.record_model_turn("t", "hi", None, None, &Usage::default());

    let lines = read_lines(&path);
    assert_eq!(lines.len(), 3, "lines = {:?}", lines);
    let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["session_meta", "response_item", "event_msg"]);
    for line in &lines {
        let ts = line["timestamp"].as_str().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
            "bad RFC3339Nano timestamp {}",
            ts
        );
        assert!(line["payload"].is_object(), "missing payload");
    }
}

/// Port of Go `TestOpenDisabled` and `TestHandleStartRolloutOffWritesNothing`.
#[test]
fn disabled_when_off() {
    let _guard = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    _guard.set("CODEX_HOME", home.path().to_str().unwrap());
    _guard.set("SUBAGENT_MCP_ROLLOUT", "off");

    let result = RolloutRecorder::create(
        "thread-off",
        Path::new("/cwd"),
        "sys",
        "prov",
        "model",
        "high",
    );
    assert!(result.is_err(), "SUBAGENT_MCP_ROLLOUT=off must disable");
    assert!(
        !home.path().join("sessions").exists(),
        "sessions dir created while rollout off"
    );
}

/// Port of Go `TestOpenUnwritableHomeReturnsNil`.
#[test]
fn unwritable_home_returns_error() {
    let _guard = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("not-a-dir");
    fs::write(&file, b"").unwrap();
    _guard.set("CODEX_HOME", file.to_str().unwrap());
    _guard.set("SUBAGENT_MCP_ROLLOUT", "");

    let result = RolloutRecorder::create(
        "thread-x",
        Path::new("/cwd"),
        "sys",
        "prov",
        "model",
        "high",
    );
    assert!(result.is_err(), "CODEX_HOME as a file must disable");
}

// ---------------------------------------------------------------------------
// Full turn golden. Reproduces the Go runner's complete line sequence plus a
// server-written session_meta and a trailing error event, and compares every
// parsed JSON line (timestamps normalized) against an exact golden vector.
// ---------------------------------------------------------------------------

#[test]
fn full_rollout_matches_go_golden() {
    let _guard = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    _guard.set("CODEX_HOME", home.path().to_str().unwrap());
    _guard.set("SUBAGENT_MCP_ROLLOUT", "");

    let cwd = home.path().join("project");
    fs::create_dir_all(cwd.join(".git/refs/heads")).unwrap();
    fs::write(cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(cwd.join(".git/refs/heads/main"), "abc123\n").unwrap();
    let cwd_str = cwd.display().to_string();

    let thread_id = "thread-1";
    let turn_id = "turn-1";
    let system_prompt = "You are a helpful subagent.";
    let provider = "deepseek";
    let model = "deepseek-v4-pro";
    let effort = "xhigh";
    let effort_sent = "max";
    let approval_policy = "never";
    let sandbox = "workspace-write";
    let writable_roots = vec![
        PathBuf::from("/allowed/root-a"),
        PathBuf::from("/allowed/root-b"),
    ];
    let prompt = "do it";

    let mut recorder =
        RolloutRecorder::create(thread_id, &cwd, system_prompt, provider, model, effort).unwrap();
    let path = find_rollout(home.path(), thread_id);

    // session_meta: constructed here, exactly as Go's server.go builds it
    // (non-auto selection, with git info from repo::git_info).
    let git = git_info(&cwd);
    let meta = json!({
        "session_id": thread_id,
        "id": thread_id,
        "timestamp": rfc3339_nano(SystemTime::now()),
        "cwd": cwd_str,
        "originator": "subagent-mcp",
        "cli_version": VERSION,
        "source": "mcp",
        "model_provider": provider,
        "base_instructions": {"text": system_prompt},
        "git": {
            "branch": git.branch.clone().unwrap_or_default(),
            "commit_hash": git.commit.clone().unwrap_or_default(),
        },
    });
    append_line(&path, "session_meta", meta);

    // Turn 1.
    recorder.record_turn_start(
        turn_id,
        &cwd,
        prompt,
        approval_policy,
        sandbox,
        Some(writable_roots.as_slice()),
        model,
        effort,
        Some(effort_sent),
    );
    recorder.record_provider_request(provider, model, 123, Some(45), None, None);

    let u = usage(10, 6, 4, 2, 14);
    let total1 = usage(10, 6, 4, 2, 14);
    let total2 = usage(20, 12, 8, 4, 28);
    recorder.record_model_turn(turn_id, "working", Some("plan"), Some(&u), &total1);

    let shell = ToolCall {
        id: "call-shell".into(),
        name: "shell".into(),
        arguments: r#"{"command":"echo hi"}"#.into(),
    };
    recorder.record_tool_call(turn_id, &shell, "shell");
    recorder.record_tool_output(turn_id, &shell, "hi", false);

    let patch_text = "*** Begin Patch\n*** Add File: n.txt\n+x\n*** End Patch";
    let apply_args = serde_json::to_string(&json!({"patch": patch_text})).unwrap();
    let apply_patch = ToolCall {
        id: "call-patch".into(),
        name: "apply_patch".into(),
        arguments: apply_args,
    };
    recorder.record_tool_call(turn_id, &apply_patch, "apply_patch");
    let changes = vec![
        Change {
            kind: ChangeKind::Add,
            path: PathBuf::from("n.txt"),
            move_to: None,
            new_content: Some(b"x\n".to_vec()),
            diff: String::new(),
        },
        Change {
            kind: ChangeKind::Update,
            path: PathBuf::from("src/a.go"),
            move_to: Some(PathBuf::from("src/b.go")),
            new_content: None,
            diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
        },
        Change {
            kind: ChangeKind::Delete,
            path: PathBuf::from("old.txt"),
            move_to: None,
            new_content: None,
            diff: String::new(),
        },
    ];
    recorder.record_patch_applied(turn_id, "call-patch", true, "Done!", "", &changes);
    recorder.record_tool_output(turn_id, &apply_patch, "Done!", false);

    // Turn 2.
    recorder.record_provider_request(provider, model, 88, None, Some("relace"), None);
    recorder.record_model_turn(turn_id, "all done", None, Some(&u), &total2);

    let started_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let completed_at = started_at + Duration::from_secs(3);
    recorder.record_task_complete(turn_id, "all done", started_at, completed_at);
    recorder.record_error("boom");

    // ---- golden ----
    let usage_payload = |i: i64, c: i64, o: i64, r: i64, t: i64| {
        json!({
            "input_tokens": i,
            "cached_input_tokens": c,
            "output_tokens": o,
            "reasoning_output_tokens": r,
            "total_tokens": t,
        })
    };
    let text_content = |kind: &str, text: &str| json!([{"type": kind, "text": text}]);

    let expected_changes = json!({
        "n.txt": {"type": "add", "move_path": null, "content": "x\n"},
        "src/a.go": {
            "type": "update",
            "move_path": "src/b.go",
            "unified_diff": "@@ -1 +1 @@\n-old\n+new\n",
        },
        "old.txt": {"type": "delete", "move_path": null},
    });

    let expected: Vec<Value> = vec![
        ln(
            "session_meta",
            json!({
                "session_id": thread_id,
                "id": thread_id,
                "timestamp": "<TS>",
                "cwd": cwd_str,
                "originator": "subagent-mcp",
                "cli_version": VERSION,
                "source": "mcp",
                "model_provider": provider,
                "base_instructions": {"text": system_prompt},
                "git": {"branch": "main", "commit_hash": "abc123"},
            }),
        ),
        ln(
            "turn_context",
            json!({
                "turn_id": turn_id,
                "cwd": cwd_str,
                "approval_policy": approval_policy,
                "sandbox_policy": {
                    "type": sandbox,
                    "writable_roots": ["/allowed/root-a", "/allowed/root-b"],
                },
                "model": model,
                "effort": effort,
                "effort_sent": effort_sent,
                "current_date": "<DATE>",
            }),
        ),
        ln(
            "event_msg",
            json!({"type": "task_started", "turn_id": turn_id, "started_at": "<START>"}),
        ),
        ln(
            "response_item",
            json!({"type": "message", "role": "user", "content": text_content("input_text", prompt)}),
        ),
        ln(
            "event_msg",
            json!({"type": "user_message", "message": prompt}),
        ),
        ln(
            "event_msg",
            json!({
                "type": "provider_request",
                "provider": provider,
                "model": model,
                "duration_ms": 123,
                "ttft_ms": 45,
            }),
        ),
        ln(
            "response_item",
            json!({"type": "reasoning", "summary": [], "content": text_content("reasoning_text", "plan")}),
        ),
        ln(
            "event_msg",
            json!({
                "type": "token_count",
                "info": {
                    "total_token_usage": usage_payload(10, 6, 4, 2, 14),
                    "last_token_usage": usage_payload(10, 6, 4, 2, 14),
                },
            }),
        ),
        ln(
            "response_item",
            json!({"type": "message", "role": "assistant", "content": text_content("output_text", "working")}),
        ),
        ln(
            "event_msg",
            json!({"type": "agent_message", "message": "working"}),
        ),
        ln(
            "response_item",
            json!({"type": "function_call", "call_id": "call-shell", "name": "shell", "arguments": r#"{"command":"echo hi"}"#}),
        ),
        ln(
            "response_item",
            json!({"type": "function_call_output", "call_id": "call-shell", "output": "hi"}),
        ),
        ln(
            "response_item",
            json!({
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": "call-patch",
                "name": "apply_patch",
                "input": patch_text,
            }),
        ),
        ln(
            "event_msg",
            json!({
                "type": "patch_apply_end",
                "call_id": "call-patch",
                "turn_id": turn_id,
                "success": true,
                "stdout": "Done!",
                "stderr": "",
                "changes": expected_changes,
            }),
        ),
        ln(
            "response_item",
            json!({"type": "custom_tool_call_output", "call_id": "call-patch", "output": "Done!"}),
        ),
        ln(
            "event_msg",
            json!({
                "type": "provider_request",
                "provider": provider,
                "model": model,
                "duration_ms": 88,
                "upstream": "relace",
            }),
        ),
        ln(
            "event_msg",
            json!({
                "type": "token_count",
                "info": {
                    "total_token_usage": usage_payload(20, 12, 8, 4, 28),
                    "last_token_usage": usage_payload(10, 6, 4, 2, 14),
                },
            }),
        ),
        ln(
            "response_item",
            json!({"type": "message", "role": "assistant", "content": text_content("output_text", "all done")}),
        ),
        ln(
            "event_msg",
            json!({"type": "agent_message", "message": "all done"}),
        ),
        ln(
            "event_msg",
            json!({
                "type": "task_complete",
                "turn_id": turn_id,
                "last_agent_message": "all done",
                "started_at": 1700000000i64,
                "completed_at": 1700000003i64,
                "duration_ms": 3000,
            }),
        ),
        ln("event_msg", json!({"type": "error", "message": "boom"})),
    ];

    let lines = read_lines(&path);
    let kinds = line_kinds(&lines);
    let want_kinds: Vec<&str> = vec![
        "session_meta",
        "turn_context",
        "event_msg:task_started",
        "response_item:message",
        "event_msg:user_message",
        "event_msg:provider_request",
        "response_item:reasoning",
        "event_msg:token_count",
        "response_item:message",
        "event_msg:agent_message",
        "response_item:function_call",
        "response_item:function_call_output",
        "response_item:custom_tool_call",
        "event_msg:patch_apply_end",
        "response_item:custom_tool_call_output",
        "event_msg:provider_request",
        "event_msg:token_count",
        "response_item:message",
        "event_msg:agent_message",
        "event_msg:task_complete",
        "event_msg:error",
    ];
    assert_eq!(kinds, want_kinds, "line kinds");

    // Targeted payload assertions (mirroring Go agent/rollout_test.go).
    assert_eq!(lines[1]["payload"]["effort"], "xhigh");
    assert_eq!(lines[1]["payload"]["effort_sent"], "max");
    assert_eq!(lines[1]["payload"]["model"], "deepseek-v4-pro");
    assert_eq!(
        lines[1]["payload"]["sandbox_policy"]["writable_roots"],
        json!(["/allowed/root-a", "/allowed/root-b"])
    );
    let info = &lines[16]["payload"]["info"];
    assert_eq!(info["total_token_usage"]["input_tokens"], 20);
    assert_eq!(info["total_token_usage"]["cached_input_tokens"], 12);
    assert_eq!(info["total_token_usage"]["reasoning_output_tokens"], 4);
    assert_eq!(info["last_token_usage"]["total_tokens"], 14);
    assert_eq!(lines[13]["payload"]["success"], true);
    assert_eq!(lines[19]["payload"]["last_agent_message"], "all done");
    assert_eq!(lines[5]["payload"]["provider"], "deepseek");
    assert_eq!(lines[5]["payload"]["model"], "deepseek-v4-pro");
    assert!(lines[5]["payload"]["duration_ms"].is_number());
    // user prompt item and user_message event.
    assert_eq!(lines[3]["payload"]["role"], "user");
    assert_eq!(lines[3]["payload"]["content"][0]["text"], "do it");
    assert_eq!(lines[4]["payload"]["message"], "do it");

    // Full normalized JSON equality.
    let mut normalized = lines.clone();
    normalize(&mut normalized);
    assert_eq!(normalized.len(), expected.len(), "line count");
    for (i, (got, want)) in normalized.iter().zip(expected.iter()).enumerate() {
        assert_eq!(got, want, "line {} mismatch", i);
    }
}

/// Go runner.go: on error, `provider_request` carries `error` and never
/// `upstream`; `ttft_ms` is still present when a first delta arrived.
#[test]
fn provider_request_error_suppresses_upstream() {
    let _guard = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    _guard.set("CODEX_HOME", home.path().to_str().unwrap());
    _guard.set("SUBAGENT_MCP_ROLLOUT", "");

    let mut recorder = RolloutRecorder::create(
        "thread-err",
        Path::new("/cwd"),
        "sys",
        "prov",
        "model",
        "high",
    )
    .unwrap();
    let path = find_rollout(home.path(), "thread-err");
    recorder.record_provider_request(
        "prov",
        "model",
        10,
        Some(5),
        Some("upstream-x"),
        Some("boom"),
    );

    let lines = read_lines(&path);
    assert_eq!(lines.len(), 1);
    let payload = &lines[0]["payload"];
    assert_eq!(lines[0]["type"], "event_msg");
    assert_eq!(payload["type"], "provider_request");
    assert_eq!(payload["error"], "boom");
    assert_eq!(payload["ttft_ms"], 5);
    assert_eq!(payload["duration_ms"], 10);
    assert!(
        payload.get("upstream").is_none(),
        "upstream must be suppressed on error"
    );
}

/// Go `internal/agent/session.go` `shellWritableRoots()` +
/// `internal/agent/rollout.go` `sandboxPolicy()`: `writable_roots` serializes
/// as JSON `null` for danger-full-access (nil slice) and for workspace-write
/// with zero roots (`append([]string(nil), ...)` stays nil), as `[]` for
/// read-only (empty non-nil slice), and as an array for workspace-write with
/// roots. The Rust recorder represents Go's nil slice with `None` and Go's
/// empty slice with `Some(&[])`.
#[test]
fn sandbox_policy_writable_roots_null_vs_empty() {
    let _guard = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    _guard.set("CODEX_HOME", home.path().to_str().unwrap());
    _guard.set("SUBAGENT_MCP_ROLLOUT", "");

    let roots = vec![
        PathBuf::from("/allowed/root-a"),
        PathBuf::from("/allowed/root-b"),
    ];
    let empty: &[PathBuf] = &[];

    let mut recorder = RolloutRecorder::create(
        "thread-roots",
        Path::new("/cwd"),
        "sys",
        "prov",
        "model",
        "high",
    )
    .unwrap();
    let path = find_rollout(home.path(), "thread-roots");

    // (label, sandbox type, roots option, expected JSON writable_roots)
    let cases: Vec<(&str, &str, Option<&[PathBuf]>, Value)> = vec![
        (
            "danger-full-access -> null",
            "danger-full-access",
            None,
            Value::Null,
        ),
        (
            "workspace-write with zero roots -> null",
            "workspace-write",
            None,
            Value::Null,
        ),
        ("read-only -> []", "read-only", Some(empty), json!([])),
        (
            "workspace-write with roots -> array",
            "workspace-write",
            Some(roots.as_slice()),
            json!(["/allowed/root-a", "/allowed/root-b"]),
        ),
    ];

    for (i, (_, sandbox, roots_opt, _)) in cases.iter().enumerate() {
        recorder.record_turn_start(
            &format!("turn-{}", i),
            Path::new("/cwd"),
            "p",
            "never",
            sandbox,
            *roots_opt,
            "model",
            "high",
            Some("high"),
        );
    }

    let lines = read_lines(&path);
    let contexts: Vec<&Value> = lines
        .iter()
        .filter(|l| l["type"] == "turn_context")
        .collect();
    assert_eq!(contexts.len(), cases.len(), "one turn_context per case");
    for (i, (label, sandbox, _, expected)) in cases.iter().enumerate() {
        let policy = &contexts[i]["payload"]["sandbox_policy"];
        assert_eq!(
            policy["type"].as_str(),
            Some(*sandbox),
            "case {label}: sandbox type"
        );
        assert_eq!(
            &policy["writable_roots"], expected,
            "case {label}: writable_roots"
        );
    }
}

/// Go `recordTurnStart` formats `current_date` from a local `time.Now()`, not
/// UTC. Verify the recorded value equals `chrono::Local`'s `YYYY-MM-DD` for the
/// instant of the call (before/after captures tolerate a midnight rollover),
/// so the assertion holds in any timezone without hardcoding a date.
#[test]
fn turn_context_current_date_is_local() {
    let _guard = EnvGuard::new();
    let home = tempfile::tempdir().unwrap();
    _guard.set("CODEX_HOME", home.path().to_str().unwrap());
    _guard.set("SUBAGENT_MCP_ROLLOUT", "");

    let mut recorder = RolloutRecorder::create(
        "thread-date",
        Path::new("/cwd"),
        "sys",
        "prov",
        "model",
        "high",
    )
    .unwrap();
    let path = find_rollout(home.path(), "thread-date");

    let empty: &[PathBuf] = &[];
    let before = chrono::Local::now().format("%Y-%m-%d").to_string();
    recorder.record_turn_start(
        "t",
        Path::new("/cwd"),
        "p",
        "never",
        "read-only",
        Some(empty),
        "model",
        "high",
        Some("high"),
    );
    let after = chrono::Local::now().format("%Y-%m-%d").to_string();

    let lines = read_lines(&path);
    let context = lines
        .iter()
        .find(|l| l["type"] == "turn_context")
        .expect("one turn_context line");
    let current_date = context["payload"]["current_date"].as_str().unwrap();
    assert_eq!(current_date.len(), 10, "YYYY-MM-DD format");
    assert!(
        current_date == before || current_date == after,
        "current_date {} is not the local date {} / {}",
        current_date,
        before,
        after
    );
}
