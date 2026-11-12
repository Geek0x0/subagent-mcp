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
//! empty); the Windows `CreateProcessW` backend (Task 18) keeps this contract.

use crate::tools::shell::MAX_OUTPUT_BYTES;
use std::ffi::OsString;
use std::io;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
use std::process::{Output, Stdio};
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
    /// Parent descriptors to inherit into the child (Go `cmd.ExtraFiles`).
    /// These are the session's held `O_PATH` directory fds; on Unix the child
    /// clears FD_CLOEXEC on them in `pre_exec` so the Landlock helper can use
    /// the exact inode the session bound, not a re-resolved path. The parent
    /// keeps ownership (no dup / no double close); empty for ordinary commands.
    pub inherited_fds: Vec<i32>,
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
/// Public so the Windows integration tests can identify the killed path.
pub fn is_cancelled(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
        && err
            .get_ref()
            .and_then(|e| e.downcast_ref::<Cancelled>())
            .is_some()
}

/// Recover the captured output bytes from a cancellation error. Public so the
/// Windows integration tests can assert output preservation on the killed path.
pub fn cancelled_output(err: &io::Error) -> Vec<u8> {
    err.get_ref()
        .and_then(|e| e.downcast_ref::<Cancelled>())
        .map(|c| c.output.clone())
        .unwrap_or_default()
}

/// Build a `std::process::Command` from a `CommandSpec` for the Unix backend,
/// which adds process-group handling before spawn. The Windows backend (Task 18)
/// uses `CreateProcessW` directly and does not use `std::process::Command`, so
/// this helper is Unix-only; keeping it unconditional would be dead code on
/// Windows and fail `clippy -D warnings` there.
#[cfg(unix)]
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
    if !spec.inherited_fds.is_empty() {
        let fds = spec.inherited_fds;
        // SAFETY: `pre_exec` runs in the forked child before exec. The closure
        // only calls the async-signal-safe `fcntl`, captures a `Vec<i32>` (no
        // allocation/locking), and touches only the passed descriptors. Clearing
        // FD_CLOEXEC lets the exact held fds survive into the helper process.
        unsafe {
            command.pre_exec(move || {
                for fd in &fds {
                    let flags = libc::fcntl(*fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    command
}

/// Dispatch to the platform backend. Unix uses a process group; Windows uses
/// the `CreateProcessW` suspended-start / Job Object / resume path.
#[cfg(unix)]
pub fn run_managed_command(
    spec: CommandSpec,
    cancel: &CancellationToken,
    wait_delay: Duration,
) -> io::Result<Output> {
    crate::tools::process_unix::run_managed_command(spec, cancel, wait_delay)
}

/// **Differential allowlist entry.** The Windows backend is the single
/// intentional security-strengthening deviation from Go (design §5.6/§8.2): Go
/// starts the child then assigns it to a Job Object, leaving a window in which a
/// descendant can escape; Rust creates the job first, starts the child suspended,
/// assigns it, then resumes. The differential suite allowlists this difference;
/// do not report it as an unexplained Go-vs-Rust diff.
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
