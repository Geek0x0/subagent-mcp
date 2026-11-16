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
pub const MAX_BODY_BYTES: usize = 100_000;

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
        Ok(items
            .iter()
            .filter_map(|item| {
                Some(ProjectRef {
                    id: item.get("id")?.as_str()?.to_string(),
                    canonical: PathBuf::from(item.get("canonical")?.as_str()?),
                })
            })
            .collect())
    }

    /// `POST /api/experimental/session/import`.
    pub async fn import(&self, body: &Value) -> Result<(), OpenCodeError> {
        let body = body.to_string();
        if body.len() > MAX_BODY_BYTES {
            return Err(OpenCodeError::TooLarge);
        }
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
        out.split_whitespace()
            .last()
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
        let text = format!("{} {}", stdout.trim(), stderr.trim());
        match http_status(&text) {
            Some(code) => Err(OpenCodeError::Status(code, text.trim().to_string())),
            None => Err(OpenCodeError::Other(text.trim().to_string())),
        }
    }
}

/// The number after the first `HTTP ` in `text` (`HTTP 409 Conflict` -> 409).
fn http_status(text: &str) -> Option<u16> {
    let rest = &text[text.find("HTTP ")? + "HTTP ".len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The `opencode` binary: the configured path when it exists, else the first
/// `opencode` (`opencode.exe` on Windows) on `PATH`.
pub fn find_opencode(cfg: &OpenCodeConfig) -> Option<PathBuf> {
    if let Some(bin) = &cfg.bin {
        return bin.is_file().then(|| bin.clone());
    }
    let name = if cfg!(windows) {
        "opencode.exe"
    } else {
        "opencode"
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
