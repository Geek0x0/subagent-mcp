//! The OpenCode database writer, against temporary SQLite files that mimic the
//! verified OpenCode v2.3.0 layout. The real OpenCode database is never touched.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use subagent_mcp::rollout::opencode::OpenCodeError;
use subagent_mcp::rollout::opencode::db::DbWriter;
use subagent_mcp::rollout::opencode::project::ProjectRef;

const SCHEMA: &str = "
CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL);
INSERT INTO migration VALUES ('20261030054400_job', 0);
CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL, vcs TEXT,
    time_created INTEGER NOT NULL DEFAULT 0, sandboxes TEXT NOT NULL DEFAULT '[]');
CREATE TABLE session_v2 (
    id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES project(id), workspace_id TEXT,
    parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, title TEXT,
    version TEXT NOT NULL, cost REAL NOT NULL DEFAULT 0,
    tokens_input INTEGER NOT NULL DEFAULT 0, tokens_output INTEGER NOT NULL DEFAULT 0,
    tokens_reasoning INTEGER NOT NULL DEFAULT 0, tokens_cache_read INTEGER NOT NULL DEFAULT 0,
    tokens_cache_write INTEGER NOT NULL DEFAULT 0, agent TEXT, model TEXT,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
    resume_attempts INTEGER NOT NULL DEFAULT 0);
CREATE TABLE session_message (
    id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session_v2(id) ON DELETE CASCADE,
    type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL,
    time_updated INTEGER NOT NULL, data TEXT NOT NULL);
CREATE UNIQUE INDEX session_message_session_seq_idx ON session_message (session_id, seq);
CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
CREATE TABLE account (id TEXT PRIMARY KEY, token TEXT NOT NULL);
INSERT INTO account VALUES ('acct', 'SENTINEL-TOKEN');
CREATE TABLE credential (id TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO credential VALUES ('cred', 'SENTINEL-SECRET');
CREATE TABLE blob (hash TEXT PRIMARY KEY, data BLOB NOT NULL);
INSERT INTO blob VALUES ('h', x'00ff');
INSERT INTO project (id, worktree, vcs) VALUES ('prj_1', '/work/repo', 'git');
INSERT INTO project (id, worktree) VALUES ('global', '/');
";

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    (dir, path)
}

fn body(session_id: &str, texts: &[&str]) -> Value {
    let mut messages = vec![json!({
        "id": format!("msg_{session_id}_0"), "type": "user", "time": {"created": 1000}, "text": texts[0]
    })];
    for (i, text) in texts.iter().enumerate().skip(1) {
        messages.push(json!({
            "id": format!("msg_{session_id}_{i}"), "type": "assistant",
            "time": {"created": 1000 + i as u64, "completed": 2000 + i as u64},
            "agent": "subagent-mcp", "model": {"id": "m", "providerID": "magpie"},
            "content": [{"type": "text", "text": text}], "finish": "stop", "cost": 0
        }));
    }
    json!({
        "info": {
            "id": session_id, "projectID": "prj_1", "agent": "subagent-mcp",
            "model": {"id": "m", "providerID": "magpie"}, "cost": 0,
            "tokens": {"input": 10, "output": 5, "reasoning": 2, "cache": {"read": 3, "write": 0}},
            "time": {"created": 1000, "updated": 3000}, "title": "subagent-mcp: t",
            "location": {"directory": "/work/repo"}
        },
        "messages": messages,
        "location": {"directory": "/work/repo"}
    })
}

fn writer(path: &Path) -> DbWriter {
    DbWriter::open(path, "2.3.0").unwrap()
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn master(conn: &Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare("SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn open_refuses_a_missing_file_and_does_not_create_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    assert!(matches!(
        DbWriter::open(&path, "2.3.0"),
        Err(OpenCodeError::NotFound)
    ));
    assert!(!path.exists(), "the database must never be created");
}

#[test]
fn open_refuses_an_unknown_schema() {
    // A newer shape: session_v2 now requires a column we do not know.
    let (_d, path) = fixture();
    Connection::open(&path)
        .unwrap()
        .execute_batch("ALTER TABLE session_v2 ADD COLUMN extra TEXT NOT NULL DEFAULT 'x'; ")
        .unwrap();
    // A defaulted extra column is fine ...
    assert!(DbWriter::open(&path, "2.3.0").is_ok());
    // ... but one without a default is an unknown required value.
    let (_d2, path2) = fixture();
    let conn = Connection::open(&path2).unwrap();
    conn.execute_batch(
        "DROP TABLE session_message; CREATE TABLE session_message (
            id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL,
            time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL,
            brand_new TEXT NOT NULL);",
    )
    .unwrap();
    assert!(matches!(
        DbWriter::open(&path2, "2.3.0"),
        Err(OpenCodeError::Schema(_))
    ));
}

#[test]
fn open_refuses_an_older_migration_and_a_missing_table() {
    let (_d, path) = fixture();
    Connection::open(&path)
        .unwrap()
        .execute_batch("UPDATE migration SET id = '20250101000000_old'")
        .unwrap();
    assert!(matches!(
        DbWriter::open(&path, "2.3.0"),
        Err(OpenCodeError::Schema(_))
    ));
    let (_d2, path2) = fixture();
    Connection::open(&path2)
        .unwrap()
        .execute_batch("DROP TABLE event_sequence")
        .unwrap();
    assert!(matches!(
        DbWriter::open(&path2, "2.3.0"),
        Err(OpenCodeError::Schema(_))
    ));
}

#[test]
fn open_refuses_a_read_only_database() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, path) = fixture();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    // A root user can still write; only assert the refusal when the file really is read-only.
    if std::fs::OpenOptions::new().write(true).open(&path).is_err() {
        assert!(DbWriter::open(&path, "2.3.0").is_err());
    }
}

#[test]
fn replace_session_inserts_session_and_messages() {
    let (_d, path) = fixture();
    let input = body("ses_000000000001aaaabbbbccccdd", &["hello", "world"]);
    writer(&path).replace_session(&input).unwrap();

    let conn = Connection::open(&path).unwrap();
    let (project, slug, dir, version, agent, model, input_t, created, updated): (
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT project_id, slug, directory, version, agent, model, tokens_input, \
             time_created, time_updated FROM session_v2 WHERE id = ?1",
            params!["ses_000000000001aaaabbbbccccdd"],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(project, "prj_1");
    assert_eq!(slug, "subagent-mcp-bbccccdd");
    assert_eq!(dir, "/work/repo");
    assert_eq!(version, "2.3.0");
    assert_eq!(agent, "subagent-mcp");
    assert_eq!(
        serde_json::from_str::<Value>(&model).unwrap(),
        json!({"id": "m", "providerID": "magpie"})
    );
    assert_eq!((input_t, created, updated), (10, 1000, 3000));

    let mut stmt = conn
        .prepare("SELECT id, type, seq, time_created, time_updated, data FROM session_message ORDER BY seq")
        .unwrap();
    let rows: Vec<(String, String, i64, i64, i64, Value)> = stmt
        .query_map([], |r| {
            let data: String = r.get(5)?;
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                serde_json::from_str(&data).unwrap(),
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str(), rows[0].2),
        ("msg_ses_000000000001aaaabbbbccccdd_0", "user", 1)
    );
    assert_eq!((rows[1].1.as_str(), rows[1].2), ("assistant", 2));
    assert_eq!((rows[1].3, rows[1].4), (1001, 2001), "created / completed");
    // `data` is the message without its id and type.
    assert_eq!(
        rows[0].5,
        json!({"time": {"created": 1000}, "text": "hello"})
    );
    assert!(rows[1].5.get("id").is_none() && rows[1].5.get("type").is_none());
    assert_eq!(rows[1].5["content"][0]["text"], "world");
}

#[test]
fn replace_session_replaces_previous_rows() {
    let (_d, path) = fixture();
    let w = writer(&path);
    w.replace_session(&body("ses_x", &["one", "two", "three"]))
        .unwrap();
    w.replace_session(&body("ses_x", &["only"])).unwrap();
    w.replace_session(&body("ses_other", &["keep"])).unwrap();
    let conn = Connection::open(&path).unwrap();
    assert_eq!(count(&conn, "session_v2"), 2);
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM session_message WHERE session_id = 'ses_x'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "the old messages must be gone");
    assert_eq!(count(&conn, "session_message"), 2);
}

#[test]
fn event_sequence_row_equals_the_last_seq() {
    let (_d, path) = fixture();
    writer(&path)
        .replace_session(&body("ses_x", &["a", "b", "c"]))
        .unwrap();
    let conn = Connection::open(&path).unwrap();
    let (seq, owner): (i64, Option<String>) = conn
        .query_row(
            "SELECT seq, owner_id FROM event_sequence WHERE aggregate_id = 'ses_x'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let max: i64 = conn
        .query_row(
            "SELECT max(seq) FROM session_message WHERE session_id = 'ses_x'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((seq, max), (3, 3));
    assert_eq!(owner, None);
    assert_eq!(count(&conn, "event_sequence"), 1, "replace keeps one row");
}

#[test]
fn touches_no_other_table() {
    let (_d, path) = fixture();
    let before = {
        let c = Connection::open(&path).unwrap();
        (master(&c), count(&c, "project"), count(&c, "migration"))
    };
    writer(&path).list_projects().unwrap();
    writer(&path)
        .replace_session(&body("ses_x", &["a", "b"]))
        .unwrap();
    let c = Connection::open(&path).unwrap();
    assert_eq!(
        master(&c),
        before.0,
        "no schema change (no table or index created)"
    );
    assert_eq!(
        (count(&c, "project"), count(&c, "migration")),
        (before.1, before.2)
    );
    assert_eq!(
        c.query_row("SELECT token FROM account", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "SENTINEL-TOKEN"
    );
    assert_eq!(
        c.query_row("SELECT value FROM credential", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "SENTINEL-SECRET"
    );
    assert_eq!(count(&c, "blob"), 1);
    assert_eq!(count(&c, "account"), 1);
    assert_eq!(count(&c, "credential"), 1);
}

#[test]
fn lock_contention_fails_fast_without_partial_writes() {
    let (_d, path) = fixture();
    // Another connection holds the write lock.
    let holder = Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = std::time::Instant::now();
    let result = writer(&path)
        .with_busy_timeout(Duration::from_millis(150))
        .replace_session(&body("ses_x", &["a", "b"]));
    assert_eq!(result.unwrap_err(), OpenCodeError::Locked);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "must give up quickly"
    );
    holder.execute_batch("ROLLBACK").unwrap();
    let conn = Connection::open(&path).unwrap();
    assert_eq!(count(&conn, "session_v2"), 0);
    assert_eq!(count(&conn, "session_message"), 0);
    assert_eq!(count(&conn, "event_sequence"), 0);
}

#[test]
fn a_failing_insert_rolls_the_whole_session_back() {
    let (_d, path) = fixture();
    writer(&path)
        .replace_session(&body("ses_x", &["a"]))
        .unwrap();
    // Two messages with the same id break the primary key mid-transaction.
    let mut bad = body("ses_x", &["a", "b"]);
    bad["messages"][1]["id"] = bad["messages"][0]["id"].clone();
    assert!(matches!(
        writer(&path).replace_session(&bad),
        Err(OpenCodeError::Other(_))
    ));
    let conn = Connection::open(&path).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM session_message WHERE session_id = 'ses_x'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "the previous copy must survive a failed replacement");
}

#[test]
fn list_projects_reads_the_project_table() {
    let (_d, path) = fixture();
    let mut projects = writer(&path).list_projects().unwrap();
    projects.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(
        projects,
        vec![
            ProjectRef {
                id: "global".into(),
                canonical: PathBuf::from("/")
            },
            ProjectRef {
                id: "prj_1".into(),
                canonical: PathBuf::from("/work/repo")
            },
        ]
    );
}
