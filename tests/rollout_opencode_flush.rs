//! Background flushing: the API path, the database fallback, warnings, ordering.
//!
//! Everything runs against the fake `opencode` executable and temporary SQLite
//! files; the real OpenCode and its database are never touched.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};
use subagent_mcp::config::{OpenCodeConfig, OpenCodeFallback, OpenCodeMode};
use subagent_mcp::rollout::opencode::Recorder;
use subagent_mcp::rollout::opencode::flush::{Flusher, drain};
use subagent_mcp::rollout::opencode::transcript::Transcript;

#[path = "support/fake_opencode.rs"]
mod fake_opencode;
use fake_opencode::Fake;

/// The flusher counts pending sessions process wide, so the tests run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const T0: u64 = 1_794_800_000_000;
const OK: &str = r#"{"data":{}}"#;

fn project_json(cwd: &Path) -> String {
    json!([{"id": "prj_1", "canonical": cwd}]).to_string()
}

fn cfg(
    fake: &Fake,
    mode: OpenCodeMode,
    fallback: OpenCodeFallback,
    db: Option<&Path>,
) -> OpenCodeConfig {
    OpenCodeConfig {
        mode,
        fallback,
        bin: Some(fake.bin.clone()),
        db_path: db.map(Path::to_path_buf),
    }
}

/// A transcript whose assistant answer is `marker`, for a session in `cwd`.
fn snapshot(cwd: &Path, marker: &str) -> Transcript {
    let mut t = Transcript::new();
    t.push_line_at(
        "session_meta",
        &json!({"cwd": cwd, "model_provider": "magpie"}),
        T0,
    );
    t.push_line_at("turn_context", &json!({"model": "m"}), T0 + 1);
    t.push_line_at(
        "response_item",
        &json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}),
        T0 + 2,
    );
    t.push_line_at(
        "response_item",
        &json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": marker}]}),
        T0 + 3,
    );
    t.push_line_at("event_msg", &json!({"type": "task_complete"}), T0 + 4);
    t
}

fn new_cwd() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    (dir, cwd)
}

const DB_SCHEMA: &str = "
CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL);
INSERT INTO migration VALUES ('20261030054400_job', 0);
CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);
CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, slug TEXT NOT NULL,
    directory TEXT NOT NULL, title TEXT, version TEXT NOT NULL, cost REAL NOT NULL DEFAULT 0,
    tokens_input INTEGER NOT NULL DEFAULT 0, tokens_output INTEGER NOT NULL DEFAULT 0,
    tokens_reasoning INTEGER NOT NULL DEFAULT 0, tokens_cache_read INTEGER NOT NULL DEFAULT 0,
    tokens_cache_write INTEGER NOT NULL DEFAULT 0, agent TEXT, model TEXT,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
    seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
    data TEXT NOT NULL);
CREATE UNIQUE INDEX m_seq ON session_message (session_id, seq);
CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
";

/// A WAL database holding one project for `cwd` and, when `known_version` is
/// given, one older session so the version can be read from the database.
fn db_fixture(cwd: &Path, known_version: Option<&str>) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    let conn = Connection::open(&path).unwrap();
    let _: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    conn.execute_batch(DB_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO project (id, worktree) VALUES ('prj_1', ?1)",
        [cwd.to_str().unwrap()],
    )
    .unwrap();
    if let Some(version) = known_version {
        conn.execute(
            "INSERT INTO session_v2 (id, project_id, slug, directory, version, time_created, time_updated) \
             VALUES ('ses_old', 'prj_1', 's', '/x', ?1, 1, 1)",
            [version],
        )
        .unwrap();
    }
    (dir, path)
}

fn db_messages(path: &Path) -> Vec<Value> {
    let conn = Connection::open(path).unwrap();
    let mut stmt = conn
        .prepare("SELECT data FROM session_message ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|data| serde_json::from_str(&data.unwrap()).unwrap())
        .collect()
}

fn db_sessions(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM session_v2 WHERE id != 'ses_old'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// The JSON body of an `api POST .../import` call.
fn body_of(call: &[String]) -> Value {
    assert_eq!(call[1], "POST", "{call:?}");
    serde_json::from_str(call.last().unwrap()).unwrap()
}

fn is_import(call: &[String]) -> bool {
    call.get(2).is_some_and(|c| c.ends_with("/session/import"))
}

// --- the API path --------------------------------------------------------------

#[tokio::test]
async fn api_success_replaces_the_session() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0)
        .reply(2, "Session deleted", "", 0)
        .reply(3, OK, "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_flush1".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "marker-A"));
    drain(Duration::from_secs(10)).await;
    flusher.schedule(snapshot(&cwd, "marker-B"));
    drain(Duration::from_secs(10)).await;

    let calls = fake.api_calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert_eq!(calls[0], vec!["api", "GET", "/api/project"]);
    assert!(is_import(&calls[1]), "the first flush only imports");
    assert_eq!(calls[2], vec!["session", "delete", "ses_flush1"]);
    assert!(is_import(&calls[3]));
    assert!(calls[1].last().unwrap().contains("marker-A"));
    assert!(calls[3].last().unwrap().contains("marker-B"));
    assert_eq!(body_of(&calls[1])["info"]["id"], "ses_flush1");
    assert_eq!(body_of(&calls[1])["info"]["projectID"], "prj_1");
    assert!(flusher.take_warning().is_none());
}

#[tokio::test]
async fn api_failure_falls_back_to_the_db() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let (_dd, db) = db_fixture(&cwd, None);
    let fake = Fake::new().reply(0, &project_json(&cwd), "", 0).reply(
        1,
        "",
        "HTTP 500 Internal Server Error",
        1,
    );
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::Db, Some(&db)),
        cwd.clone(),
        "ses_flush2".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "marker-db"));
    drain(Duration::from_secs(10)).await;

    assert_eq!(db_sessions(&db), 1, "the session reached the database");
    let messages = db_messages(&db);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["content"][0]["text"], "marker-db");
    let warning = flusher.take_warning().expect("the API failure is reported");
    assert!(
        warning.contains("wrote the session to the database"),
        "{warning}"
    );
    assert_eq!(warning.lines().count(), 1);
    assert!(flusher.take_warning().is_none());
}

#[tokio::test]
async fn fallback_none_leaves_the_session_missing_and_warns() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0)
        .reply(2, "Session deleted", "", 0)
        .reply(3, "", "HTTP 500 boom", 1);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_flush3".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "one"));
    drain(Duration::from_secs(10)).await;
    assert!(flusher.take_warning().is_none());
    flusher.schedule(snapshot(&cwd, "two"));
    drain(Duration::from_secs(10)).await;

    let warning = flusher.take_warning().expect("a warning");
    assert!(warning.contains("missing"), "{warning}");
    assert!(flusher.take_warning().is_none(), "warned once");
}

#[tokio::test]
async fn import_failure_after_delete_restores_via_the_db() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let (_dd, db) = db_fixture(&cwd, None);
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0)
        .reply(2, "Session deleted", "", 0)
        .reply(3, "", "HTTP 500 boom", 1);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::Db, Some(&db)),
        cwd.clone(),
        "ses_flush4".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "first"));
    drain(Duration::from_secs(10)).await;
    flusher.schedule(snapshot(&cwd, "second"));
    drain(Duration::from_secs(10)).await;

    // The API holds nothing now, so the database restored the whole session.
    assert_eq!(db_sessions(&db), 1);
    assert_eq!(db_messages(&db)[1]["content"][0]["text"], "second");
    let warning = flusher.take_warning().unwrap();
    assert!(warning.contains("database instead"), "{warning}");
}

#[tokio::test]
async fn a_first_import_that_finds_the_id_deletes_and_retries() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, "", "HTTP 409 Conflict", 1)
        .reply(2, "Session deleted", "", 0)
        .reply(3, OK, "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_flush5".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "x"));
    drain(Duration::from_secs(10)).await;
    let calls = fake.api_calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(is_import(&calls[1]));
    assert_eq!(calls[2], vec!["session", "delete", "ses_flush5"]);
    assert!(is_import(&calls[3]));
    assert!(flusher.take_warning().is_none());
}

// --- the database path ---------------------------------------------------------

#[tokio::test]
async fn mode_db_never_spawns_opencode() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let (_dd, db) = db_fixture(&cwd, Some("2.3.0"));
    let fake = Fake::new();
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Db, OpenCodeFallback::None, Some(&db)),
        cwd.clone(),
        "ses_flush6".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "straight-to-db"));
    drain(Duration::from_secs(10)).await;

    assert!(fake.calls().is_empty(), "opencode must not be run");
    assert_eq!(db_sessions(&db), 1);
    assert!(flusher.take_warning().is_none());
}

#[tokio::test]
async fn missing_binary_warns_once_and_uses_the_db() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let (_dd, db) = db_fixture(&cwd, Some("2.3.0"));
    let config = OpenCodeConfig {
        mode: OpenCodeMode::Api,
        fallback: OpenCodeFallback::Db,
        bin: Some(PathBuf::from("/nonexistent/opencode")),
        db_path: Some(db.clone()),
    };
    let flusher = Flusher::new(config, cwd.clone(), "ses_flush7".to_string());
    flusher.schedule(snapshot(&cwd, "one"));
    drain(Duration::from_secs(10)).await;
    flusher.schedule(snapshot(&cwd, "two"));
    drain(Duration::from_secs(10)).await;

    assert_eq!(db_sessions(&db), 1);
    assert_eq!(db_messages(&db)[1]["content"][0]["text"], "two");
    let warning = flusher.take_warning().expect("a warning");
    assert_eq!(warning.lines().count(), 1, "warned once: {warning}");
    assert!(flusher.take_warning().is_none());
}

#[tokio::test]
async fn an_unknown_database_version_fails_the_db_path_cleanly() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let (_dd, db) = db_fixture(&cwd, None); // no session, so no version to read
    let fake = Fake::new();
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Db, OpenCodeFallback::None, Some(&db)),
        cwd.clone(),
        "ses_flush8".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "x"));
    drain(Duration::from_secs(10)).await;
    assert_eq!(db_sessions(&db), 0);
    assert!(flusher.take_warning().unwrap().contains("version unknown"));
}

// --- projects and warnings -----------------------------------------------------

#[tokio::test]
async fn no_project_match_skips_with_one_warning() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new().reply(0, "[]", "", 0).reply(1, "[]", "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_flush9".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "a"));
    drain(Duration::from_secs(10)).await;
    let warning = flusher.take_warning().expect("a warning");
    assert!(
        warning.contains(&format!("no OpenCode project for {}", cwd.display())),
        "{warning}"
    );
    // Later runs try again (a project may appear) but never warn twice.
    flusher.schedule(snapshot(&cwd, "b"));
    drain(Duration::from_secs(10)).await;
    assert!(flusher.take_warning().is_none());
    let calls = fake.api_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|c| !is_import(c)), "nothing was written");
}

#[tokio::test]
async fn warning_is_returned_once_and_a_failed_flusher_stops() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, "", "HTTP 500 boom", 1);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_flush10".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "a"));
    drain(Duration::from_secs(10)).await;
    assert!(flusher.take_warning().is_some());
    assert!(flusher.take_warning().is_none());
    // Every allowed path failed: later snapshots are dropped, nothing is retried.
    flusher.schedule(snapshot(&cwd, "b"));
    drain(Duration::from_secs(10)).await;
    assert_eq!(fake.api_calls().len(), 2);
    assert!(flusher.take_warning().is_none());
}

// --- ordering and timing ---------------------------------------------------------

#[tokio::test]
async fn rapid_second_run_only_writes_the_latest_snapshot() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .sleep(1, "1")
        .reply(1, OK, "", 0)
        .reply(2, "Session deleted", "", 0)
        .reply(3, OK, "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_flush11".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "marker-A"));
    tokio::time::sleep(Duration::from_millis(300)).await; // A's import is in flight
    flusher.schedule(snapshot(&cwd, "marker-B"));
    flusher.schedule(snapshot(&cwd, "marker-C"));
    drain(Duration::from_secs(15)).await;

    let calls = fake.api_calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(calls[1].last().unwrap().contains("marker-A"));
    assert_eq!(calls[2], vec!["session", "delete", "ses_flush11"]);
    assert!(calls[3].last().unwrap().contains("marker-C"));
    assert!(
        calls
            .iter()
            .all(|c| !c.last().unwrap().contains("marker-B")),
        "the superseded snapshot is never written"
    );
}

#[tokio::test]
async fn run_never_waits_for_the_flush() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .sleep(1, "2")
        .reply(1, OK, "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_flush12".to_string(),
    );
    let started = Instant::now();
    flusher.schedule(snapshot(&cwd, "x"));
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "schedule must return at once, took {:?}",
        started.elapsed()
    );
    drain(Duration::from_secs(15)).await;
    assert_eq!(fake.api_calls().len(), 2);
}

#[tokio::test]
async fn drain_waits_for_pending_flushes_up_to_the_timeout() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .sleep(1, "2")
        .reply(1, OK, "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_flush13".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "slow"));
    let started = Instant::now();
    drain(Duration::from_millis(300)).await;
    let short = started.elapsed();
    assert!(
        short >= Duration::from_millis(250) && short < Duration::from_millis(1500),
        "a short drain gives up at its timeout, took {short:?}"
    );
    drain(Duration::from_secs(15)).await;
    assert_eq!(fake.api_calls().len(), 2, "the flush finished once drained");
    assert!(started.elapsed() >= Duration::from_millis(1500));
}

// --- the recorder ------------------------------------------------------------------

#[tokio::test]
async fn run_ended_reaches_the_flusher_and_tool_errors_are_marked() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0);
    let mut recorder = Recorder::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "thread-1".to_string(),
    );
    recorder.push_line(
        "session_meta",
        &json!({"cwd": cwd, "model_provider": "magpie"}),
    );
    recorder.push_line(
        "response_item",
        &json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]}),
    );
    recorder.push_line(
        "response_item",
        &json!({"type": "function_call", "call_id": "c1", "name": "shell", "arguments": "{\"command\":\"false\"}"}),
    );
    recorder.push_line(
        "response_item",
        &json!({"type": "function_call_output", "call_id": "c1", "output": "boom"}),
    );
    recorder.mark_tool_error("c1", true);
    recorder.run_ended();
    drain(Duration::from_secs(10)).await;

    let calls = fake.api_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    let body = body_of(&calls[1]);
    let tool = &body["messages"][1]["content"][0];
    assert_eq!(tool["state"]["status"], "error");
    assert_eq!(tool["state"]["error"]["message"], "boom");
    assert!(recorder.take_warning().is_none());
}

#[tokio::test]
async fn an_empty_transcript_flushes_nothing() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new();
    let mut recorder = Recorder::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd,
        "thread-2".to_string(),
    );
    recorder.run_ended();
    drain(Duration::from_secs(5)).await;
    assert!(fake.api_calls().is_empty());
}

// --- fix round 1: data safety and version awareness --------------------------------

#[tokio::test]
async fn a_version_outside_the_tested_range_warns_but_still_writes() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .version("opencode v2.4.0")
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_fix1".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "x"));
    drain(Duration::from_secs(10)).await;
    let calls = fake.api_calls();
    assert_eq!(calls.len(), 2, "the API is still tried: {calls:?}");
    assert!(is_import(&calls[1]));
    let warning = flusher.take_warning().expect("a version warning");
    assert!(warning.contains("outside the tested range"), "{warning}");
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|c| *c == &vec!["--version"])
            .count(),
        1,
        "the version is read once"
    );
}

#[tokio::test]
async fn a_too_large_body_never_deletes_the_earlier_copy() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_fix2".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "small"));
    drain(Duration::from_secs(10)).await;
    assert!(flusher.take_warning().is_none());
    // The session has grown past what a command line can carry.
    flusher.schedule(snapshot(&cwd, &"x".repeat(120_000)));
    drain(Duration::from_secs(10)).await;

    let calls = fake.api_calls();
    assert_eq!(calls.len(), 2, "no delete and no second import: {calls:?}");
    assert!(
        calls
            .iter()
            .all(|c| c.get(1).map(String::as_str) != Some("delete"))
    );
    let warning = flusher.take_warning().expect("a warning");
    assert!(warning.contains("too large"), "{warning}");
    assert!(
        !warning.contains("missing"),
        "the earlier copy is intact: {warning}"
    );
}

#[tokio::test]
async fn a_delete_timeout_is_reported_as_possibly_missing() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0)
        .sleep(2, "3")
        .reply(2, "Session deleted", "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_fix3".to_string(),
    )
    .with_api_timeout(Duration::from_millis(500));
    flusher.schedule(snapshot(&cwd, "one"));
    drain(Duration::from_secs(10)).await;
    flusher.schedule(snapshot(&cwd, "two"));
    drain(Duration::from_secs(10)).await;
    let warning = flusher.take_warning().expect("a warning");
    assert!(
        warning.contains("missing"),
        "the server may have deleted it: {warning}"
    );
}

#[tokio::test]
async fn a_delete_timeout_is_restored_through_the_db() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let (_dd, db) = db_fixture(&cwd, None);
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0)
        .sleep(2, "3")
        .reply(2, "Session deleted", "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::Db, Some(&db)),
        cwd.clone(),
        "ses_fix4".to_string(),
    )
    .with_api_timeout(Duration::from_millis(500));
    flusher.schedule(snapshot(&cwd, "one"));
    drain(Duration::from_secs(10)).await;
    flusher.schedule(snapshot(&cwd, "two"));
    drain(Duration::from_secs(10)).await;
    assert_eq!(db_sessions(&db), 1, "the database restored the session");
    assert_eq!(db_messages(&db)[1]["content"][0]["text"], "two");
}

#[tokio::test]
async fn a_failed_db_fallback_after_the_delete_says_the_session_is_missing() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let missing_db = cwd.join("no-such-opencode.db");
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0)
        .reply(2, "Session deleted", "", 0)
        .reply(3, "", "HTTP 500 boom", 1);
    let flusher = Flusher::new(
        cfg(
            &fake,
            OpenCodeMode::Api,
            OpenCodeFallback::Db,
            Some(&missing_db),
        ),
        cwd.clone(),
        "ses_fix5".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "one"));
    drain(Duration::from_secs(10)).await;
    flusher.schedule(snapshot(&cwd, "two"));
    drain(Duration::from_secs(10)).await;
    let warning = flusher.take_warning().expect("a warning");
    assert!(warning.contains("missing from OpenCode"), "{warning}");
    assert!(warning.contains("database fallback failed"), "{warning}");
}

#[tokio::test]
async fn deleting_a_session_that_is_already_gone_is_fine() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0)
        // The user deleted the session in OpenCode in the meantime.
        .reply(2, "", "Session not found: ses_fix6", 1)
        .reply(3, OK, "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_fix6".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "one"));
    drain(Duration::from_secs(10)).await;
    flusher.schedule(snapshot(&cwd, "two"));
    drain(Duration::from_secs(10)).await;
    let calls = fake.api_calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(is_import(&calls[3]), "the import went ahead");
    assert!(flusher.take_warning().is_none());
}

#[tokio::test]
async fn a_snapshot_queued_during_a_failing_flush_is_not_retried() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .sleep(1, "1")
        .reply(1, "", "HTTP 500 boom", 1);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_fix7".to_string(),
    );
    flusher.schedule(snapshot(&cwd, "A"));
    tokio::time::sleep(Duration::from_millis(300)).await; // A's import is in flight
    flusher.schedule(snapshot(&cwd, "B"));
    drain(Duration::from_secs(15)).await;
    assert_eq!(fake.api_calls().len(), 2, "B must be dropped, not retried");
}

// --- fix round 2: the worker's claim ---------------------------------------------------

#[test]
fn a_task_dropped_before_its_first_poll_releases_the_session() {
    let _serial = SERIAL.blocking_lock();
    let (_d, cwd) = new_cwd();
    let fake = Fake::new()
        .reply(0, &project_json(&cwd), "", 0)
        .reply(1, OK, "", 0);
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Api, OpenCodeFallback::None, None),
        cwd.clone(),
        "ses_fix8".to_string(),
    );
    // The worker task is spawned but this runtime never polls it before it is dropped.
    let first = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    first.block_on(async {
        flusher.schedule(snapshot(&cwd, "never-written"));
    });
    drop(first);

    let second = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    second.block_on(async {
        let started = Instant::now();
        drain(Duration::from_secs(5)).await;
        assert!(
            started.elapsed() < Duration::from_millis(1000),
            "the dropped task must not keep the process 'pending', took {:?}",
            started.elapsed()
        );
        // The session is not stuck: a later snapshot starts a new worker and is written.
        flusher.schedule(snapshot(&cwd, "written"));
        drain(Duration::from_secs(10)).await;
    });
    let calls = fake.api_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[1].last().unwrap().contains("written"));
}

/// Best effort: the window between a worker finding nothing to do and ending is
/// a few instructions wide, so this cannot prove the absence of the race, only
/// catch a regression that widens it. The structure (`running` is cleared in the
/// critical section that sees no work) is what guards it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_last_snapshot_is_always_written() {
    let _serial = SERIAL.lock().await;
    let (_d, cwd) = new_cwd();
    let (_dd, db) = db_fixture(&cwd, Some("2.3.0"));
    let fake = Fake::new();
    let flusher = Flusher::new(
        cfg(&fake, OpenCodeMode::Db, OpenCodeFallback::None, Some(&db)),
        cwd.clone(),
        "ses_fix9".to_string(),
    );
    for round in 0..40 {
        for i in 0..5 {
            flusher.schedule(snapshot(&cwd, &format!("m-{round}-{i}")));
            tokio::task::yield_now().await;
        }
        drain(Duration::from_secs(20)).await;
        assert_eq!(
            db_messages(&db)[1]["content"][0]["text"],
            format!("m-{round}-4"),
            "round {round}: the newest snapshot must be the one written"
        );
    }
    assert!(flusher.take_warning().is_none());
}
