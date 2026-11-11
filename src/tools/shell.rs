//! Shell tool and its `bash -lc` wrapper (port of `internal/tools/tools.go`).
//!
//! `run_shell` builds a `CommandSpec`, optionally lets a [`SandboxLauncher`]
//! rewrite it, then runs it through [`crate::tools::process::run_managed_command`]
//! inside `spawn_blocking` so no blocking syscall runs on the async runtime. The
//! per-command shell timeout and the parent [`RunContext`] cancellation/deadline
//! are folded into a child cancellation token; when it fires the managed runner
//! signals the process group and wakes its blocking waiter.

use crate::tools::process::{self, CommandSpec};
use crate::util::duration::format_go_duration;
use crate::util::run::RunContext;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::{LazyLock, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Go `MaxOutputBytes`.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024;
/// Go `DefaultShellTimeout`.
pub const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(60);
/// Go `MaxShellTimeout`.
pub const MAX_SHELL_TIMEOUT: Duration = Duration::from_secs(600);
/// Go `cmd.WaitDelay = time.Second`: bounds inherited-pipe waits.
pub const WAIT_DELAY: Duration = Duration::from_secs(1);

/// Rust port of Go's `(out string, exitCode int, err error)`. Go's `err` carries
/// a message that `ShellResult` has no dedicated field for; `is_error` records
/// `err != nil` and, on those error paths, `output` carries the exact Go error
/// message (Go's `out` is empty on every error path this tool exercises). On
/// success and on non-zero exits, `output` is the combined, capped stdout/stderr.
pub struct ShellResult {
    pub output: String,
    pub exit_code: i32,
    pub is_error: bool,
}

/// Hook Task 20 uses to wrap the command in the Landlock helper before spawn.
pub trait SandboxLauncher: Send + Sync {
    fn wrap(&self, spec: &mut CommandSpec);
}

/// Go `const shellScript`. Byte-for-byte identical to
/// `internal/tools/tools.go`; keep the raw string exactly as written there.
/// It removes configured env names/prefixes *after* bash has loaded the login
/// profile, disables profile aliases/functions that could replace allowlisted
/// executables, and wraps allowlisted Git reads with safe config overrides.
pub const SHELL_SCRIPT: &str = r##"exact_count=$1
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
        diff.submodule=short interactive.diffFilter=
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
    diff|show|log|blame) set -- --no-ext-diff --no-textconv "$@" ;;
    esac
    (
        local i
        export GIT_CONFIG_COUNT=${#keys[@]} GIT_CONFIG_PARAMETERS= GIT_OPTIONAL_LOCKS=0 \
            GIT_EXTERNAL_DIFF= GIT_PAGER=cat PAGER=cat GIT_SSH= GIT_SSH_COMMAND= GIT_ASKPASS=
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

struct ScrubConfig {
    names: Vec<String>,
    prefixes: Vec<String>,
}

/// Go `scrubNames` (empty) and `scrubPrefixes` (`["SUBAGENT_MCP_"]`).
static SCRUB: LazyLock<RwLock<ScrubConfig>> = LazyLock::new(|| {
    RwLock::new(ScrubConfig {
        names: Vec::new(),
        prefixes: vec!["SUBAGENT_MCP_".to_string()],
    })
});

/// Go `SetScrubbedEnv`: exact names plus every name starting with a prefix.
pub fn set_scrubbed_env(names: Vec<String>, prefixes: Vec<String>) {
    let mut cfg = SCRUB.write().unwrap();
    cfg.names = names;
    cfg.prefixes = prefixes;
}

/// Go `ScrubbedEnv`: current environment minus configured exact names and
/// prefixes, so a shell child cannot read provider API keys.
pub fn scrubbed_env() -> Vec<(OsString, OsString)> {
    let cfg = SCRUB.read().unwrap();
    let mut scrubbed = Vec::new();
    for (name, value) in std::env::vars_os() {
        let name_os = name.as_os_str();
        let exact = cfg.names.iter().any(|n| OsStr::new(n.as_str()) == name_os);
        let prefixed = cfg
            .prefixes
            .iter()
            .any(|p| name.to_string_lossy().starts_with(p.as_str()));
        if !exact && !prefixed {
            scrubbed.push((name, value));
        }
    }
    scrubbed
}

/// Go `shellArgv`: `bash -lc SCRIPT bash <n> <p> <names…> <prefixes…> <command>`.
/// Names are sorted (Go `sort.Strings`); prefixes keep their configured order.
pub fn shell_argv(command: &str) -> Vec<OsString> {
    let cfg = SCRUB.read().unwrap();
    let mut names = cfg.names.clone();
    names.sort();
    names.dedup();
    let prefixes = cfg.prefixes.clone();
    drop(cfg);

    let mut argv: Vec<OsString> = vec![
        "bash".into(),
        "-lc".into(),
        SHELL_SCRIPT.into(),
        "bash".into(),
        names.len().to_string().into(),
        prefixes.len().to_string().into(),
    ];
    for name in names {
        argv.push(name.into());
    }
    for prefix in prefixes {
        argv.push(prefix.into());
    }
    argv.push(command.into());
    argv
}

/// Go `runCommandWithFiles` timeout clamp: `<= 0` -> 60s, `> 600s` -> 600s.
pub fn effective_timeout(timeout: Option<Duration>) -> Duration {
    match timeout {
        None | Some(Duration::ZERO) => DEFAULT_SHELL_TIMEOUT,
        Some(d) if d > MAX_SHELL_TIMEOUT => MAX_SHELL_TIMEOUT,
        Some(d) => d,
    }
}

/// Go `limitedBuffer`: keep the first `MaxOutputBytes` bytes of the merged
/// stdout/stderr stream and append a truncation marker reporting the total.
/// Rust captures the two pipes separately, so stdout precedes stderr; Go uses a
/// single merged pipe. Total byte count and the cap are identical.
fn cap_output(stdout: &[u8], stderr: &[u8]) -> String {
    let total = stdout.len() + stderr.len();
    let mut combined = Vec::with_capacity(total.min(MAX_OUTPUT_BYTES));
    let from_stdout = stdout.len().min(MAX_OUTPUT_BYTES);
    combined.extend_from_slice(&stdout[..from_stdout]);
    if from_stdout < MAX_OUTPUT_BYTES {
        let from_stderr = stderr.len().min(MAX_OUTPUT_BYTES - from_stdout);
        combined.extend_from_slice(&stderr[..from_stderr]);
    }
    let mut out = String::from_utf8_lossy(&combined).into_owned();
    if total > MAX_OUTPUT_BYTES {
        out.push_str(&format!("\n[output truncated: {total} bytes total]"));
    }
    out
}

#[derive(Clone, Copy)]
enum Reason {
    /// Shell timeout or parent run deadline (`context.DeadlineExceeded`).
    Deadline,
    /// Parent cancellation (`context.Canceled`).
    Canceled,
}

/// Cancel the child token on the first of: shell timeout, parent run deadline,
/// or parent cancellation. Go checks `DeadlineExceeded` before `Canceled`, so
/// the select is biased toward the deadline arms.
async fn watchdog(ctx: &RunContext, effective: Duration, child: &CancellationToken) -> Reason {
    let parent_deadline = ctx.deadline;
    tokio::select! {
        biased;
        _ = tokio::time::sleep(effective) => {
            child.cancel();
            Reason::Deadline
        }
        _ = async {
            match parent_deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        } => {
            child.cancel();
            Reason::Deadline
        }
        _ = ctx.cancel.cancelled() => {
            child.cancel();
            Reason::Canceled
        }
    }
}

/// Map the managed-command outcome to the exact Go `RunShell` result.
fn assemble(
    result: io::Result<Output>,
    effective: Duration,
    reason: Option<Reason>,
    ctx: &RunContext,
) -> ShellResult {
    let why = reason.unwrap_or_else(|| {
        if ctx.cancel.is_cancelled() {
            Reason::Canceled
        } else {
            Reason::Deadline
        }
    });
    match result {
        // The runner had to kill a live direct child: this is the timeout /
        // cancellation path (Go checks `runCtx.Err()` before `*ExitError`).
        Err(e) if process::is_cancelled(&e) => {
            let output = match why {
                Reason::Deadline => {
                    format!("command timed out after {}", format_go_duration(effective))
                }
                Reason::Canceled => "context canceled".to_string(),
            };
            ShellResult {
                output,
                exit_code: -1,
                is_error: true,
            }
        }
        // A genuine spawn / I/O failure (Go: `return out, -1, runErr`).
        Err(e) => ShellResult {
            output: e.to_string(),
            exit_code: -1,
            is_error: true,
        },
        Ok(output) => {
            let combined = cap_output(&output.stdout, &output.stderr);
            // Go: `ErrWaitDelay`/nil -> 0; `*ExitError` -> ExitCode; a signal
            // kill not caused by cancellation reports -1 with no error.
            if output.status.success() {
                ShellResult {
                    output: combined,
                    exit_code: 0,
                    is_error: false,
                }
            } else if let Some(code) = output.status.code() {
                ShellResult {
                    output: combined,
                    exit_code: code,
                    is_error: false,
                }
            } else {
                ShellResult {
                    output: combined,
                    exit_code: -1,
                    is_error: false,
                }
            }
        }
    }
}

pub async fn run_shell(
    ctx: &RunContext,
    cwd: &Path,
    command: &str,
    timeout: Option<Duration>,
    sandbox: Option<&dyn SandboxLauncher>,
) -> ShellResult {
    let effective = effective_timeout(timeout);

    let argv = shell_argv(command);
    let mut spec = CommandSpec {
        program: argv[0].clone(),
        args: argv[1..].to_vec(),
        cwd: Some(cwd.to_path_buf()),
        env: scrubbed_env(),
        stdout: Stdio::piped(),
        stderr: Stdio::piped(),
    };
    if let Some(launcher) = sandbox {
        launcher.wrap(&mut spec);
    }

    let child_token = ctx.child().cancel;
    let blocking_token = child_token.clone();
    let mut join = tokio::task::spawn_blocking(move || {
        process::run_managed_command(spec, &blocking_token, WAIT_DELAY)
    });

    enum Outcome {
        Done(io::Result<Output>),
        TimedOut(Reason),
    }
    let outcome = tokio::select! {
        res = &mut join => Outcome::Done(res.expect("managed command task panicked")),
        r = watchdog(ctx, effective, &child_token) => Outcome::TimedOut(r),
    };
    let (result, reason) = match outcome {
        Outcome::Done(r) => (r, None),
        Outcome::TimedOut(r) => (join.await.expect("managed command task panicked"), Some(r)),
    };
    assemble(result, effective, reason, ctx)
}
