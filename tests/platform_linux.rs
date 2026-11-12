#![cfg(target_os = "linux")]

//! Native Linux platform runtime evidence (Task 28).
//!
//! Consolidated platform gate for the Linux-specific runtime guarantees the
//! rewrite depends on. The underlying implementations landed in Tasks 8, 18,
//! and 20 (`sandbox_linux.rs`, `tools_shell.rs`, and the inline TLS tests cover
//! the unit level); this file is the per-platform release gate named by the
//! plan, and it is what the `platform-linux-*` CI jobs run.
//!
//! Covered here:
//!
//! - the Landlock helper re-executed through the real `subagent-mcp` binary,
//! - `openat2(RESOLVE_BENEATH)` confinement in [`BoundDirectory`],
//! - process-group cleanup of backgrounded descendants,
//! - `WaitDelay` zero-exit and non-zero-exit precedence when a descendant that
//!   left the process group keeps the inherited pipes open,
//! - SIGTERM while stdin is held open and idle (the shutdown invariant the
//!   detached stdin reader exists for).
//!
//! Tests that need a kernel without Landlock print a skip line and return, as
//! the Go suite does through `testutil.LandlockUnavailable`.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use subagent_mcp::patch::FS;
use subagent_mcp::sandbox::{BoundDirectory, HELPER_ARG, available, command};
use subagent_mcp::tools::process::{CommandSpec, run_managed_command};
use subagent_mcp::tools::shell::scrubbed_env;
use tokio_util::sync::CancellationToken;

/// Bound on the shutdown assertions, matching the 10 s production budget.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);

fn exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_subagent-mcp"))
}

fn argv(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

fn spec(program: &str, args: &[&str], cwd: Option<&Path>) -> CommandSpec {
    CommandSpec {
        program: OsString::from(program),
        args: args.iter().map(OsString::from).collect(),
        cwd: cwd.map(Path::to_path_buf),
        env: scrubbed_env(),
        stdout: Stdio::piped(),
        stderr: Stdio::piped(),
        inherited_fds: Vec::new(),
    }
}

fn shell_spec(script: &str, cwd: &Path) -> CommandSpec {
    spec("bash", &["-c", script], Some(cwd))
}

/// Kill a descendant that intentionally left the process group, so a test
/// failure cannot leave a five-second `sleep` behind.
struct KillGuard(i32);

impl Drop for KillGuard {
    fn drop(&mut self) {
        // SAFETY: best-effort SIGKILL of a PID the test itself started.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
    }
}

fn read_pid(path: &Path) -> i32 {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("parse pid from {}: {e}", path.display()))
}

/// Whether a missing kernel feature must fail instead of skip. CI sets this so
/// the Linux gate cannot pass on a kernel without Landlock.
fn landlock_required() -> bool {
    std::env::var("SUBAGENT_MCP_REQUIRE_LANDLOCK").as_deref() == Ok("1")
}

/// Return false (with a visible skip line) when Landlock is unavailable, or
/// panic when the run declares Landlock mandatory.
fn landlock_available_or_skip(test: &str) -> bool {
    if available() {
        return true;
    }
    if landlock_required() {
        panic!("{test}: SUBAGENT_MCP_REQUIRE_LANDLOCK=1 but Landlock is unavailable");
    }
    eprintln!(
        "skip: {test}: landlock unavailable on this kernel (set \
         SUBAGENT_MCP_REQUIRE_LANDLOCK=1 to fail instead)"
    );
    false
}

// ---------------------------------------------------------------------------
// Landlock helper (real binary, Task 20)
// ---------------------------------------------------------------------------

/// The re-executed helper grants writes beneath its `--rw` roots and denies
/// them everywhere else, while reads, exec, and device writes still work.
#[test]
fn landlock_helper_restricts_writes() {
    if !landlock_available_or_skip("landlock_helper_restricts_writes") {
        return;
    }
    let inside = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let inside_s = inside.path().to_string_lossy().into_owned();
    let outside_s = outside.path().to_string_lossy().into_owned();

    let allowed =
        format!("touch {inside_s}/ok && echo x > /dev/null && cat /etc/passwd > /dev/null");
    let out = command(
        exe(),
        &[inside.path().to_path_buf()],
        &argv(&["bash", "-c", &allowed]),
    )
    .output()
    .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "helper denied an in-root write: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(inside.path().join("ok").exists());

    let denied = format!("touch {outside_s}/bad");
    let out = command(
        exe(),
        &[inside.path().to_path_buf()],
        &argv(&["bash", "-c", &denied]),
    )
    .output()
    .unwrap();
    assert_ne!(
        out.status.code(),
        Some(0),
        "helper allowed an out-of-root write: {out:?}"
    );
    assert!(
        !outside.path().join("bad").exists(),
        "out-of-root file was created"
    );
}

/// The helper is reached through `argv[1]` before any Tokio runtime exists; a
/// malformed helper invocation fails closed with exit 126 and a diagnostic.
#[test]
fn helper_bad_args_exit_126() {
    let out = std::process::Command::new(exe())
        .arg(HELPER_ARG)
        .arg("--bogus")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(126), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sandbox helper: unexpected argument"),
        "stderr = {stderr:?}"
    );
}

// ---------------------------------------------------------------------------
// openat2(RESOLVE_BENEATH) confinement (Task 20)
// ---------------------------------------------------------------------------

/// Absolute and relative symlinks that point outside the bound directory are
/// refused by the kernel-enforced `openat2` filesystem, not merely by string
/// comparison.
#[test]
fn openat2_bound_directory_denies_symlink_escape() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    let outside = base.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"s").unwrap();
    symlink(&outside, root.join("abs")).unwrap();
    symlink("../outside", root.join("rel")).unwrap();

    let dir = BoundDirectory::open(&root).unwrap();
    let fs: &dyn FS = dir.fs().expect("Linux BoundDirectory exposes a bound fs");
    let p = |rel: &str| dir.path().join(rel);

    // Positive control first: the same handle must succeed on safe paths, so a
    // wholly broken `fs()` cannot make the escape assertions below pass.
    fs.write(&p("safe.txt"), b"safe", 0o644).unwrap();
    assert_eq!(fs.read(&p("safe.txt")).unwrap(), b"safe");
    symlink("safe.txt", root.join("inside-link")).unwrap();
    assert_eq!(fs.read(&p("inside-link")).unwrap(), b"safe");

    for rel in ["abs/new.txt", "rel/new.txt", "abs/secret", "rel/secret"] {
        assert!(
            fs.write(&p(rel), b"x", 0o644).is_err(),
            "write({rel}) escaped the bound root"
        );
        assert!(
            fs.read(&p(rel)).is_err(),
            "read({rel}) escaped the bound root"
        );
    }
    assert!(!outside.join("new.txt").exists(), "file appeared outside");
    assert!(
        fs.write(&outside.join("direct.txt"), b"x", 0o644).is_err(),
        "accepted a path outside the bound root"
    );
    assert!(dir.check().is_ok());
}

// ---------------------------------------------------------------------------
// Process-group cleanup (Task 17)
// ---------------------------------------------------------------------------

/// A descendant backgrounded with its own redirection must not survive the
/// call: the direct child exits at once and holds no pipe, so only the
/// post-exit process-group sweep can stop it.
#[test]
fn process_group_cleanup_kills_backgrounded_descendant() {
    let cwd = tempfile::tempdir().unwrap();
    let marker = cwd.path().join("survivor");
    let script = format!(
        "( sleep 1; touch {} ) >/dev/null 2>&1 & echo started",
        marker.display()
    );

    let out = run_managed_command(
        shell_spec(&script, cwd.path()),
        &CancellationToken::new(),
        Duration::from_secs(2),
    )
    .expect("clean start");
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("started"));

    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !marker.exists(),
        "backgrounded descendant survived the call and wrote {}",
        marker.display()
    );
}

// ---------------------------------------------------------------------------
// WaitDelay precedence (Task 17)
// ---------------------------------------------------------------------------

/// A descendant that calls `setsid` leaves the process group and may survive
/// (documented Unix limit). When the direct child exits 0, the inherited pipes
/// are bounded by ONE `WaitDelay` and the result is a zero exit
/// (`exec.ErrWaitDelay` -> success), not an error or a hang.
#[test]
fn wait_delay_zero_exit_when_descendant_holds_pipes() {
    let cwd = tempfile::tempdir().unwrap();
    let wait_delay = Duration::from_millis(250);
    let start = Instant::now();
    let out = run_managed_command(
        shell_spec(
            "setsid sleep 5 & echo $! > escaped.pid; echo started",
            cwd.path(),
        ),
        &CancellationToken::new(),
        wait_delay,
    )
    .expect("zero exit with pipes held");
    let elapsed = start.elapsed();

    let escaped = read_pid(&cwd.path().join("escaped.pid"));
    let _guard = KillGuard(escaped);

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stderr.is_empty(), "stderr must already be merged");
    assert!(String::from_utf8_lossy(&out.stdout).contains("started"));
    assert!(
        elapsed >= wait_delay / 2,
        "WaitDelay was not applied: elapsed = {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "two sequential WaitDelay periods suspected: elapsed = {elapsed:?}"
    );
}

/// When the direct child exits non-zero while an escaped descendant holds the
/// pipes, the non-zero exit code takes precedence over the `ErrWaitDelay`
/// success path (Go: `Wait` returns the `ExitError`, not `ErrWaitDelay`).
#[test]
fn wait_delay_nonzero_exit_precedence_when_descendant_holds_pipes() {
    let cwd = tempfile::tempdir().unwrap();
    let wait_delay = Duration::from_millis(250);
    let start = Instant::now();
    let out = run_managed_command(
        shell_spec("setsid sleep 5 & echo $! > escaped.pid; exit 7", cwd.path()),
        &CancellationToken::new(),
        wait_delay,
    )
    .expect("non-zero exit with pipes held");
    let elapsed = start.elapsed();

    let escaped = read_pid(&cwd.path().join("escaped.pid"));
    let _guard = KillGuard(escaped);

    assert_eq!(out.status.code(), Some(7), "{out:?}");
    assert!(out.stdout.is_empty(), "stdout = {:?}", out.stdout);
    assert!(
        elapsed >= wait_delay / 2,
        "WaitDelay was not applied: elapsed = {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "two sequential WaitDelay periods suspected: elapsed = {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// SIGTERM with stdin open (Task 25 invariant, platform repeat)
// ---------------------------------------------------------------------------

/// SKELETON — ignored until Task 25 lands the serve lifecycle.
///
/// A process whose stdin pipe is held open and idle must exit within the 10 s
/// shutdown budget when it receives SIGTERM. The production serve loop that
/// installs the handler is Task 25's `cli::serve`; until it exists there is no
/// crate code on this path, so this test only pins the OS-level shell behavior
/// and must not be claimed as a gate. Task 25 must replace the `sh` child with
/// the real `subagent-mcp` server (open stdin, initialize, SIGTERM) and remove
/// `#[ignore]`.
#[test]
#[ignore = "requires Task 25 serve lifecycle"]
fn sigterm_with_stdin_open_exits_within_budget() {
    let mut child = std::process::Command::new("sh")
        .args([
            "-c",
            "trap 'exit 0' TERM; echo ready; read line; echo after-read",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stdin-blocked child");

    // Hold the write end open for the whole test: stdin is never written and
    // never closed, which is the "client keeps the pipe open" case.
    let stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut ready = [0u8; 6];
    stdout
        .read_exact(&mut ready)
        .expect("child never reached the blocking read");
    assert_eq!(&ready, b"ready\n");

    let start = Instant::now();
    let pid = i32::try_from(child.id()).expect("pid fits i32");
    // SAFETY: the child was started by this test and has not been reaped.
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGTERM) },
        0,
        "send SIGTERM: {}",
        std::io::Error::last_os_error()
    );

    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for child") {
            break status;
        }
        if start.elapsed() >= SHUTDOWN_BUDGET {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not exit within {SHUTDOWN_BUDGET:?} of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(
        status.success(),
        "child exited with {status:?}; the SIGTERM trap did not run"
    );
    assert!(
        start.elapsed() < SHUTDOWN_BUDGET,
        "elapsed = {:?}",
        start.elapsed()
    );
    drop(stdin);
}
