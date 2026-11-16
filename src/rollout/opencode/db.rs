//! Writing an OpenCode session straight into OpenCode's SQLite database.
//!
//! This is the fallback when the official API is unavailable, and it writes a
//! database owned by another, possibly running, program. The rules are strict:
//! the file must already exist and be writable (it is never created); no
//! migration is run; the OpenCode version must be inside the tested range
//! (`2.3.x`, a leading `v` is tolerated); the schema is verified before every
//! write and an unknown shape is refused (an additive nullable or defaulted
//! column is tolerated, a new required column or an older `migration` id is
//! not); only `session_v2`, `session_message` and `event_sequence` are written
//! and only `project` and `migration` are read; one `BEGIN IMMEDIATE`
//! transaction per session; the journal mode is never changed.
//!
//! Two SQLite defaults need care. The bundled SQLite has foreign keys ON, and
//! the real OpenCode database has `ON DELETE CASCADE` children of `session_v2`
//! (`blob_ref`, `instruction_state`, ...) and of `event_sequence`, so every
//! connection turns foreign keys OFF right after opening: deleting the rows we
//! own can then never cascade into a table we do not own. And a closing
//! connection would checkpoint and delete the `-wal` file OpenCode relies on,
//! so every connection disables auto-checkpointing and checkpoint-on-close
//! (`SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE`); we ourselves never issue a checkpoint.
//! While OpenCode is not running the `-wal` file therefore keeps growing with
//! our writes (OpenCode folds it back on its own next commit), so a backup of
//! `opencode.db` alone would miss them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior, params};
use serde_json::Value;

use super::OpenCodeError;
use super::project::ProjectRef;

/// The migration the row layout below was verified against; a database whose
/// latest migration is older than this is refused.
const BASELINE_MIGRATION: &str = "20261030054400_job";
const DEFAULT_BUSY: Duration = Duration::from_secs(3);

const SESSION_COLUMNS: &[&str] = &[
    "id",
    "project_id",
    "slug",
    "directory",
    "title",
    "version",
    "cost",
    "tokens_input",
    "tokens_output",
    "tokens_reasoning",
    "tokens_cache_read",
    "tokens_cache_write",
    "time_created",
    "time_updated",
    "agent",
    "model",
];
const MESSAGE_COLUMNS: &[&str] = &[
    "id",
    "session_id",
    "type",
    "seq",
    "time_created",
    "time_updated",
    "data",
];
const SEQUENCE_COLUMNS: &[&str] = &["aggregate_id", "seq", "owner_id"];

/// `$OPENCODE_DB`, else `$XDG_DATA_HOME/opencode/opencode.db`, else
/// `~/.local/share/opencode/opencode.db`.
pub fn default_db_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OPENCODE_DB").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let data_home = match std::env::var_os("XDG_DATA_HOME").filter(|p| !p.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => crate::config::user_home()?.join(".local/share"),
    };
    Some(data_home.join("opencode/opencode.db"))
}

/// A validated handle on an OpenCode database file.
pub struct DbWriter {
    path: PathBuf,
    version: String,
    busy: Duration,
}

impl DbWriter {
    /// Check that `path` is an existing, writable OpenCode database of a known
    /// shape. `opencode_version` fills the `version` column of written sessions.
    pub fn open(path: &Path, opencode_version: &str) -> Result<Self, OpenCodeError> {
        if !super::in_tested_range(opencode_version) {
            return Err(OpenCodeError::Schema(format!(
                "OpenCode {opencode_version} is outside the tested range 2.3.x"
            )));
        }
        let release = opencode_version.trim();
        let release = release.strip_prefix('v').unwrap_or(release);
        let writer = Self {
            path: path.to_path_buf(),
            version: release.to_string(),
            busy: DEFAULT_BUSY,
        };
        writer.connect()?;
        Ok(writer)
    }

    /// The OpenCode version recorded on the newest session in the database
    /// (`session_v2.version`), for when the `opencode` binary is not used. Read
    /// only; the file must exist.
    pub fn detect_version(path: &Path) -> Result<String, OpenCodeError> {
        if !path.is_file() {
            return Err(OpenCodeError::NotFound);
        }
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(map_err)?;
        conn.busy_timeout(DEFAULT_BUSY).map_err(map_err)?;
        let version: Option<String> = conn
            .query_row(
                "SELECT version FROM session_v2 ORDER BY time_created DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(map_err(other)),
            })?;
        version.ok_or_else(|| OpenCodeError::Other("OpenCode version unknown".to_string()))
    }

    /// Override the lock wait (the default is 3 s).
    pub fn with_busy_timeout(mut self, busy: Duration) -> Self {
        self.busy = busy;
        self
    }

    /// Every project's id and root directory, from the read-only `project` table.
    pub fn list_projects(&self) -> Result<Vec<ProjectRef>, OpenCodeError> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT id, worktree FROM project")
            .map_err(map_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ProjectRef {
                    id: row.get(0)?,
                    canonical: PathBuf::from(row.get::<_, String>(1)?),
                })
            })
            .map_err(map_err)?;
        rows.collect::<Result<_, _>>().map_err(map_err)
    }

    /// Replace the session described by an import body (`{info, messages}`):
    /// delete its rows and insert the new ones in one transaction.
    pub fn replace_session(&self, body: &Value) -> Result<(), OpenCodeError> {
        let info = body
            .get("info")
            .ok_or_else(|| OpenCodeError::Other("import body has no info".to_string()))?;
        let session_id = text(info, "id")?;
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| OpenCodeError::Other("import body has no messages".to_string()))?;

        let mut conn = self.connect()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_err)?;
        // Foreign keys are off (see `connect`), so nothing cascades; children still
        // go first so the intermediate states stay consistent.
        for sql in [
            "DELETE FROM session_message WHERE session_id = ?1",
            "DELETE FROM event_sequence WHERE aggregate_id = ?1",
            "DELETE FROM session_v2 WHERE id = ?1",
        ] {
            tx.execute(sql, params![session_id]).map_err(map_err)?;
        }

        // Foreign keys are off, so the database no longer rejects a session whose
        // project does not exist; check it here, inside the transaction.
        let project_id = text(info, "projectID")?;
        let known: i64 = tx
            .query_row(
                "SELECT count(*) FROM project WHERE id = ?1",
                params![project_id],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        if known == 0 {
            return Err(OpenCodeError::Other(format!(
                "unknown OpenCode project {project_id}"
            )));
        }

        let tokens = info.get("tokens").cloned().unwrap_or(Value::Null);
        let time = info.get("time").cloned().unwrap_or(Value::Null);
        tx.execute(
            "INSERT INTO session_v2 (id, project_id, slug, directory, title, version, cost, \
             tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, \
             time_created, time_updated, agent, model) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                session_id,
                project_id,
                slug(&session_id),
                text_at(info, "/location/directory")?,
                info.get("title").and_then(Value::as_str),
                self.version,
                info.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
                int(&tokens, "/input"),
                int(&tokens, "/output"),
                int(&tokens, "/reasoning"),
                int(&tokens, "/cache/read"),
                int(&tokens, "/cache/write"),
                required_int(&time, "/created")?,
                required_int(&time, "/updated")?,
                info.get("agent").and_then(Value::as_str),
                info.get("model")
                    .filter(|model| !model.is_null())
                    .map(Value::to_string),
            ],
        )
        .map_err(map_err)?;

        for (index, message) in messages.iter().enumerate() {
            let seq = index as i64 + 1;
            let created = required_int(message, "/time/created")?;
            let updated = message
                .pointer("/time/completed")
                .and_then(Value::as_i64)
                .unwrap_or(created);
            let mut data = message.clone();
            if let Some(object) = data.as_object_mut() {
                object.remove("id");
                object.remove("type");
            }
            tx.execute(
                "INSERT INTO session_message (id, session_id, type, seq, time_created, \
                 time_updated, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    text(message, "id")?,
                    session_id,
                    text(message, "type")?,
                    seq,
                    created,
                    updated,
                    data.to_string(),
                ],
            )
            .map_err(map_err)?;
        }
        tx.execute(
            "INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?1, ?2, NULL)",
            params![session_id, messages.len() as i64],
        )
        .map_err(map_err)?;
        tx.commit().map_err(map_err)
    }

    /// Open the file (never creating it), set the lock wait, and verify the
    /// schema. Called for every operation, so a schema that changed under us is
    /// noticed.
    fn connect(&self) -> Result<Connection, OpenCodeError> {
        if !self.path.is_file() {
            return Err(OpenCodeError::NotFound);
        }
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_err)?;
        // SQLite silently falls back to read-only for an unwritable file.
        if conn.is_readonly("main").map_err(map_err)? {
            return Err(OpenCodeError::Other("database is read-only".to_string()));
        }
        conn.busy_timeout(self.busy).map_err(map_err)?;
        // Before anything else, and outside any transaction (a no-op inside one):
        // the bundled SQLite defaults to foreign keys ON.
        conn.pragma_update(None, "foreign_keys", false)
            .map_err(map_err)?;
        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .map_err(map_err)?;
        if foreign_keys != 0 {
            return Err(OpenCodeError::Other(
                "could not turn foreign keys off".to_string(),
            ));
        }
        // Never checkpoint: not automatically, and not when this connection closes.
        conn.pragma_update(None, "wal_autocheckpoint", 0)
            .map_err(map_err)?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
            .map_err(map_err)?;
        check_schema(&conn)?;
        Ok(conn)
    }
}

/// Refuse a database whose shape we have not verified.
fn check_schema(conn: &Connection) -> Result<(), OpenCodeError> {
    let latest: Option<String> = conn
        .query_row("SELECT max(id) FROM migration", [], |row| row.get(0))
        .map_err(|e| match &e {
            rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("no such table") => {
                OpenCodeError::Schema("no migration table".to_string())
            }
            _ => map_err(e),
        })?;
    match latest {
        Some(id) if id.as_str() >= BASELINE_MIGRATION => {}
        other => {
            return Err(OpenCodeError::Schema(format!(
                "latest migration {other:?} is older than {BASELINE_MIGRATION}"
            )));
        }
    }
    check_table(conn, "session_v2", SESSION_COLUMNS)?;
    check_table(conn, "session_message", MESSAGE_COLUMNS)?;
    check_table(conn, "event_sequence", SEQUENCE_COLUMNS)?;
    check_table(conn, "project", &["id", "worktree"])
}

/// `table` must exist with every column in `written` (or read), and must not
/// require a value for any column we do not supply.
fn check_table(conn: &Connection, table: &str, written: &[&str]) -> Result<(), OpenCodeError> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(map_err)?;
    // (name, not null, has default, part of the primary key)
    let columns: Vec<(String, bool, bool, bool)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, Option<String>>(4)?.is_some(),
                row.get::<_, i64>(5)? != 0,
            ))
        })
        .map_err(map_err)?
        .collect::<Result<_, _>>()
        .map_err(map_err)?;
    if columns.is_empty() {
        return Err(OpenCodeError::Schema(format!("table {table} is missing")));
    }
    let present: HashSet<&str> = columns.iter().map(|c| c.0.as_str()).collect();
    if let Some(missing) = written.iter().find(|c| !present.contains(**c)) {
        return Err(OpenCodeError::Schema(format!(
            "table {table} has no column {missing}"
        )));
    }
    // `project` is only read, so what it requires of inserts does not matter.
    if table != "project"
        && let Some((name, ..)) = columns.iter().find(|(name, not_null, has_default, pk)| {
            *not_null && !*has_default && !*pk && !written.contains(&name.as_str())
        })
    {
        return Err(OpenCodeError::Schema(format!(
            "table {table} requires a value for unknown column {name}"
        )));
    }
    Ok(())
}

/// `subagent-mcp-` plus the last 8 characters of the session id.
fn slug(session_id: &str) -> String {
    let tail: String = session_id
        .chars()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("subagent-mcp-{tail}")
}

fn text(value: &Value, key: &str) -> Result<String, OpenCodeError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| OpenCodeError::Other(format!("import body is missing {key}")))
}

/// An optional count: missing or not an integer means 0.
fn int(value: &Value, pointer: &str) -> i64 {
    value.pointer(pointer).and_then(Value::as_i64).unwrap_or(0)
}

/// A required integer (a timestamp): missing, a float, negative or out of `i64`
/// range is an error rather than a silent 1970.
fn required_int(value: &Value, pointer: &str) -> Result<i64, OpenCodeError> {
    value
        .pointer(pointer)
        .and_then(Value::as_i64)
        .filter(|n| *n >= 0)
        .ok_or_else(|| {
            OpenCodeError::Other(format!(
                "import body has no non-negative integer at {pointer}"
            ))
        })
}

fn text_at(value: &Value, pointer: &str) -> Result<String, OpenCodeError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| OpenCodeError::Other(format!("import body has no text at {pointer}")))
}

fn map_err(error: rusqlite::Error) -> OpenCodeError {
    if let rusqlite::Error::SqliteFailure(failure, _) = &error {
        match failure.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => return OpenCodeError::Locked,
            _ => {}
        }
    }
    OpenCodeError::Other(error.to_string())
}
