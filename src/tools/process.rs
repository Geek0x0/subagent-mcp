//! Platform-neutral command specification and managed-command dispatch.
//!
//! `CommandSpec` describes a child process before it is spawned so a
//! [`crate::tools::shell::SandboxLauncher`] can rewrite it (Task 20). The actual
//! blocking spawn/wait lives in the platform backends, which are reached through
//! [`run_managed_command`]; the shell tool runs that in `spawn_blocking` so the
//! async runtime is never blocked.
//!
//! Both backends drain stdout/stderr into a single [`LimitedBuffer`] that caps
//! the kept bytes during draining (never buffering unbounded output) and counts
//! every byte seen so the exact Go truncation marker can be produced. The final
//! capped/marked bytes are returned in `Output.stdout` (with `Output.stderr`
//! empty) — Task 18 must keep this contract when it replaces the Windows backend.

use crate::tools::shell::MAX_OUTPUT_BYTES;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The exact child command to launch. `env` is the *complete* environment (the
/// caller already scrubbed it); `build_command` calls `env_clear` before applying
/// it, matching Go's `cmd.Env = ScrubbedEnv()`.
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
    pub stdout: Stdio,
    pub stderr: Stdio,
}

/// Go `limitedBuffer`. Both stdout and stderr feed the *same* buffer (Go reuses
/// one pipe when `Stdout == Stderr`). It keeps only the first `MAX_OUTPUT_BYTES`
/// bytes, but counts every byte written so `finish` can append the exact Go
/// truncation marker. Draining always continues — bytes past the cap are counted
/// and discarded, never left in the pipe (which would deadlock the child).
pub(crate) struct LimitedBuffer {
    kept: Vec<u8>,
    total: usize,
}

impl LimitedBuffer {
    pub(crate) fn new() -> Self {
        Self {
            kept: Vec::new(),
            total: 0,
        }
    }

    /// Count all bytes and keep only the first `MAX_OUTPUT_BYTES` of the stream.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len();
        let remaining = MAX_OUTPUT_BYTES.saturating_sub(self.kept.len());
        if remaining > 0 {
            let n = remaining.min(chunk.len());
            self.kept.extend_from_slice(&chunk[..n]);
        }
    }

    /// Go `limitedBuffer.String()`: the kept bytes plus, when truncated, the
    /// `\n[output truncated: N bytes total]` marker. Returned as raw bytes; the
    /// shell layer converts to a `String` with `from_utf8_lossy`.
    pub(crate) fn finish(&self) -> Vec<u8> {
        let mut out = self.kept.clone();
        if self.total > MAX_OUTPUT_BYTES {
            out.extend_from_slice(
                format!("\n[output truncated: {} bytes total]", self.total).as_bytes(),
            );
        }
        out
    }
}

/// Sentinel string for the cancellation error.
pub(crate) const CANCELLED: &str = "subagent-mcp: managed command cancelled";

/// Payload for the cancellation sentinel. It carries the captured/truncated
/// output so `run_shell` can preserve Go's `(out, code, err)` semantics on the
/// killed path instead of dropping the bytes the command already produced.
#[derive(Debug)]
pub(crate) struct Cancelled {
    pub output: Vec<u8>,
}

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(CANCELLED)
    }
}
impl std::error::Error for Cancelled {}

/// Build the cancellation error, retaining the captured output bytes.
pub(crate) fn cancelled_error(output: Vec<u8>) -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, Cancelled { output })
}

/// True when `run_managed_command` had to kill a still-running direct child.
pub(crate) fn is_cancelled(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
        && err
            .get_ref()
            .and_then(|e| e.downcast_ref::<Cancelled>())
            .is_some()
}

/// Recover the captured output bytes from a cancellation error.
pub(crate) fn cancelled_output(err: &io::Error) -> Vec<u8> {
    err.get_ref()
        .and_then(|e| e.downcast_ref::<Cancelled>())
        .map(|c| c.output.clone())
        .unwrap_or_default()
}

/// Build a `std::process::Command` from a `CommandSpec`. Platform-neutral; the
/// platform backends add process-group / job-object handling before spawn.
pub(crate) fn build_command(spec: CommandSpec) -> Command {
    let mut command = Command::new(spec.program);
    command.args(spec.args);
    if let Some(cwd) = spec.cwd {
        command.current_dir(cwd);
    }
    // Go: `cmd.Env = ScrubbedEnv()` sets the child's *entire* environment.
    command.env_clear();
    command.envs(spec.env);
    // Go: a nil Stdin reads from the null device (os.DevNull).
    command.stdin(Stdio::null());
    command.stdout(spec.stdout);
    command.stderr(spec.stderr);
    command
}

/// Dispatch to the platform backend. Task 18 replaces the temporary Windows
/// backend with the suspended-start `CreateProcessW` / Job Object path.
#[cfg(unix)]
pub fn run_managed_command(
    spec: CommandSpec,
    cancel: &CancellationToken,
    wait_delay: Duration,
) -> io::Result<Output> {
    crate::tools::process_unix::run_managed_command(spec, cancel, wait_delay)
}

#[cfg(windows)]
pub fn run_managed_command(
    spec: CommandSpec,
    cancel: &CancellationToken,
    wait_delay: Duration,
) -> io::Result<Output> {
    crate::tools::process_windows::run_managed_command(spec, cancel, wait_delay)
}

#[cfg(not(any(unix, windows)))]
pub fn run_managed_command(
    _spec: CommandSpec,
    _cancel: &CancellationToken,
    _wait_delay: Duration,
) -> io::Result<Output> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "managed commands are unsupported on this platform",
    ))
}
