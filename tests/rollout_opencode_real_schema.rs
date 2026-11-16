//! The database writer against the REAL OpenCode table layout.
//!
//! Ignored by default because it reads the user's OpenCode database (read-only,
//! structure and the latest migration id only; no session data is read). Run it on
//! purpose, for example after an OpenCode upgrade:
//!
//! ```text
//! cargo test --test rollout_opencode_real_schema -- --ignored --nocapture
//! ```
//!
//! The writer runs on a temporary database built from that layout, never on the
//! real file.

#![cfg(unix)]

use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use subagent_mcp::rollout::opencode::db::{DbWriter, default_db_path};

#[test]
#[ignore = "reads the real OpenCode database layout"]
fn db_writer_works_on_the_real_table_layout() {
    let real_path = default_db_path().expect("a default OpenCode db path");
    let real = Connection::open_with_flags(&real_path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("opencode.db");
    let temp = Connection::open(&path).unwrap();
    let _mode: String = temp
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    let tables: Vec<String> = real
        .prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND sql IS NOT NULL")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for sql in &tables {
        temp.execute_batch(sql).unwrap();
    }
    let latest: String = real
        .query_row("SELECT max(id) FROM migration", [], |r| r.get(0))
        .unwrap();
    temp.execute(
        "INSERT INTO migration (id, time_completed) VALUES (?1, 0)",
        [&latest],
    )
    .unwrap();

    // One project row with a placeholder for every required column.
    let mut columns = Vec::new();
    let mut values = Vec::new();
    let mut stmt = temp.prepare("PRAGMA table_info(project)").unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (name, ty, notnull, default) = row.unwrap();
        if name == "id" {
            columns.push(name);
            values.push("'prj_1'".to_string());
        } else if notnull == 1 && default.is_none() {
            columns.push(name);
            values.push(
                if ty.to_uppercase().contains("INT") {
                    "0"
                } else {
                    "'/work/repo'"
                }
                .to_string(),
            );
        }
    }
    drop(stmt);
    temp.execute_batch(&format!(
        "INSERT INTO project ({}) VALUES ({})",
        columns.join(","),
        values.join(",")
    ))
    .unwrap();
    drop(temp);

    let body = |text: &str| {
        json!({
            "info": {
                "id": "ses_realschema", "projectID": "prj_1", "agent": "subagent-mcp",
                "model": {"id": "m", "providerID": "magpie"}, "cost": 0,
                "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}},
                "time": {"created": 1000, "updated": 3000}, "title": "t",
                "location": {"directory": "/work/repo"}
            },
            "messages": [
                {"id": "msg_a", "type": "user", "time": {"created": 1000}, "text": "q"},
                {"id": "msg_b", "type": "assistant", "time": {"created": 1001, "completed": 1002},
                 "agent": "subagent-mcp", "model": {"id": "m", "providerID": "magpie"},
                 "content": [{"type": "text", "text": text}], "finish": "stop", "cost": 0}
            ],
            "location": {"directory": "/work/repo"}
        })
    };
    let writer = DbWriter::open(&path, "2.3.0").expect("the real layout is accepted");
    writer.replace_session(&body("one")).unwrap();
    writer.replace_session(&body("two")).unwrap();

    let conn = Connection::open(&path).unwrap();
    let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(count("SELECT count(*) FROM session_v2"), 1);
    assert_eq!(count("SELECT count(*) FROM session_message"), 2);
    assert_eq!(count("SELECT seq FROM event_sequence"), 2);
}
