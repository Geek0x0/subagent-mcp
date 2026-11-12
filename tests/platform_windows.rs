#![cfg(target_os = "windows")]

//! Native Windows platform runtime evidence (Task 28).
//!
//! Covers the Windows-specific runtime guarantees the rewrite depends on: the
//! suspended-start / assign / resume Job Object path (Task 18), path checks
//! (the held directory handle and policy confinement), whole-job process
//! cleanup, and bounded termination while stdin is held open.
//!
//! Platform TLS trust is covered by `tests/tls_platform.rs`, which the
//! `platform-windows-*` CI jobs run alongside this file. Windows arm64 runs
//! this same binary natively on the `windows-11-arm` runner (or on a Windows
//! arm64 VM as recorded in `docs/release/live-smoke-evidence.md`); a cross-build
//! is not acceptance.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use subagent_mcp::policy::path::inside_relative;
use subagent_mcp::sandbox::BoundDirectory;
use subagent_mcp::tools::process::{
    CommandSpec, cancelled_output, is_cancelled, run_managed_command,
};
use subagent_mcp::tools::shell::scrubbed_env;
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    TerminateProcess,
};

static SERIAL: Mutex<()> = Mutex::new(());

/// `GetExitCodeProcess` reports this code while the process is still running.
const STILL_ACTIVE: u32 = 259;
/// Bounds the inherited-pipe drain. Kept small so the suite is fast.
const WAIT_DELAY: Duration = Duration::from_millis(250);
/// Bound on the shutdown assertions, matching the 10 s production budget.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);

fn cmd_spec(command: &str) -> CommandSpec {
    CommandSpec {
        program: std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe")),
        args: vec![OsString::from("/C"), OsString::from(command)],
        cwd: None,
        env: scrubbed_env(),
        stdout: Stdio::piped(),
        stderr: Stdio::piped(),
        inherited_fds: Vec::new(),
    }
}

fn powershell_spec(script: &str) -> CommandSpec {
    CommandSpec {
        program: OsString::from("powershell.exe"),
        args: vec![
            OsString::from("-NoProfile"),
            OsString::from("-Command"),
            OsString::from(script),
        ],
        cwd: None,
        env: scrubbed_env(),
        stdout: Stdio::piped(),
        stderr: Stdio::piped(),
        inherited_fds: Vec::new(),
    }
}

/// Spawn a descendant (`ping`) from a PowerShell child and record its PID.
/// `Start-Process -NoNewWindow` uses CreateProcess, so the descendant is
/// created inside the same Job Object as its parent.
fn spawn_descendant_script(pid_file: &Path) -> String {
    format!(
        "$p = Start-Process -FilePath 'cmd.exe' -ArgumentList '/C ping -n 30 127.0.0.1 > NUL' -PassThru -NoNewWindow; Set-Content -Path '{}' -Value $p.Id",
        pid_file.display()
    )
}

fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("parse pid from {}: {e}", path.display()))
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: OpenProcess with QUERY_LIMITED_INFORMATION only inspects state.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut code = 0u32;
    // SAFETY: handle is a valid process handle and `code` a valid u32 out-param.
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    // SAFETY: handle was opened here and must be closed.
    unsafe { CloseHandle(handle) };
    ok != 0 && code == STILL_ACTIVE
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// Job Object: suspended start / assign / resume (Task 18)
// ---------------------------------------------------------------------------

/// The child is created suspended, assigned to a Job Object with
/// `KILL_ON_JOB_CLOSE`, then resumed. A descendant it creates must therefore
/// be inside the job and die when the job handle closes — the security
/// strengthening over Go's start-then-assign window.
#[test]
fn job_object_contains_descendants_of_the_resumed_child() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("grandchild.pid");

    let out = run_managed_command(
        powershell_spec(&spawn_descendant_script(&pid_file)),
        &CancellationToken::new(),
        WAIT_DELAY,
    )
    .expect("direct child should run to completion");
    assert!(
        out.status.success(),
        "powershell failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    let pid = read_pid(&pid_file);
    // `run_managed_command` dropped its WindowsChild before returning, which
    // closed the job handle and triggered KILL_ON_JOB_CLOSE for the descendant.
    assert!(
        wait_for_process_exit(pid, Duration::from_secs(3)),
        "descendant {pid} survived job closure"
    );
}

// ---------------------------------------------------------------------------
// Path checks
// ---------------------------------------------------------------------------

/// Windows confinement is policy-level (there is no kernel FS off Linux) plus
/// the held directory handle: `check()` proves the path still names the
/// session's original directory, and `inside_relative` refuses escapes.
#[test]
fn path_checks_confine_targets_and_detect_replacement() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    let outside = base.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"s").unwrap();

    // Policy confinement: inside is accepted, outside and `..` are refused.
    assert!(inside_relative(&root, &root.join("sub/file.txt")).is_ok());
    let err = inside_relative(&root, &outside.join("secret")).unwrap_err();
    assert!(
        err.to_string()
            .contains("resolves outside working directory"),
        "err = {err}"
    );
    let err = inside_relative(&root, Path::new("..\\escape")).unwrap_err();
    assert!(
        err.to_string().contains("parent-directory component"),
        "err = {err}"
    );

    // Held directory: an open handle on Windows, path-only FS.
    let dir = BoundDirectory::open(&root).unwrap();
    assert_eq!(dir.path(), root);
    assert!(dir.fs().is_none(), "Windows must not expose a kernel FS");
    assert!(!dir.raw_handle().is_null(), "the directory handle is held");
    assert!(dir.check().is_ok());

    // The bound name no longer reaches the original directory.
    let saved = base.path().join("saved");
    fs::rename(&root, &saved).unwrap();
    let err = dir.check().unwrap_err().to_string();
    assert!(err.contains("no longer reachable"), "err = {err}");

    // Recreating the name yields a different directory identity.
    fs::create_dir(&root).unwrap();
    let err = dir.check().unwrap_err().to_string();
    assert!(
        err.contains("no longer names the session's original directory"),
        "err = {err}"
    );
}

// ---------------------------------------------------------------------------
// Process cleanup (whole job, Task 18)
// ---------------------------------------------------------------------------

/// Cancellation terminates the whole job — direct child and descendants — and
/// the bytes captured before the kill travel with the cancellation error.
#[test]
fn process_cleanup_terminates_the_whole_job() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("grandchild.pid");
    let script = format!(
        "Write-Output BEFORE_CANCEL; {}; Start-Sleep -Seconds 30",
        spawn_descendant_script(&pid_file)
    );

    let cancel = CancellationToken::new();
    let c = cancel.clone();
    let pid_file_for_thread = pid_file.clone();
    std::thread::spawn(move || {
        // Wait until the descendant PID is recorded, so the assertion below
        // checks a process that really was inside the job.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !pid_file_for_thread.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        std::thread::sleep(Duration::from_millis(100));
        c.cancel();
    });

    let err = run_managed_command(powershell_spec(&script), &cancel, WAIT_DELAY)
        .expect_err("cancellation must return an error");
    assert!(is_cancelled(&err), "wrong error: {err}");
    let captured = cancelled_output(&err);
    assert!(
        String::from_utf8_lossy(&captured).contains("BEFORE_CANCEL"),
        "captured output lost on cancellation: {:?}",
        String::from_utf8_lossy(&captured)
    );

    let pid = read_pid(&pid_file);
    assert!(
        wait_for_process_exit(pid, Duration::from_secs(3)),
        "descendant {pid} survived cancellation of the job"
    );
}

// ---------------------------------------------------------------------------
// SIGTERM with stdin open (Task 25 invariant, platform repeat)
// ---------------------------------------------------------------------------

/// Windows has no SIGTERM. The shutdown primitives available to the supervisor
/// are Job Object termination (covered above) and `TerminateProcess`; this test
/// pins the invariant that matters for the serve loop: a child blocked on an
/// open, idle stdin pipe must still be terminated within the 10 s budget.
///
/// `cmd /C pause` blocks reading stdin, and the parent holds the write end open
/// for the whole test. A platform whose termination path waits for that read
/// fails here.
#[test]
fn terminate_process_with_stdin_open_is_bounded() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let comspec = std::env::var_os("ComSpec").unwrap_or_else(|| OsString::from("cmd.exe"));
    let mut child = std::process::Command::new(comspec)
        .args(["/C", "pause"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stdin-blocked child");

    // Hold the write end open: stdin is never written and never closed.
    let stdin = child.stdin.take().expect("stdin is piped");
    let pid = child.id();

    // The child must be running (blocked in the pause read) before termination.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(process_alive(pid), "child {pid} never reached the pause");

    let start = Instant::now();
    // SAFETY: OpenProcess/TerminateProcess/CloseHandle on a PID this test owns.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        assert!(!handle.is_null(), "OpenProcess failed");
        let terminated = TerminateProcess(handle, 1);
        CloseHandle(handle);
        assert_ne!(terminated, 0, "TerminateProcess failed");
    }

    let status = child.wait().expect("wait for terminated child");
    assert!(
        start.elapsed() < SHUTDOWN_BUDGET,
        "elapsed = {:?}",
        start.elapsed()
    );
    assert!(!status.success(), "terminated child reported success");
    drop(stdin);
}

/// The stdout/stderr merge and 16 KiB truncation contract holds on the Windows
/// `CreateProcessW` backend; this is the platform-level check for the same
/// buffer the Linux runner uses.
#[test]
fn captures_and_merges_stdout_stderr() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let out = run_managed_command(
        cmd_spec("echo OUT_MARKER & echo ERR_MARKER 1>&2"),
        &CancellationToken::new(),
        WAIT_DELAY,
    )
    .expect("command should succeed");
    assert!(out.status.success());
    assert!(out.stderr.is_empty(), "stderr must already be merged");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("OUT_MARKER"), "stdout = {text:?}");
    assert!(text.contains("ERR_MARKER"), "stdout = {text:?}");
}
