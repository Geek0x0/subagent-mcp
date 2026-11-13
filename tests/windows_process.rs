#![cfg(windows)]

//! Windows Job Object integration tests (Task 18).
//!
//! These exercise `run_managed_command` end-to-end on Windows: the suspended-
//! start / assign / resume path, stdout+stderr capture into one capped buffer,
//! the 16 KiB truncation marker, cancellation terminating the whole job, and
//! the assignment-failure path killing a still-suspended child. They are the
//! **native-Windows runtime evidence** for the one intentional security-
//! strengthening deviation from Go (design §5.6/§8.2); the differential suite
//! allowlists that difference.
//!
//! All tests serialize on `SERIAL` because `force_assignment_failure_for_test`
//! arms process-global state in the library.

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use subagent_mcp::tools::process::{
    CommandSpec, cancelled_output, is_cancelled, run_managed_command,
};
use subagent_mcp::tools::process_windows::force_assignment_failure_for_test;
use subagent_mcp::tools::shell::scrubbed_env;
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

static SERIAL: Mutex<()> = Mutex::new(());

// Both copies of this test executable act as benign marker-writing bash.exe
// peers. The directory containing the selected executable identifies its source.
#[test]
fn executable_search_marker_peer() {
    if let Some(marker) = std::env::var_os("WINDOWS_EXECUTABLE_SEARCH_MARKER") {
        let executable = std::env::current_exe().unwrap();
        fs::write(
            marker,
            executable
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();
    }
}

#[test]
fn path_executable_wins_over_cwd_bash() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let intended = tmp.path().join("bin");
    let cwd = tmp.path().join("repo");
    fs::create_dir(&intended).unwrap();
    fs::create_dir(&cwd).unwrap();
    let executable = std::env::current_exe().unwrap();
    fs::copy(&executable, intended.join("bash.exe")).unwrap();
    fs::copy(&executable, cwd.join("bash.exe")).unwrap();
    struct RestoreCwd(std::path::PathBuf);
    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }
    let _restore = RestoreCwd(std::env::current_dir().unwrap());
    std::env::set_current_dir(&cwd).unwrap();
    let marker = cwd.join("marker.txt");
    let mut spec = cmd_spec("");
    spec.program = "bash".into();
    spec.args = vec![
        "--exact".into(),
        "executable_search_marker_peer".into(),
        "--nocapture".into(),
    ];
    spec.cwd = Some(cwd.clone());
    spec.env
        .retain(|(key, _)| !key.to_string_lossy().eq_ignore_ascii_case("PATH"));
    spec.env.push(("PATH".into(), intended.into_os_string()));
    spec.env.push((
        "WINDOWS_EXECUTABLE_SEARCH_MARKER".into(),
        marker.clone().into_os_string(),
    ));
    let output = run_managed_command(spec, &CancellationToken::new(), WAIT_DELAY).unwrap();
    assert!(output.status.success());
    assert_eq!(fs::read_to_string(marker).unwrap(), "bin");
}

#[test]
fn shared_output_pipe_preserves_order_and_truncation() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let out = run_managed_command(
        powershell_spec("[Console]::Out.Write('out1'); [Console]::Error.Write('err1'); [Console]::Out.Write('out2')"),
        &CancellationToken::new(),
        WAIT_DELAY,
    )
    .unwrap();
    assert_eq!(out.stdout, b"out1err1out2");
    let script = "[Console]::Error.Write('err'); [Console]::Out.Write(('a' * 20000))";
    let out = run_managed_command(
        powershell_spec(script),
        &CancellationToken::new(),
        WAIT_DELAY,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!(
            "err{}\n[output truncated: 20003 bytes total]",
            "a".repeat(16381)
        )
    );
}

/// `GetExitCodeProcess` reports this code while the process is still running.
const STILL_ACTIVE: u32 = 259;
/// Bounds the inherited-pipe drain. Kept small so the suite is fast.
const WAIT_DELAY: Duration = Duration::from_millis(250);

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

/// Spawn a descendant (a long `ping`) from a PowerShell child and record its
/// PID. `Start-Process -NoNewWindow` uses CreateProcess, so the descendant is
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
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut code = 0u32;
    // SAFETY: h is a valid process handle and `code` a valid u32 out-param.
    let ok = unsafe { GetExitCodeProcess(h, &mut code) };
    // SAFETY: h is a handle we opened and must close.
    unsafe { CloseHandle(h) };
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

/// A child created suspended, assigned to a Job Object with KILL_ON_JOB_CLOSE,
/// and resumed must NOT be able to spawn a descendant that survives job closure.
#[test]
fn descendant_cannot_survive_job_closure() {
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

/// stdout and stderr are merged into `Output.stdout` (with `Output.stderr`
/// empty), matching the Go `limitedBuffer` / Task 17 contract.
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

/// The 16 KiB cap and the exact Go truncation marker are produced by the
/// shared `LimitedBuffer` on the Windows `CreateProcessW` path too.
#[test]
fn caps_output_at_16k_with_go_marker() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let out = run_managed_command(
        powershell_spec("[Console]::Out.Write('a' * 20000)"),
        &CancellationToken::new(),
        Duration::from_secs(10),
    )
    .expect("command should succeed");
    assert!(out.stderr.is_empty());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.len() > 16 * 1024 && text.len() < 16 * 1024 + 100,
        "len = {}",
        text.len()
    );
    assert!(
        text.ends_with("[output truncated: 20000 bytes total]"),
        "tail = {:?}",
        &text[text.len().saturating_sub(60)..]
    );
}

/// Cancellation terminates the WHOLE job (direct child + descendants) and the
/// bytes captured before the kill travel with the cancellation error.
#[test]
fn cancellation_terminates_whole_job_and_preserves_output() {
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

/// The inherited-pipe drain is bounded by ONE combined WaitDelay period (never
/// two sequential periods = ~2x). A descendant holds the pipe after the direct
/// child exits, so a 2x regression would push elapsed past the bound.
#[test]
fn one_combined_wait_delay_bounds_inherited_pipe() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let delay = Duration::from_millis(500);
    let start = Instant::now();
    let out = run_managed_command(
        cmd_spec("start /B cmd.exe /C ping -n 15 127.0.0.1 > NUL"),
        &CancellationToken::new(),
        delay,
    )
    .expect("command should succeed");
    let elapsed = start.elapsed();
    assert!(out.status.success());
    assert!(
        elapsed < delay + Duration::from_millis(300),
        "two sequential WaitDelay periods suspected: elapsed = {elapsed:?}"
    );
}

/// Assignment failure kills the still-suspended child (it must never run) and
/// returns an error. `ResetAssignFailure` guarantees the test seam is disarmed
/// even if an assertion panics.
struct ResetAssignFailure;
impl Drop for ResetAssignFailure {
    fn drop(&mut self) {
        force_assignment_failure_for_test(false);
    }
}

#[test]
fn assignment_failure_kills_suspended_child_and_returns_error() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("RAN");
    let command = format!("echo SHOULD_NOT_RUN > \"{}\"", marker.display());

    force_assignment_failure_for_test(true);
    let _reset = ResetAssignFailure;
    let result = run_managed_command(cmd_spec(&command), &CancellationToken::new(), WAIT_DELAY);

    let err = result.expect_err("assignment failure must return an error");
    assert!(
        !is_cancelled(&err),
        "assignment failure must not be a cancellation error: {err}"
    );
    assert!(
        !marker.exists(),
        "suspended child resumed and ran despite assignment failure"
    );
}
