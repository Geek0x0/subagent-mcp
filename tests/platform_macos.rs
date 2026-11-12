#![cfg(target_os = "macos")]

//! Native macOS platform runtime evidence (Task 28).
//!
//! Off Linux there is no kernel-enforced filesystem sandbox: [`sandbox`] is a
//! *path* sandbox. Policy-level confinement still rejects escapes, and the
//! held [`BoundDirectory`] proves inode identity through an open descriptor,
//! but `fs()` returns `None` so callers fall back to ordinary paths. This file
//! pins exactly that contract plus the process-group and SIGTERM shutdown
//! invariants shared with the other Unix platforms.
//!
//! Platform TLS trust is covered by `tests/tls_platform.rs`, which the
//! `platform-macos-*` CI jobs run alongside this file.

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use subagent_mcp::policy::path::inside_relative;
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

fn shell_spec(script: &str, cwd: &Path) -> CommandSpec {
    CommandSpec {
        program: OsString::from("sh"),
        args: vec![OsString::from("-c"), OsString::from(script)],
        cwd: Some(cwd.to_path_buf()),
        env: scrubbed_env(),
        stdout: Stdio::piped(),
        stderr: Stdio::piped(),
        inherited_fds: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Path sandbox (no kernel enforcement off Linux)
// ---------------------------------------------------------------------------

/// macOS has no Landlock equivalent in this crate: `available()` is false and
/// the helper fails closed with the documented message instead of pretending
/// to confine the child.
#[test]
fn path_sandbox_reports_no_kernel_enforcement() {
    assert!(!available(), "macOS must not report a kernel sandbox");
    let out = command(exe(), &[], &argv(&["true"])).output().unwrap();
    assert_eq!(out.status.code(), Some(126), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("landlock unavailable: kernel sandbox requires Linux"),
        "stderr = {stderr:?}"
    );
}

/// The policy layer is the macOS sandbox: it resolves symlinks, rejects `..`
/// components, and refuses targets outside the working directory.
#[test]
fn path_sandbox_confines_targets_lexically_and_by_resolution() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    let outside = base.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"s").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("abs")).unwrap();

    assert!(inside_relative(&root, &root.join("sub/file.txt")).is_ok());
    assert!(inside_relative(&root, &root).is_ok());

    let err = inside_relative(&root, &outside.join("secret")).unwrap_err();
    assert!(
        err.to_string()
            .contains("resolves outside working directory"),
        "err = {err}"
    );

    let err = inside_relative(&root, Path::new("../escape")).unwrap_err();
    assert!(
        err.to_string().contains("parent-directory component"),
        "err = {err}"
    );

    let err = inside_relative(&root, &root.join("abs/secret")).unwrap_err();
    assert!(
        err.to_string()
            .contains("resolves outside working directory"),
        "err = {err}"
    );
}

/// The held directory is an open descriptor on macOS too: `path()` is the
/// bound path, `check()` proves inode identity, and `fs()` is `None` because
/// there is no `openat2` equivalent to hand out.
#[test]
fn bound_directory_holds_the_inode_and_is_path_only() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("marker"), b"original").unwrap();

    let dir = BoundDirectory::open(&root).unwrap();
    assert_eq!(dir.path(), root);
    assert!(dir.fs().is_none(), "macOS must not expose a kernel FS");
    assert!(dir.check().is_ok());
    assert!(dir.raw_fd() >= 0, "the directory descriptor is held");
    assert_eq!(fs::read(dir.path().join("marker")).unwrap(), b"original");

    let saved = base.path().join("saved");
    fs::rename(&root, &saved).unwrap();
    fs::create_dir(&root).unwrap();
    let err = dir.check().unwrap_err().to_string();
    assert!(
        err.contains("no longer names the session's original directory"),
        "err = {err}"
    );
}

// ---------------------------------------------------------------------------
// Process-group cleanup (Task 17, Unix backend)
// ---------------------------------------------------------------------------

/// A backgrounded descendant must not survive the call on macOS: the direct
/// child exits immediately and holds no pipe, so the post-exit process-group
/// sweep is what stops the descendant.
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
// SIGTERM with stdin open (Task 25 invariant, platform repeat)
// ---------------------------------------------------------------------------

/// A serving process whose stdin pipe is held open and idle must exit within
/// the 10 s shutdown budget when it receives SIGTERM, and it must report
/// success rather than the cancellation the serve loop ends with.
///
/// The child is driven through an MCP `initialize` exchange first, so the
/// signal cannot arrive before the server installed its handler.
#[test]
fn sigterm_with_stdin_open_exits_within_budget() {
    let home = tempfile::tempdir().unwrap();
    let config = write_server_config(home.path());
    let mut child = std::process::Command::new(exe())
        .env("SUBAGENT_MCP_CONFIG", &config)
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn subagent-mcp");

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let (sender, lines) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match stdout.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if sender.send(line.clone()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    stdin
        .write_all(initialize_frame().as_bytes())
        .expect("write initialize");
    stdin.flush().expect("flush initialize");
    let response = lines
        .recv_timeout(Duration::from_secs(20))
        .expect("server never answered initialize");
    assert!(
        response.contains("\"serverInfo\""),
        "initialize response = {response:?}"
    );

    // Hold the write end open for the whole test: stdin is never written again
    // and never closed, which is the "client keeps the pipe open" case.
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
        "child exited with {status:?}; the serve loop did not shut down cleanly"
    );
    assert!(
        start.elapsed() < SHUTDOWN_BUDGET,
        "elapsed = {:?}",
        start.elapsed()
    );
    drop(stdin);
    drop(lines);
    let _ = reader.join();
}

/// The provider config the platform SIGTERM test starts: a valid chat
/// completions provider whose key is unset, so startup never needs the network.
fn write_server_config(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    fs::write(
        &path,
        r#"
[providers.test]
api = "chat-completions"
env_key = "SUBAGENT_PLATFORM_TEST_KEY"
default_model = "test-model"
models = [{ id = "test-model" }]
"#,
    )
    .unwrap();
    path
}

fn initialize_frame() -> String {
    concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"platform-test","version":"1"}}}"#,
        "\n"
    )
    .to_string()
}
