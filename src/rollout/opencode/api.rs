//! The official OpenCode HTTP API, reached through the `opencode api` client.
//!
//! Authentication stays with OpenCode: this module never reads a credential.
//! `opencode api -d` only accepts the request body as one command-line argument
//! (`@file`, `-` and `@-` are sent literally), so a body above [`MAX_BODY_BYTES`]
//! cannot be sent and is reported as [`OpenCodeError::TooLarge`] without
//! spawning anything.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use super::OpenCodeError;
use super::project::ProjectRef;
use crate::config::OpenCodeConfig;

/// The largest request body sent on a command line. Linux limits one argument
/// to about 128 KiB; this stays well below it.
#[cfg(not(windows))]
pub const MAX_BODY_BYTES: usize = 100_000;
/// Windows limits a whole command line to about 32,767 characters, and the JSON
/// is quoted again on the way: stay far below that. (Unverified on Windows; the
/// OpenCode format is supported on Linux and macOS.)
#[cfg(windows)]
pub const MAX_BODY_BYTES: usize = 12_000;

/// `TooLarge` when `body` cannot travel on a command line. Check this BEFORE
/// deleting an earlier copy of the session: a refused import after a delete
/// loses the session.
pub fn check_body_size(body: &Value) -> Result<(), OpenCodeError> {
    if body.to_string().len() > MAX_BODY_BYTES {
        Err(OpenCodeError::TooLarge)
    } else {
        Ok(())
    }
}

/// A client that shells out to `opencode`.
pub struct ApiClient {
    bin: PathBuf,
    timeout: Duration,
}

impl ApiClient {
    pub fn new(bin: PathBuf, timeout: Duration) -> Self {
        Self { bin, timeout }
    }

    /// `GET /api/project`: every project's id and canonical path.
    pub async fn list_projects(&self) -> Result<Vec<ProjectRef>, OpenCodeError> {
        let out = self.run(&["api", "GET", "/api/project"]).await?;
        let parsed: Value = serde_json::from_str(out.trim())
            .map_err(|e| OpenCodeError::Other(format!("project list is not JSON: {e}")))?;
        let items = parsed
            .as_array()
            .ok_or_else(|| OpenCodeError::Other("project list is not a JSON array".to_string()))?;
        let projects: Vec<ProjectRef> = items
            .iter()
            .filter_map(|item| {
                Some(ProjectRef {
                    id: item.get("id")?.as_str()?.to_string(),
                    canonical: PathBuf::from(item.get("canonical")?.as_str()?),
                })
            })
            .collect();
        // Entries without `id`/`canonical` are skipped, but a list in which none is
        // usable means the shape changed: say so instead of reporting "no project".
        if projects.is_empty() && !items.is_empty() {
            return Err(OpenCodeError::Other(format!(
                "none of the {} listed projects has an id and a canonical path",
                items.len()
            )));
        }
        Ok(projects)
    }

    /// `POST /api/experimental/session/import`.
    pub async fn import(&self, body: &Value) -> Result<(), OpenCodeError> {
        check_body_size(body)?;
        let body = body.to_string();
        self.run(&[
            "api",
            "POST",
            "/api/experimental/session/import",
            "-H",
            "content-type: application/json",
            "-d",
            &body,
        ])
        .await
        .map(|_| ())
    }

    /// `opencode --version`: stdout such as `opencode v2.3.0` -> `2.3.0`.
    pub async fn version(&self) -> Result<String, OpenCodeError> {
        let out = self.run(&["--version"]).await?;
        // The version is the last word of the first line; later lines (an upgrade
        // notice, for instance) are ignored.
        out.lines()
            .find(|line| !line.trim().is_empty())
            .and_then(|line| line.split_whitespace().last())
            .map(|word| word.trim_start_matches('v').to_string())
            .filter(|version| !version.is_empty())
            .ok_or_else(|| OpenCodeError::Other("opencode --version printed nothing".to_string()))
    }

    /// `opencode session delete <id>` (also deletes the session's children).
    pub async fn delete(&self, session_id: &str) -> Result<(), OpenCodeError> {
        self.run(&["session", "delete", session_id])
            .await
            .map(|_| ())
    }
    /// Run `opencode <args>` and return its stdout. A non-zero exit becomes
    /// `Status` when the output names an HTTP status, `Other` otherwise.
    async fn run(&self, args: &[&str]) -> Result<String, OpenCodeError> {
        let mut command = Command::new(&self.bin);
        // Same scrubbing as the shell tool: the provider API keys and the
        // `SUBAGENT_MCP_*` variables never reach another program.
        command
            .env_clear()
            .envs(crate::tools::shell::scrubbed_env());
        // A neutral working directory: the server's own is usually the repository the
        // agent works on, which must not influence another program.
        #[cfg(unix)]
        command.current_dir("/");
        command
            .args(args.iter().map(OsString::from))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => OpenCodeError::NotFound,
            _ => OpenCodeError::Spawn(e.to_string()),
        })?;
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| OpenCodeError::Timeout)?
            .map_err(|e| OpenCodeError::Other(e.to_string()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = format!("{}\n{}", stdout.trim(), stderr.trim());
        // `session delete` reports a missing session as `Session not found: <id>`
        // (no `HTTP 404` line, unlike `opencode api`); it may follow a banner line.
        let code = http_status(&text).or_else(|| {
            text.to_lowercase()
                .contains("session not found")
                .then_some(404)
        });
        match code {
            Some(code) => Err(OpenCodeError::Status(code, summary(&text))),
            None => Err(OpenCodeError::Other(summary(&text))),
        }
    }
}

/// The status of a line that starts with `HTTP ` (`HTTP 409 Conflict` -> 409).
/// Only a whole line counts: a JSON body that merely mentions "HTTP 409" does not.
fn http_status(text: &str) -> Option<u16> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("HTTP ")?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    })
}

/// A failure text as one short line: the first three non-empty lines joined, at
/// most 300 characters. It ends up in a warning shown to the caller, so it must
/// stay small; three lines keep a pretty-printed error body readable.
fn summary(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    if line.chars().count() > 300 {
        let cut: String = line.chars().take(300).collect();
        format!("{cut}...")
    } else {
        line
    }
}

/// The `opencode` binary: the configured path when it exists, else the first
/// `opencode` (`opencode.exe` on Windows) on `PATH`.
pub fn find_opencode(cfg: &OpenCodeConfig) -> Option<PathBuf> {
    // Absolute, because the child runs in `/`: a relative path would resolve there.
    let absolute = |path: PathBuf| std::path::absolute(&path).unwrap_or(path);
    if let Some(bin) = &cfg.bin {
        return bin.is_file().then(|| absolute(bin.clone()));
    }
    let name = if cfg!(windows) {
        "opencode.exe"
    } else {
        "opencode"
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map(absolute)
}
