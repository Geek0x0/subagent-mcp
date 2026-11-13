//! Shared differential harness support.
//!
//! `ReferenceBinary` locates the pinned Go 0.13.1 artifact, `McpProcess` drives
//! a newline-delimited JSON-RPC stdio server, and [`normalize`] rewrites only
//! the documented volatile fields. Task 27 builds its scenarios on these
//! interfaces.

pub mod golden;
pub mod mock_http;
pub mod normalize;

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ring::digest::{SHA256, digest};
use serde::Deserialize;
use serde_json::{Value, json};

/// The pinned baseline manifest checked in at `tests/differential/baseline.lock`.
#[derive(Debug, Clone, Deserialize)]
pub struct BaselineManifest {
    pub version: String,
    pub commit: String,
    pub repo: String,
    pub artifact: String,
    pub sha256: String,
    pub binary: String,
}

impl BaselineManifest {
    /// Parse the checked-in manifest. A malformed manifest is a hard error.
    pub fn pinned() -> Result<Self, String> {
        toml::from_str(include_str!("../baseline.lock"))
            .map_err(|error| format!("tests/differential/baseline.lock: {error}"))
    }

    /// Release asset URL derived from the pinned repo, version, and artifact.
    pub fn artifact_url(&self) -> String {
        format!(
            "https://github.com/{}/releases/download/v{}/{}",
            self.repo, self.version, self.artifact
        )
    }
}

/// The downloaded reference binary plus the archive its checksum covers.
#[derive(Debug, Clone)]
pub struct ReferenceBinary {
    manifest: BaselineManifest,
    path: PathBuf,
    archive: PathBuf,
}

impl ReferenceBinary {
    /// Locate the pinned reference binary under `target/reference/`.
    ///
    /// `SUBAGENT_MCP_REFERENCE_DIR` overrides the directory and
    /// `SUBAGENT_MCP_REFERENCE_BIN` overrides the binary path (the archive is
    /// still expected beside the default directory for checksum verification).
    pub fn locate() -> Result<Self, String> {
        let manifest = BaselineManifest::pinned()?;
        let directory = reference_dir();
        let path = env::var_os("SUBAGENT_MCP_REFERENCE_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| directory.join(&manifest.binary));
        if !path.is_file() {
            return Err(format!(
                "pinned Go {} reference binary not found at {}; run `bash scripts/fetch-baseline.sh`",
                manifest.version,
                path.display()
            ));
        }
        Ok(Self {
            archive: directory.join(&manifest.artifact),
            manifest,
            path,
        })
    }

    /// The reference binary path.
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// The pinned manifest this reference was located with.
    pub fn manifest(&self) -> &BaselineManifest {
        &self.manifest
    }

    /// The downloaded release archive.
    pub fn archive_path(&self) -> PathBuf {
        self.archive.clone()
    }

    /// Verify the archive SHA-256 against the pinned manifest.
    pub fn verify_checksum(&self) -> Result<(), String> {
        let bytes = std::fs::read(&self.archive).map_err(|error| {
            format!(
                "read pinned archive {}: {error}; run `bash scripts/fetch-baseline.sh`",
                self.archive.display()
            )
        })?;
        let got = hex_lower(digest(&SHA256, &bytes).as_ref());
        let want = self.manifest.sha256.to_ascii_lowercase();
        if got != want {
            return Err(format!(
                "checksum mismatch for {}: expected {want}, got {got}",
                self.archive.display()
            ));
        }
        Ok(())
    }
}

fn reference_dir() -> PathBuf {
    env::var_os("SUBAGENT_MCP_REFERENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("reference")
        })
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Locate the reference for a differential test, or return `None` when the
/// artifact has not been fetched.
///
/// The differential CI job sets `SUBAGENT_MCP_REQUIRE_BASELINE=1`, which turns
/// a missing artifact into a failure instead of a silent skip.
pub fn reference_or_skip() -> Option<ReferenceBinary> {
    match ReferenceBinary::locate() {
        Ok(reference) => Some(reference),
        Err(reason) => {
            if baseline_required() {
                panic!("differential baseline required: {reason}");
            }
            eprintln!("skipping differential test: {reason}");
            None
        }
    }
}

fn baseline_required() -> bool {
    match env::var("SUBAGENT_MCP_REQUIRE_BASELINE") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// The Rust binary built for this test run.
pub fn rust_binary() -> &'static str {
    env!("CARGO_BIN_EXE_subagent-mcp")
}

/// Run one CLI process with a clean environment and a temporary `HOME`.
///
/// `argv[0]` is pinned to `subagent-mcp` for both binaries so Go's
/// `Usage of <argv0>:` line stays comparable.
pub fn run_cli(binary: &Path, args: &[&str], envs: &[(&str, &str)], home: &Path) -> Output {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new(binary);
    command
        .arg0("subagent-mcp")
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .current_dir(home);
    for (name, value) in envs {
        command.env(name, value);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", binary.display()))
}

/// Set `name` to `value` for the scope of this guard, restoring the previous
/// value (or its absence) on drop.
///
/// Task 27 review finding: `rollout.rs` mutated `CODEX_HOME` process-wide and
/// never restored it. Every env mutation in the differential suite now goes
/// through this guard, so a later `CODEX_HOME`-respecting scenario cannot
/// become a heisenbug. SAFETY: callers serialize environment mutation behind
/// their module's `SCENARIO_LOCK`.
pub struct EnvGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    pub fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = env::var_os(name);
        // SAFETY: callers serialize environment mutation behind a scenario lock.
        unsafe { env::set_var(name, value) };
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: same lock discipline as `EnvGuard::set`.
        unsafe {
            match &self.previous {
                Some(value) => env::set_var(self.name, value),
                None => env::remove_var(self.name),
            }
        }
    }
}

/// A newline-delimited JSON-RPC MCP process.
///
/// One reader thread parses stdout frames; [`McpProcess::request`] waits for
/// the response with the matching id and stores every other frame (server
/// notifications or requests) for [`McpProcess::notifications`].
pub struct McpProcess {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    frames: Receiver<Result<Value, String>>,
    notifications: Vec<Value>,
    stderr: Arc<Mutex<String>>,
    timeout: Duration,
}

impl McpProcess {
    /// Start an MCP server. `env` is applied on top of a clean environment
    /// containing only `PATH`; `HOME` also becomes the working directory.
    pub fn start(binary: &Path, args: &[&str], env: &[(&str, &str)]) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new(binary);
        command
            .arg0("subagent-mcp")
            .args(args)
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap_or_default());
        let mut home: Option<PathBuf> = None;
        for (name, value) in env {
            command.env(name, value);
            if *name == "HOME" {
                home = Some(PathBuf::from(value));
            }
        }
        if let Some(home) = home {
            command.current_dir(home);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("MCP stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("MCP stdout was not piped"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("MCP stderr was not piped"))?;

        let (sender, frames) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let frame = serde_json::from_str::<Value>(&line)
                            .map_err(|error| format!("invalid JSON frame {line:?}: {error}"));
                        if sender.send(frame).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(format!("read MCP stdout: {error}")));
                        break;
                    }
                }
            }
        });

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_buffer = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => stderr_buffer.lock().unwrap().push_str(&line),
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            frames,
            notifications: Vec::new(),
            stderr,
            timeout: Duration::from_secs(30),
        })
    }

    /// Override the per-response wait timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Send a request and return the response with the same id.
    ///
    /// Frames without a matching id are retained for
    /// [`McpProcess::notifications`].
    pub fn request(&mut self, request: Value) -> Value {
        self.request_with_hook(request, |_| None)
    }

    /// Send a request and return the response with the same id, running
    /// `hook` for every non-response frame observed while waiting (in arrival
    /// order). A hook may answer with a frame to send back (e.g. a
    /// `notifications/cancelled` the instant the first `subagent/event`
    /// arrives) or `None` to do nothing.
    pub fn request_with_hook(
        &mut self,
        request: Value,
        mut hook: impl FnMut(&Value) -> Option<Value>,
    ) -> Value {
        let id = request
            .get("id")
            .cloned()
            .unwrap_or_else(|| panic!("request has no id: {request}"));
        self.send(&request).unwrap_or_else(|error| {
            panic!(
                "write MCP request {request}: {error}; stderr:\n{}",
                self.stderr()
            )
        });

        loop {
            match self.frames.recv_timeout(self.timeout) {
                Ok(Ok(frame)) => {
                    if frame.get("method").is_none() && frame.get("id") == Some(&id) {
                        return frame;
                    }
                    if let Some(reply) = hook(&frame) {
                        self.send(&reply).unwrap_or_else(|error| {
                            panic!("write MCP hook frame: {error}; stderr:\n{}", self.stderr())
                        });
                    }
                    self.notifications.push(frame);
                }
                Ok(Err(error)) => panic!("{error}; stderr:\n{}", self.stderr()),
                Err(RecvTimeoutError::Timeout) => panic!(
                    "timed out after {:?} waiting for response to {request}; stderr:\n{}",
                    self.timeout,
                    self.stderr()
                ),
                Err(RecvTimeoutError::Disconnected) => panic!(
                    "MCP server closed stdout while waiting for response to {request}; stderr:\n{}",
                    self.stderr()
                ),
            }
        }
    }

    /// Send a notification (or any frame without a response) and flush it.
    pub fn notify(&mut self, notification: Value) -> io::Result<()> {
        self.send(&notification)
    }

    /// Synchronization barrier between `notifications/initialized` and the
    /// first `tools/call`: send a `ping` and wait for its response.
    ///
    /// mcp-go has an initialized-session race that can drop notifications
    /// racing the first request; the pong proves the session is fully live.
    /// The pong is harness plumbing, so it is neither returned nor recorded —
    /// goldens do not contain it.
    pub fn initialized_session_barrier(&mut self) {
        const PING_ID: i64 = 4; // 1 initialize, 2 tools/call, 3 budget follow-up
        let pong = self.request(json!({
            "jsonrpc": "2.0",
            "id": PING_ID,
            "method": "ping"
        }));
        assert_eq!(pong["id"], PING_ID, "barrier ping was not answered");
    }

    /// Drain non-response frames until `quiet` has passed with no new frame,
    /// bounded by `overall`. Returns the drained frames in arrival order (they
    /// are not retained for [`McpProcess::take_notifications`]).
    ///
    /// This captures notifications that legally trail the response they
    /// belong to (e.g. Go writes `subagent/event` frames after the
    /// `tools/call` result) without imposing any sleep on the compared
    /// binaries: the quiet window only ever shortens once the server is
    /// genuinely done.
    pub fn drain(&mut self, quiet: Duration, overall: Duration) -> Vec<Value> {
        let start = Instant::now();
        let mut drained = Vec::new();
        loop {
            let left = overall.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            match self.frames.recv_timeout(quiet.min(left)) {
                Ok(Ok(frame)) => drained.push(frame),
                Ok(Err(error)) => panic!("invalid MCP frame while draining: {error}"),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        drained
    }

    fn send(&mut self, frame: &Value) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::other("MCP stdin is closed"))?;
        serde_json::to_writer(&mut *stdin, frame)?;
        stdin.write_all(b"\n")?;
        stdin.flush()
    }

    /// Every non-response frame observed while waiting for responses.
    pub fn notifications(&self) -> Vec<Value> {
        self.notifications.clone()
    }

    /// Drain the stored non-response frames.
    pub fn take_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.notifications)
    }

    /// Captured stderr so far.
    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Close stdin (the server sees EOF) and wait briefly for exit.
    pub fn close(mut self) {
        drop(self.stdin.take());
        let Some(mut child) = self.child.take() else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;

    #[test]
    fn trailing_frames_are_not_repeated_on_followup() {
        let (tx, frames) = mpsc::channel();
        let mut process = McpProcess {
            child: None,
            stdin: None,
            frames,
            notifications: vec![],
            stderr: Arc::new(Mutex::new(String::new())),
            timeout: Duration::from_secs(1),
        };
        let trailing = json!({"method":"subagent/event","params":{"call":1}});
        tx.send(Ok(trailing.clone())).unwrap();
        let first = process.drain(Duration::from_millis(1), Duration::from_millis(10));
        assert_eq!(first, vec![trailing]);
        assert!(
            process.take_notifications().is_empty(),
            "followup must not repeat first call's trailing frames"
        );
        let second = json!({"method":"subagent/event","params":{"call":2}});
        tx.send(Ok(second.clone())).unwrap();
        assert_eq!(
            process.drain(Duration::from_millis(1), Duration::from_millis(10)),
            vec![second]
        );
        assert!(process.take_notifications().is_empty());
    }
}
