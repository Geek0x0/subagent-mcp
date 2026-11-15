#![cfg(unix)]

//! Shell tool integration tests (Task 17).
//!
//! Port of the Go shell cases in `internal/tools/tools_test.go`,
//! `internal/tools/git_vectors_test.go`, and the shell-facing cases in
//! `cmd/subagent-mcp/main_test.go`. The bash wrapper is asserted byte-for-byte
//! against an independent golden copied verbatim from `internal/tools/tools.go`.
//!
//! The whole target is Unix-only: the tests invoke Unix-only tools and libc
//! (`bash`, `sleep`, `setsid`, `/dev/zero`, `head`, `tr`, `/bin/sh`,
//! `.bash_profile`, `kill`, `/proc/{pid}/stat`). Gating the file keeps every
//! target compiling while leaving Unix coverage unchanged (Task 17 fix I-1).
//!
//! Every test takes `SERIAL` because the Go baseline keeps the scrub
//! configuration and `HOME` in process-global state; the Rust port mirrors that
//! with `set_scrubbed_env` and `std::env::set_var`, so the tests must not run
//! concurrently.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use subagent_mcp::policy::lexer::analyze_shell;
use subagent_mcp::tools::process::CommandSpec;
use subagent_mcp::tools::shell::{
    DEFAULT_SHELL_TIMEOUT, MAX_OUTPUT_BYTES, MAX_SHELL_TIMEOUT, SHELL_SCRIPT, SandboxLauncher,
    ShellResult, WAIT_DELAY, effective_timeout, run_shell, scrubbed_env, set_scrubbed_env,
    shell_argv,
};
use subagent_mcp::util::duration::format_go_duration;
use subagent_mcp::util::run::RunContext;
use tokio_util::sync::CancellationToken;

// Independent golden: copied byte-for-byte from Go `const shellScript`.
const EXPECTED_SCRIPT: &str = r##"exact_count=$1
prefix_count=$2
shift 2
while ((exact_count > 0)); do
    builtin unset -- "$1"
    shift
    exact_count=$((exact_count - 1))
done
while ((prefix_count > 0)); do
    prefix=$1
    shift
    while IFS= read -r name; do
        if [[ $name == "$prefix"* ]]; then
            builtin unset -- "$name"
        fi
    done < <(builtin compgen -A variable)
    prefix_count=$((prefix_count - 1))
done
builtin shopt -u expand_aliases
builtin unalias -a 2>/dev/null
for __subagent_mcp_name in ls cat head tail rg grep find pwd wc stat which echo git; do
    builtin unset -f "$__subagent_mcp_name" 2>/dev/null
done
builtin unset -- __subagent_mcp_name
__subagent_mcp_safe_git() {
    local subcommand=${1-}
    case "$subcommand" in
    status|branch|rev-parse|ls-files|diff|show|log|blame) ;;
    *) command git "$@"; return ;;
    esac
    shift
    # Overrides travel in GIT_CONFIG_KEY_n/VALUE_n (Git 2.31+), which take a key
    # verbatim: "-c key=value" splits at the first "=", so a filter or diff driver
    # whose name contains "=" would slip through. Probe support first; without
    # it the overrides would be silently ignored.
    if [[ $(GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=subagent.probe GIT_CONFIG_VALUE_0=ok \
            command git config --get subagent.probe 2>/dev/null) != ok ]]; then
        echo "subagent-mcp: git 2.31 or newer is required to run git safely" >&2
        return 126
    fi
    local -a fixed=(
        "alias.$subcommand=" core.fsmonitor=false core.fsmonitorHookPath= diff.external=
        core.pager=cat core.sshCommand= credential.helper= core.hooksPath=/dev/null
        log.showSignature=false status.submoduleSummary=false submodule.recurse=false
        diff.submodule=short interactive.diffFilter= protocol.allow=never protocol.ext.allow=never
        gpg.program=/bin/false gpg.openpgp.program=/bin/false gpg.x509.program=/bin/false
        gpg.ssh.program=/bin/false
    )
    local -a keys=() vals=()
    local item drivers driver_status driver_key
    for item in "${fixed[@]}"; do
        keys+=("${item%%=*}")
        vals+=("${item#*=}")
    done
    # Every program a filter or diff driver could run, whichever config file
    # (repository, user, or included) defines it, is emptied.
    drivers=$(
        GIT_CONFIG_COUNT=0 GIT_CONFIG_PARAMETERS= \
        command git --no-pager -c alias.config= config --name-only --get-regexp \
            '^(filter\..*\.(clean|smudge|process|required)|diff\..*\.(textconv|command))$'
    )
    driver_status=$?
    if ((driver_status > 1)); then
        return "$driver_status"
    fi
    if ((driver_status == 0)); then
        while IFS= read -r driver_key; do
            if [[ -n $driver_key ]]; then
                keys+=("$driver_key")
                case "$driver_key" in
                *.required) vals+=(false) ;;
                *) vals+=("") ;;
                esac
            fi
        done <<< "$drivers"
    fi
    case "$subcommand" in
    status|diff) set -- --ignore-submodules=dirty "$@" ;;
    esac
    case "$subcommand" in
    diff|show|log|blame) set -- --no-ext-diff --no-textconv "$@" ;;
    esac
    (
        local i
        export GIT_CONFIG_COUNT=${#keys[@]} GIT_CONFIG_PARAMETERS= GIT_OPTIONAL_LOCKS=0 \
            GIT_NO_LAZY_FETCH=1 GIT_EXTERNAL_DIFF= GIT_PAGER=cat PAGER=cat GIT_SSH= \
            GIT_SSH_COMMAND= GIT_ASKPASS=
        for ((i = 0; i < ${#keys[@]}; i++)); do
            export "GIT_CONFIG_KEY_$i=${keys[i]}" "GIT_CONFIG_VALUE_$i=${vals[i]}"
        done
        command git --no-pager "$subcommand" "$@"
    )
}
git() {
    __subagent_mcp_safe_git "$@"
}
__subagent_mcp_command=$1
set --
builtin eval "$__subagent_mcp_command""##;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const GIT_ENV_KEYS: &[&str] = &[
    "HOME",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_KEY_0",
    "GIT_CONFIG_VALUE_0",
    "GIT_CONFIG_PARAMETERS",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
    "PAGER",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
];

fn ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

async fn run(cwd: &Path, command: &str, timeout: Duration) -> ShellResult {
    run_shell(&ctx(), cwd, command, Some(timeout), None).await
}

fn chmod_exec(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Change only the timestamps: the index entry becomes stat-dirty while the
/// content still matches, so git content-compares (and therefore runs the clean
/// filter) when it visits the submodule's worktree. Every call picks a distinct
/// mtime so an index refresh from an earlier probe cannot mask the next one.
fn touch_old(path: &Path) {
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst);
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    let then = std::time::UNIX_EPOCH + Duration::from_secs(1_500_000_000 + generation);
    file.set_times(fs::FileTimes::new().set_accessed(then).set_modified(then))
        .unwrap();
}

/// Snapshot and restore process-global env vars (serialized by `SERIAL`).
struct EnvRestore(Vec<(String, Option<OsString>)>);
impl EnvRestore {
    fn capture(keys: &[&str]) -> Self {
        EnvRestore(
            keys.iter()
                .map(|k| (k.to_string(), std::env::var_os(k)))
                .collect(),
        )
    }
}
impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (k, v) in &self.0 {
            // SAFETY: all callers hold SERIAL, so no other test reads/writes
            // the process environment concurrently.
            unsafe {
                match v {
                    Some(val) => std::env::set_var(k.as_str(), val),
                    None => std::env::remove_var(k.as_str()),
                }
            }
        }
    }
}

fn kill_pid(pid: i32) {
    // SAFETY: signalling an arbitrary integer pid is safe; ESRCH is ignored.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

struct KillGuard(i32);
impl Drop for KillGuard {
    fn drop(&mut self) {
        kill_pid(self.0);
    }
}

fn read_pid(path: &Path) -> i32 {
    let data = fs::read_to_string(path).unwrap();
    data.trim().parse().unwrap()
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) only probes existence/permissions.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat"))
        && let Some(end) = stat.rfind(')')
        && stat[end + 1..].trim_start().starts_with("Z ")
    {
        return false;
    }
    true
}

fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// Exact wrapper / argv / constants
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shell_script_is_byte_exact() {
    let _g = SERIAL.lock().await;
    assert_eq!(SHELL_SCRIPT, EXPECTED_SCRIPT);
    assert_eq!(SHELL_SCRIPT.len(), 3689);
    // A few structural anchors, including the safe-git wrapper.
    assert!(SHELL_SCRIPT.starts_with("exact_count=$1\n"));
    assert!(SHELL_SCRIPT.ends_with("builtin eval \"$__subagent_mcp_command\""));
    assert!(SHELL_SCRIPT.contains("builtin shopt -u expand_aliases"));
    assert!(SHELL_SCRIPT.contains("builtin unalias -a 2>/dev/null"));
    assert!(SHELL_SCRIPT.contains(
        "for __subagent_mcp_name in ls cat head tail rg grep find pwd wc stat which echo git; do"
    ));
    assert!(SHELL_SCRIPT.contains("__subagent_mcp_safe_git() {"));
    assert!(SHELL_SCRIPT.contains(
        "'^(filter\\..*\\.(clean|smudge|process|required)|diff\\..*\\.(textconv|command))$'"
    ));
    assert!(SHELL_SCRIPT.contains("protocol.allow=never protocol.ext.allow=never"));
    assert!(SHELL_SCRIPT.contains("GIT_NO_LAZY_FETCH=1"));
    assert!(SHELL_SCRIPT.contains("status|diff) set -- --ignore-submodules=dirty \"$@\" ;;"));
    assert!(SHELL_SCRIPT.contains("builtin eval \"$__subagent_mcp_command\""));
}

#[tokio::test]
async fn shell_argv_layout_is_exact() {
    let _g = SERIAL.lock().await;
    set_scrubbed_env(
        vec!["Z_KEY".into(), "A_KEY".into()],
        vec!["Q_".into(), "P_".into()],
    );
    let argv = shell_argv("echo hello");
    // Names are sorted, prefixes keep their configured order, counts are
    // positional, and the command string is the final positional arg.
    let want: Vec<OsString> = vec![
        "bash".into(),
        "-lc".into(),
        EXPECTED_SCRIPT.into(),
        "bash".into(),
        "2".into(),
        "2".into(),
        "A_KEY".into(),
        "Z_KEY".into(),
        "Q_".into(),
        "P_".into(),
        "echo hello".into(),
    ];
    assert_eq!(argv, want);

    // Default scrub configuration: no exact names, one default prefix.
    set_scrubbed_env(vec![], vec!["SUBAGENT_MCP_".into()]);
    let argv = shell_argv("pwd");
    let want: Vec<OsString> = vec![
        "bash".into(),
        "-lc".into(),
        EXPECTED_SCRIPT.into(),
        "bash".into(),
        "0".into(),
        "1".into(),
        "SUBAGENT_MCP_".into(),
        "pwd".into(),
    ];
    assert_eq!(argv, want);
}

#[tokio::test]
async fn timeout_and_output_constants() {
    let _g = SERIAL.lock().await;
    assert_eq!(DEFAULT_SHELL_TIMEOUT, Duration::from_secs(60));
    assert_eq!(MAX_SHELL_TIMEOUT, Duration::from_secs(600));
    assert_eq!(MAX_OUTPUT_BYTES, 16 * 1024);
    assert_eq!(WAIT_DELAY, Duration::from_secs(1));
}

#[tokio::test]
async fn effective_timeout_clamps_to_60_and_600_seconds() {
    let _g = SERIAL.lock().await;
    assert_eq!(effective_timeout(None), Duration::from_secs(60));
    assert_eq!(
        effective_timeout(Some(Duration::ZERO)),
        Duration::from_secs(60)
    );
    assert_eq!(
        effective_timeout(Some(Duration::from_secs(5))),
        Duration::from_secs(5)
    );
    assert_eq!(
        effective_timeout(Some(Duration::from_secs(600))),
        Duration::from_secs(600)
    );
    assert_eq!(
        effective_timeout(Some(Duration::from_secs(601))),
        Duration::from_secs(600)
    );
    assert_eq!(
        effective_timeout(Some(Duration::from_secs(3600))),
        Duration::from_secs(600)
    );
    // Go duration formatting used by the timeout result text.
    assert_eq!(format_go_duration(Duration::from_millis(100)), "100ms");
    assert_eq!(format_go_duration(Duration::from_millis(200)), "200ms");
    assert_eq!(format_go_duration(Duration::from_secs(60)), "1m0s");
    assert_eq!(format_go_duration(Duration::from_secs(600)), "10m0s");
}

#[tokio::test]
async fn scrubbed_env_filters_names_and_prefixes() {
    let _g = SERIAL.lock().await;
    let _env = EnvRestore::capture(&["MY_PROVIDER_KEY", "SUBAGENT_MCP_X", "KEEP_ME"]);
    // SAFETY: SERIAL is held.
    unsafe {
        std::env::set_var("MY_PROVIDER_KEY", "secret");
        std::env::set_var("SUBAGENT_MCP_X", "hidden");
        std::env::set_var("KEEP_ME", "kept");
    }
    set_scrubbed_env(vec!["MY_PROVIDER_KEY".into()], vec!["SUBAGENT_MCP_".into()]);
    let env = scrubbed_env();
    let has = |name: &str, value: &str| {
        env.iter()
            .any(|(k, v)| k.to_string_lossy() == name && v.to_string_lossy() == value)
    };
    assert!(!env.iter().any(|(k, _)| k == OsStr::new("MY_PROVIDER_KEY")));
    assert!(
        !env.iter()
            .any(|(k, _)| k.to_string_lossy().starts_with("SUBAGENT_MCP_"))
    );
    assert!(has("KEEP_ME", "kept"));
    set_scrubbed_env(vec![], vec!["SUBAGENT_MCP_".into()]);
}

// ---------------------------------------------------------------------------
// Core RunShell behaviour (port of TestRunShell)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runs_command_in_working_directory() {
    let _g = SERIAL.lock().await;
    let cwd = tempfile::tempdir().unwrap();
    let r = run(cwd.path(), "echo hello; pwd", Duration::from_secs(2)).await;
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0);
    let lines: Vec<&str> = r.output.trim().split('\n').collect();
    assert_eq!(lines.len(), 2, "output = {:?}", r.output);
    assert_eq!(lines[0], "hello");
    assert_eq!(
        fs::canonicalize(lines[1]).unwrap(),
        fs::canonicalize(cwd.path()).unwrap()
    );
}

#[tokio::test]
async fn nonzero_exit_without_error() {
    let _g = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let r = run(tmp.path(), "exit 3", Duration::from_secs(2)).await;
    assert!(!r.is_error);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 3);
    assert_eq!(r.output, "");
}

#[tokio::test]
async fn merges_stdout_and_stderr() {
    let _g = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let r = run(
        tmp.path(),
        "printf out1; printf err1 >&2; printf out2",
        Duration::from_secs(2),
    )
    .await;
    assert!(!r.is_error);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.output, "out1err1out2");
}

#[tokio::test]
async fn descendant_finishes_within_wait_delay() {
    let _g = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let r = run(
        tmp.path(),
        "(sleep 0.2; printf descendant-done) & exit 0",
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(r.output, "descendant-done");
    assert_eq!(r.exit_code, 0);
    assert!(!r.is_error);
}

#[tokio::test]
async fn shared_stream_truncation_keeps_descriptor_order() {
    let _g = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let r = run(
        tmp.path(),
        "for ((i=0; i<9000; i++)); do printf e >&2; printf o; done",
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(
        r.output,
        format!(
            "{}\n[output truncated: 18000 bytes total]",
            "eo".repeat(MAX_OUTPUT_BYTES / 2)
        )
    );
}

#[tokio::test]
async fn kills_process_group_on_timeout() {
    let _g = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let r = run(tmp.path(), "sleep 5", Duration::from_millis(100)).await;
    let elapsed = start.elapsed();
    assert!(r.is_error);
    assert_eq!(r.exit_code, -1);
    // `sleep 5` produces no output; the timeout text now lives in `error`.
    assert_eq!(r.output, "");
    assert_eq!(r.error.as_deref(), Some("command timed out after 100ms"));
    assert!(elapsed < Duration::from_secs(1), "elapsed = {elapsed:?}");
}

/// I-4: Go returns `(out, code, err)` on timeout — the bytes produced before
/// the kill are preserved in `output`, the timeout text moves to `error`, and
/// `is_error` mirrors `error.is_some()`.
#[tokio::test]
async fn timeout_preserves_output_before_kill() {
    let _g = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let r = run(
        tmp.path(),
        "echo BEFORE_OUTPUT; sleep 5",
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(r.output, "BEFORE_OUTPUT\n");
    assert_eq!(r.exit_code, -1);
    assert_eq!(r.error.as_deref(), Some("command timed out after 300ms"));
    assert!(r.is_error);
}

#[tokio::test]
async fn returns_success_when_background_process_holds_pipe() {
    let _g = SERIAL.lock().await;
    let cwd = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let r = run(
        cwd.path(),
        "sleep 5 & echo $! > child.pid; echo done",
        Duration::from_secs(1),
    )
    .await;
    let elapsed = start.elapsed();
    let child_pid = read_pid(&cwd.path().join("child.pid"));
    let _guard = KillGuard(child_pid);
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0);
    assert!(r.output.contains("done"), "{:?}", r.output);
    assert!(elapsed < Duration::from_secs(3), "elapsed = {elapsed:?}");
    assert!(wait_for_process_exit(child_pid, Duration::from_secs(1)));
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn grandchild_timeout_bounds_inherited_pipe_wait() {
    let _g = SERIAL.lock().await;
    let cwd = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let r = run(
        cwd.path(),
        "sleep 5 & echo $! > child.pid; setsid sleep 5 & echo $! > escaped.pid; wait",
        Duration::from_secs(1),
    )
    .await;
    let elapsed = start.elapsed();
    let child_pid = read_pid(&cwd.path().join("child.pid"));
    let escaped_pid = read_pid(&cwd.path().join("escaped.pid"));
    let _guard = KillGuard(escaped_pid);
    assert!(r.is_error);
    assert_eq!(r.exit_code, -1);
    assert_eq!(r.output, "");
    assert_eq!(r.error.as_deref(), Some("command timed out after 1s"));
    assert!(elapsed < Duration::from_secs(3), "elapsed = {elapsed:?}");
    // The in-group grandchild is killed by the process-group sweep.
    assert!(wait_for_process_exit(child_pid, Duration::from_secs(1)));
}

/// `setsid` moves a descendant out of the process group, so the post-exit sweep
/// cannot reach it and it MAY survive (until its own 5s timer). This documents
/// the boundary Go also documents in `process_unix.go`: only a PID namespace or
/// cgroup fully closes the gap. The direct child exits 0 with the pipes held,
/// so the 1s WaitDelay turns into success (ErrWaitDelay -> zero exit).
#[tokio::test]
#[cfg(target_os = "linux")]
async fn setsid_escape_descendant_may_survive() {
    let _g = SERIAL.lock().await;
    let cwd = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let r = run(
        cwd.path(),
        "setsid sleep 5 & echo $! > escaped.pid; echo started",
        Duration::from_secs(10),
    )
    .await;
    let elapsed = start.elapsed();
    let escaped_pid = read_pid(&cwd.path().join("escaped.pid"));
    let _guard = KillGuard(escaped_pid);
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0);
    assert!(r.output.contains("started"), "{:?}", r.output);
    // WaitDelay (1s) bounds the drain of the inherited pipe.
    assert!(elapsed >= WAIT_DELAY / 2, "elapsed = {elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "elapsed = {elapsed:?}");
}

/// When the direct child exits non-zero while an escaped descendant holds the
/// pipes, the non-zero exit code takes precedence over the ErrWaitDelay success
/// path (Go: Wait returns the ExitError, not ErrWaitDelay).
#[tokio::test]
#[cfg(target_os = "linux")]
async fn nonzero_exit_precedence_when_pipes_held() {
    let _g = SERIAL.lock().await;
    let cwd = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let r = run(
        cwd.path(),
        "setsid sleep 5 & echo $! > escaped.pid; exit 7",
        Duration::from_secs(10),
    )
    .await;
    let elapsed = start.elapsed();
    let escaped_pid = read_pid(&cwd.path().join("escaped.pid"));
    let _guard = KillGuard(escaped_pid);
    assert!(!r.is_error);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 7);
    assert_eq!(r.output, "");
    assert!(elapsed >= WAIT_DELAY / 2, "elapsed = {elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "elapsed = {elapsed:?}");
}

#[tokio::test]
async fn truncates_output_at_16k() {
    let _g = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let r = run(
        tmp.path(),
        "head -c 20000 /dev/zero | tr '\\0' 'a'",
        Duration::from_secs(2),
    )
    .await;
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0);
    let len = r.output.len();
    assert!(
        len > MAX_OUTPUT_BYTES && len < MAX_OUTPUT_BYTES + 100,
        "len = {len}"
    );
    let marker = "[output truncated: 20000 bytes total]";
    assert!(r.output.ends_with(marker), "suffix = {:?}", r.output);
}

// ---------------------------------------------------------------------------
// Env scrubbing (port of TestScrubbedEnv* and the login-profile tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scrubbed_env_configurable() {
    let _g = SERIAL.lock().await;
    let _env = EnvRestore::capture(&["MY_PROVIDER_KEY", "SUBAGENT_MCP_CONFIG", "KEEP_ME"]);
    // SAFETY: SERIAL is held.
    unsafe {
        std::env::set_var("MY_PROVIDER_KEY", "secret-1");
        std::env::set_var("SUBAGENT_MCP_CONFIG", "/x");
        std::env::set_var("KEEP_ME", "kept");
    }
    set_scrubbed_env(vec!["MY_PROVIDER_KEY".into()], vec!["SUBAGENT_MCP_".into()]);
    let tmp = tempfile::tempdir().unwrap();
    let r = run(tmp.path(), "env", Duration::from_secs(5)).await;
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert!(!r.output.contains("secret-1"));
    assert!(!r.output.contains("SUBAGENT_MCP_CONFIG"));
    assert!(r.output.contains("KEEP_ME=kept"), "{:?}", r.output);
    set_scrubbed_env(vec![], vec!["SUBAGENT_MCP_".into()]);
}

#[tokio::test]
async fn scrubs_login_profile_provider_key_keeps_path() {
    let _g = SERIAL.lock().await;
    let _env = EnvRestore::capture(&["HOME"]);
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let tool = bin.path().join("profile-tool");
    fs::write(&tool, "#!/bin/sh\nprintf 'profile-tool-found\\n'\n").unwrap();
    chmod_exec(&tool);
    let profile = format!(
        "export PROFILE_PROVIDER_KEY=profile-provider-secret\nexport PATH='{}':$PATH\n",
        bin.path().display()
    );
    fs::write(home.path().join(".bash_profile"), profile).unwrap();

    // SAFETY: SERIAL is held.
    unsafe { std::env::set_var("HOME", home.path()) };
    set_scrubbed_env(
        vec!["PROFILE_PROVIDER_KEY".into()],
        vec!["SUBAGENT_MCP_".into()],
    );
    let tmp = tempfile::tempdir().unwrap();
    let r = run(
        tmp.path(),
        "printf 'key=%s\\n' \"${PROFILE_PROVIDER_KEY-unset}\"; profile-tool",
        Duration::from_secs(5),
    )
    .await;
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert!(
        !r.output.contains("profile-provider-secret"),
        "{:?}",
        r.output
    );
    assert!(r.output.contains("key=unset\n"), "{:?}", r.output);
    assert!(r.output.contains("profile-tool-found"), "{:?}", r.output);
    set_scrubbed_env(vec![], vec!["SUBAGENT_MCP_".into()]);
}

#[tokio::test]
async fn scrubs_login_profile_subagent_environment() {
    let _g = SERIAL.lock().await;
    let _env = EnvRestore::capture(&["HOME"]);
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join(".bash_profile"),
        "export SUBAGENT_MCP_PROFILE_SECRET=profile-subagent-secret\n",
    )
    .unwrap();
    // SAFETY: SERIAL is held.
    unsafe { std::env::set_var("HOME", home.path()) };
    set_scrubbed_env(vec![], vec!["SUBAGENT_MCP_".into()]);
    let tmp = tempfile::tempdir().unwrap();
    let r = run(
        tmp.path(),
        "printf 'subagent=%s\\n' \"${SUBAGENT_MCP_PROFILE_SECRET-unset}\"",
        Duration::from_secs(5),
    )
    .await;
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.output, "subagent=unset\n");
    set_scrubbed_env(vec![], vec!["SUBAGENT_MCP_".into()]);
}

#[tokio::test]
async fn disables_profile_aliases_and_functions() {
    let _g = SERIAL.lock().await;
    let _env = EnvRestore::capture(&["HOME"]);
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join(".bash_profile"),
        "shopt -s expand_aliases\nalias echo='printf alias\\n'\nfunction echo { printf 'function\\n'; }\n",
    )
    .unwrap();
    // SAFETY: SERIAL is held.
    unsafe { std::env::set_var("HOME", home.path()) };
    let tmp = tempfile::tempdir().unwrap();
    let r = run(tmp.path(), "echo safe", Duration::from_secs(5)).await;
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.output, "safe\n");
}

// ---------------------------------------------------------------------------
// Safe-git wrapper behaviour (port of git_vectors_test.go / tools_test.go)
// ---------------------------------------------------------------------------

fn git_cmd(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Point git at a clean HOME/global/system config, matching Go `gitTestEnv`,
/// so the machine's real ~/.gitconfig (hooks, aliases) cannot interfere.
fn clean_git_env(home: &Path) {
    fs::create_dir_all(home).unwrap();
    // SAFETY: SERIAL is held by every caller.
    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("GIT_CONFIG_GLOBAL", home.join("global-config"));
        std::env::set_var("GIT_CONFIG_SYSTEM", home.join("system-config"));
        std::env::set_var("GIT_PAGER", "cat");
        std::env::set_var("PAGER", "cat");
        for k in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_PARAMETERS",
            "GIT_EXTERNAL_DIFF",
            "GIT_SSH",
            "GIT_SSH_COMMAND",
            "GIT_ASKPASS",
        ] {
            std::env::remove_var(k);
        }
    }
}

/// Build a repo with one committed tracked file `f.txt`.
fn git_repo(home: &Path, repo: &Path) {
    clean_git_env(home);
    fs::create_dir_all(repo).unwrap();
    git_ok(repo, &["init", "-q"]);
    git_ok(repo, &["config", "user.email", "test@example.com"]);
    git_ok(repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("f.txt"), "one\n").unwrap();
    git_ok(repo, &["add", "f.txt"]);
    git_ok(repo, &["commit", "-qm", "initial"]);
}

#[tokio::test]
async fn git_function_does_not_break_commit() {
    let _g = SERIAL.lock().await;
    let _env = EnvRestore::capture(GIT_ENV_KEYS);
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    git_repo(&home, &repo);
    fs::write(repo.join("file.txt"), "content\n").unwrap();
    let r = run(
        &repo,
        "git add file.txt && git commit -qm initial",
        Duration::from_secs(10),
    )
    .await;
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0, "output = {:?}", r.output);
    assert!(repo.join(".git/objects").exists());
}

#[tokio::test]
async fn git_wrapper_honours_global_config() {
    let _g = SERIAL.lock().await;
    let _env = EnvRestore::capture(GIT_ENV_KEYS);
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    git_repo(&home, &repo);
    // Go vector: `core.abbrev = 9` in the user's global config still applies
    // through the wrapper, yielding a 9-character short hash.
    fs::write(home.join("global-config"), "[core]\n\tabbrev = 9\n").unwrap();
    let r = run(&repo, "git rev-parse --short HEAD", Duration::from_secs(10)).await;
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0, "output = {:?}", r.output);
    assert_eq!(r.output.trim().len(), 9, "output = {:?}", r.output);
}

#[tokio::test]
async fn safe_git_neutralizes_repository_diff_external() {
    let _g = SERIAL.lock().await;
    let _env = EnvRestore::capture(GIT_ENV_KEYS);
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    let marker = dir.path().join("marker");
    let hook = dir.path().join("hook.sh");
    git_repo(&home, &repo);
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nprintf 'triggered\\n' > '{}'\n",
            marker.display()
        ),
    )
    .unwrap();
    chmod_exec(&hook);
    let hook_str = hook.to_string_lossy().to_string();
    // A repository-configured program: an external diff and a `diff` alias.
    git_ok(&repo, &["config", "diff.external", &hook_str]);
    git_ok(&repo, &["config", "alias.diff", &format!("!{hook_str}")]);
    fs::write(repo.join("f.txt"), "two\n").unwrap();

    // Positive control: plain git runs the repository program.
    let _ = fs::remove_file(&marker);
    let plain = git_cmd(&repo, &["diff"]);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(
        marker.exists(),
        "plain git did not run the repository program"
    );

    // Through the wrapper, the program must not run.
    let _ = fs::remove_file(&marker);
    let r = run(&repo, "git diff", Duration::from_secs(10)).await;
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0, "output = {:?}", r.output);
    assert!(
        !marker.exists(),
        "wrapper ran the repository-configured program: {:?}",
        r.output
    );
}

/// A submodule whose own `.git/config` defines a clean filter must never be
/// entered by the wrapped `git status`/`git diff`: the wrapper forces
/// `--ignore-submodules=dirty`, which keeps pointer changes visible without
/// reading the submodule worktree. Every form that overrides that flag is
/// rejected by the lexer.
#[tokio::test]
async fn safe_git_does_not_run_submodule_clean_filters() {
    let _g = SERIAL.lock().await;
    if !git_available() {
        eprintln!("skipping safe_git_does_not_run_submodule_clean_filters: git not found");
        return;
    }
    let _env = EnvRestore::capture(GIT_ENV_KEYS);
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    let sub = repo.join("sub");
    let marker = dir.path().join("marker");
    let clean = dir.path().join("clean.sh");
    clean_git_env(&home);

    // Build the submodule in place, then record it as a gitlink. No clone and
    // no network are involved, and `sub` is a real repository with its own
    // `.git/config` for the filter.
    fs::create_dir_all(&sub).unwrap();
    git_ok(&sub, &["init", "-q"]);
    git_ok(&sub, &["config", "user.email", "test@example.com"]);
    git_ok(&sub, &["config", "user.name", "Test"]);
    fs::write(sub.join("f.txt"), "one\n").unwrap();
    fs::write(sub.join(".gitattributes"), "f.txt filter=x\n").unwrap();
    git_ok(&sub, &["add", "f.txt", ".gitattributes"]);
    git_ok(&sub, &["commit", "-qm", "sub initial"]);
    let first_head = String::from_utf8(git_cmd(&sub, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();

    git_ok(&repo, &["init", "-q"]);
    git_ok(&repo, &["config", "user.email", "test@example.com"]);
    git_ok(&repo, &["config", "user.name", "Test"]);
    fs::write(
        repo.join(".gitmodules"),
        "[submodule \"sub\"]\n\tpath = sub\n\turl = ./sub\n",
    )
    .unwrap();
    let cache_info = format!("160000,{first_head},sub");
    git_ok(
        &repo,
        &["update-index", "--add", "--cacheinfo", &cache_info],
    );
    git_ok(&repo, &["add", ".gitmodules"]);
    git_ok(&repo, &["commit", "-qm", "add submodule"]);

    fs::write(
        &clean,
        format!(
            "#!/bin/sh\nprintf 'ran\\n' >> '{}'\ncat\n",
            marker.display()
        ),
    )
    .unwrap();
    chmod_exec(&clean);
    let clean = clean.to_str().unwrap();
    git_ok(&sub, &["config", "--local", "filter.x.clean", clean]);

    // Every bypass form named in the hardening unit is rejected before the
    // wrapper gets to run.
    for command in [
        "git status --ignore-submodules=none",
        "git status --ignore-submodules=untracked",
        "git status --ignore-s=none",
        "git status --no-ignore-submodules",
        "git diff --ignore-submodules=none",
        "git diff HEAD --ignore-submodules=none",
        "git ls-files --rec",
        "git ls-files --recu",
        "git ls-files --recur",
        "git ls-files --recurs",
        "git branch --rec",
    ] {
        let analysis = analyze_shell(command).expect("bypass form should lex");
        assert!(!analysis.allowed, "lexer allowed {command:?}");
    }

    // Positive control: with submodule recursion enabled, git enters the
    // submodule and its clean filter writes the marker.
    touch_old(&sub.join("f.txt"));
    let _ = fs::remove_file(&marker);
    let plain = git_cmd(&repo, &["status", "--ignore-submodules=none"]);
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(
        marker.exists(),
        "positive control: plain git did not run the submodule clean filter"
    );

    // `untracked` and `git diff` recurse into the worktree as well.
    touch_old(&sub.join("f.txt"));
    let _ = fs::remove_file(&marker);
    let untracked = git_cmd(&repo, &["status", "--ignore-submodules=untracked"]);
    assert!(
        untracked.status.success(),
        "{}",
        String::from_utf8_lossy(&untracked.stderr)
    );
    assert!(
        marker.exists(),
        "untracked unexpectedly skipped the submodule clean filter"
    );

    touch_old(&sub.join("f.txt"));
    let _ = fs::remove_file(&marker);
    let plain_diff = git_cmd(&repo, &["diff", "--ignore-submodules=none"]);
    assert!(
        plain_diff.status.success(),
        "{}",
        String::from_utf8_lossy(&plain_diff.stderr)
    );
    assert!(
        marker.exists(),
        "positive control: git diff did not run the submodule clean filter"
    );

    // `dirty` ignores the worktree change (no filter) while still noticing
    // pointer changes.
    touch_old(&sub.join("f.txt"));
    let _ = fs::remove_file(&marker);
    let dirty = git_cmd(&repo, &["status", "--ignore-submodules=dirty"]);
    assert!(
        dirty.status.success(),
        "{}",
        String::from_utf8_lossy(&dirty.stderr)
    );
    assert!(
        !marker.exists(),
        "dirty unexpectedly ran the submodule clean filter"
    );

    // The wrapper forces `dirty`, so neither permitted invocation may enter the
    // submodule worktree.
    for command in ["git status", "git diff"] {
        touch_old(&sub.join("f.txt"));
        let _ = fs::remove_file(&marker);
        let r = run(&repo, command, Duration::from_secs(10)).await;
        assert!(!r.is_error, "{command}: err {:?}", r.output);
        assert_eq!(r.exit_code, 0, "{command}: output {:?}", r.output);
        assert!(
            !marker.exists(),
            "{command} ran the submodule clean filter: {:?}",
            r.output
        );
    }

    // A new submodule commit moves the gitlink; `dirty` still reports it.
    git_ok(&sub, &["commit", "--allow-empty", "-qm", "sub second"]);
    let second_head = String::from_utf8(git_cmd(&sub, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_ne!(first_head, second_head);

    touch_old(&sub.join("f.txt"));
    let _ = fs::remove_file(&marker);
    let status = run(&repo, "git status", Duration::from_secs(10)).await;
    assert!(!status.is_error, "err: {:?}", status.output);
    assert_eq!(status.exit_code, 0, "output = {:?}", status.output);
    assert!(
        status.output.contains("new commits"),
        "status hid the submodule pointer change: {:?}",
        status.output
    );
    assert!(
        !marker.exists(),
        "git status ran the submodule clean filter: {:?}",
        status.output
    );

    touch_old(&sub.join("f.txt"));
    let diff = run(&repo, "git diff", Duration::from_secs(10)).await;
    assert!(!diff.is_error, "err: {:?}", diff.output);
    assert_eq!(diff.exit_code, 0, "output = {:?}", diff.output);
    assert!(
        diff.output.contains(&first_head) && diff.output.contains(&second_head),
        "diff hid the submodule pointer change: {:?}",
        diff.output
    );
    assert!(
        !marker.exists(),
        "git diff ran the submodule clean filter: {:?}",
        diff.output
    );
}

// ---------------------------------------------------------------------------
// SandboxLauncher hook
// ---------------------------------------------------------------------------

struct FakeLauncher {
    called: AtomicBool,
}
impl SandboxLauncher for FakeLauncher {
    fn wrap(&self, spec: &mut CommandSpec) {
        self.called.store(true, Ordering::SeqCst);
        // Prefix the shell command (the final positional argv entry) with a
        // marker so the test can prove `wrap` ran before spawn.
        if let Some(last) = spec.args.last_mut() {
            let mut new = OsString::from("echo SANDBOX_WRAPPED; ");
            new.push(&*last);
            *last = new;
        }
    }
}

#[tokio::test]
async fn fake_sandbox_launcher_wraps_command() {
    let _g = SERIAL.lock().await;
    let tmp = tempfile::tempdir().unwrap();
    let launcher = FakeLauncher {
        called: AtomicBool::new(false),
    };
    let r = run_shell(
        &ctx(),
        tmp.path(),
        "echo REAL",
        Some(Duration::from_secs(2)),
        Some(&launcher),
    )
    .await;
    assert!(launcher.called.load(Ordering::SeqCst));
    assert!(!r.is_error, "err: {:?}", r.output);
    assert_eq!(r.error, None);
    assert_eq!(r.exit_code, 0);
    assert!(r.output.starts_with("SANDBOX_WRAPPED"), "{:?}", r.output);
    assert!(r.output.contains("REAL"), "{:?}", r.output);
}
