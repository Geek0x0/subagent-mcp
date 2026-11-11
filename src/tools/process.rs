//! Platform-neutral command specification and managed-command dispatch.
//!
//! `CommandSpec` describes a child process before it is spawned so a
//! [`crate::tools::shell::SandboxLauncher`] can rewrite it (Task 20). The actual
//! blocking spawn/wait lives in the platform backends, which are reached through
//! [`run_managed_command`]; the shell tool runs that in `spawn_blocking` so the
//! async runtime is never blocked.

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

/// Sentinel for the `io::Error` returned when the managed command had to kill a
/// still-running direct child because `cancel` fired. `run_shell` maps this to
/// the exact Go timeout / cancellation text; any other `io::Error` is a real
/// spawn/I/O failure.
pub(crate) const CANCELLED: &str = "subagent-mcp: managed command cancelled";

pub(crate) fn cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, CANCELLED)
}

pub(crate) fn is_cancelled(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted && err.to_string() == CANCELLED
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
