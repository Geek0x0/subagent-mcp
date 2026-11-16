//! The OpenCode rollout against the REAL `opencode` on this machine.
//!
//! Ignored by default because it creates, replaces and deletes one session in the
//! user's real OpenCode. Run it on purpose, for example after an OpenCode upgrade:
//!
//! ```text
//! cargo test --test rollout_opencode_real -- --ignored --nocapture
//! ```
//!
//! It only uses the API path (`fallback = none`), never the database, and removes
//! the session it made even when an assertion fails.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use subagent_mcp::config::{OpenCodeConfig, OpenCodeFallback, OpenCodeMode};
use subagent_mcp::rollout::opencode::api::{ApiClient, find_opencode};
use subagent_mcp::rollout::opencode::flush::{Flusher, drain};
use subagent_mcp::rollout::opencode::transcript::{Transcript, new_session_id};

/// Deletes the test session on the way out, whatever happened.
struct Cleanup {
    bin: PathBuf,
    session_id: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new(&self.bin)
            .args(["session", "delete", &self.session_id])
            .output();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn snapshot(cwd: &std::path::Path, answer: &str) -> Transcript {
    let mut t = Transcript::new();
    let at = now_ms();
    t.push_line_at(
        "session_meta",
        &json!({"cwd": cwd, "model_provider": "roundtrip"}),
        at,
    );
    t.push_line_at("turn_context", &json!({"model": "roundtrip-model"}), at + 1);
    t.push_line_at(
        "response_item",
        &json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "round trip"}]}),
        at + 2,
    );
    t.push_line_at(
        "response_item",
        &json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": answer}]}),
        at + 3,
    );
    t.push_line_at("event_msg", &json!({"type": "task_complete"}), at + 4);
    t
}

#[tokio::test]
#[ignore = "creates, replaces and deletes one session in the real OpenCode"]
async fn real_opencode_import_list_delete_round_trip() {
    let config = OpenCodeConfig {
        mode: OpenCodeMode::Api,
        fallback: OpenCodeFallback::None,
        ..OpenCodeConfig::default()
    };
    let bin = find_opencode(&config).expect("`opencode` is not on PATH");
    let client = ApiClient::new(bin.clone(), Duration::from_secs(30));

    // Any real (non-global) OpenCode project will do as the session's directory.
    let projects = client.list_projects().await.expect("list projects");
    let project = projects
        .iter()
        .find(|p| p.canonical.parent().is_some() && p.canonical.is_dir())
        .expect("OpenCode has no usable project");
    let cwd = project.canonical.clone();

    let session_id = new_session_id(now_ms());
    let _cleanup = Cleanup {
        bin: bin.clone(),
        session_id: session_id.clone(),
    };
    let marker_one = format!("ROUNDTRIP-ONE-{}", now_ms());
    let marker_two = format!("ROUNDTRIP-TWO-{}", now_ms());
    let flusher = Flusher::new(config, cwd.clone(), session_id.clone())
        .with_api_timeout(Duration::from_secs(30));

    let list = || {
        let out = Command::new(&bin)
            .args(["session", "list"])
            .current_dir(&cwd)
            .output()
            .expect("opencode session list");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let messages = || {
        let out = Command::new(&bin)
            .args(["api", "GET", &format!("/api/session/{session_id}/message")])
            .output()
            .expect("opencode api GET");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // 1. The session appears in the project's session list.
    flusher.schedule(snapshot(&cwd, &marker_one));
    drain(Duration::from_secs(60)).await;
    assert!(
        flusher.take_warning().is_none(),
        "the first flush reported a problem"
    );
    assert_eq!(list().matches(&session_id).count(), 1, "listed once");
    assert!(messages().contains(&marker_one));

    // 2. A second flush replaces it: still one entry, now with the new answer.
    flusher.schedule(snapshot(&cwd, &marker_two));
    drain(Duration::from_secs(60)).await;
    assert!(
        flusher.take_warning().is_none(),
        "the replacement reported a problem"
    );
    assert_eq!(list().matches(&session_id).count(), 1, "still listed once");
    let after = messages();
    assert!(after.contains(&marker_two), "the new answer is stored");
    assert!(!after.contains(&marker_one), "the old copy is gone");

    // 3. Deleting it removes it (the guard repeats this if an assertion above failed).
    client.delete(&session_id).await.expect("delete");
    assert_eq!(list().matches(&session_id).count(), 0, "gone after delete");
}
