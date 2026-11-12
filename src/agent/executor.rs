//! Built-in tool executor (Task 23B): the policy/approval/tool layer behind
//! the runner.
//!
//! Ports Go `internal/agent/runner.go`'s `BuiltinTools`, `execToolCall`,
//! `authorize`, `safeExecToolCall` (panic recovery), `emitExecEnd`,
//! `escalationsFor`, `displayPath` and `truncateArguments`. Every tool call is
//! evaluated against the policy matrix on its own; approval-requiring requests
//! are combined into one prompt; the four tools (`shell`, `read_file`,
//! `write_file`, `apply_patch`) run sequentially.
//!
//! The whole async tool execution is wrapped in
//! `FutureExt::catch_unwind(AssertUnwindSafe(..))` at the **polling boundary**
//! (around the future, not just its creation), so a panic raised after an
//! `.await` becomes `internal error: tool execution panicked: ...` instead of
//! unwinding the run. An [`ExecEnd`] guard stands in for Go's `defer
//! emitExecEnd`, so the end event is emitted even when a begin/emit panics.
//!
//! Deadline enforcement: `shell` and `read_file` receive the call's
//! [`RunContext`] (both are deadline-aware and kill/abandon their work when
//! `ctx.deadline` fires), the approval wait is raced against
//! `ctx.cancel`/`ctx.deadline`, and an already-finished context short-circuits
//! to `cancelled: not executed`. Nothing relies on the runner's before/after
//! checks to bound an in-flight tool.

use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

use crate::agent::Emitter;
use crate::agent::runner::{CANCELLED_TOOL_RESULT, RunHooks, ToolExecutor, ToolResult};
use crate::agent::session::SessionLease;
use crate::mcp::ToolError;
use crate::patch::{Change, FS, Hunk, OS, Patch, commit_fs, parse_patch, plan_fs, summary};
use crate::policy::{
    Decision, PolicyError, PolicyRequest, Sandbox, ToolKind, bound_path, evaluate_with_reason,
};
use crate::provider::{ToolCall, ToolSpec};
use crate::sandbox::{BoundDirectory, Sandbox as LandlockLauncher};
use crate::tools::files::read_file_range;
use crate::tools::shell::{ShellResult, run_shell};
use crate::util::approval::{ApprovalRequest, Approver};
use crate::util::json::truncate_utf8;
use crate::util::run::RunContext;

/// The executor the runner delegates to. A unit struct: all per-call state
/// (progress and approval sinks) arrives through [`RunHooks`], never stored on
/// the executor, so concurrent runs cannot share a sink.
pub struct BuiltinToolExecutor;

fn schema(raw: &str) -> Box<serde_json::value::RawValue> {
    serde_json::value::RawValue::from_string(raw.to_owned())
        .expect("built-in tool schema is valid JSON")
}

/// Go `BuiltinTools()`: the four tool declarations with byte-faithful schemas.
pub fn builtin_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "shell".into(),
            description: "Run a bash command in the working directory. The sandbox policy may deny the call; providing justification helps if approval is required.".into(),
            parameters: schema(
                r##"{
				"type": "object",
				"properties": {
					"command": {"type": "string", "description": "Bash command to run."},
					"timeout_seconds": {"type": "integer", "description": "Optional timeout in seconds (clamped, max 600)."},
					"justification": {"type": "string", "description": "Why this call is needed if approval is required."}
				},
				"required": ["command"]
			}"##,
            ),
        },
        ToolSpec {
            name: "read_file".into(),
            description: "Read a file, resolving relative paths against the working directory. The sandbox policy may deny the call; providing justification helps if approval is required.".into(),
            parameters: schema(
                r##"{
				"type": "object",
				"properties": {
					"path": {"type": "string", "description": "File path to read."},
					"offset": {"type": "integer", "description": "1-based line number to start reading from; defaults to 1"},
					"limit": {"type": "integer", "description": "maximum number of lines to return; defaults to all that fit in the 16 KiB output cap"},
					"justification": {"type": "string", "description": "Why this call is needed if approval is required."}
				},
				"required": ["path"]
			}"##,
            ),
        },
        ToolSpec {
            name: "write_file".into(),
            description: "Create or overwrite a whole file, creating parent directories as needed. The sandbox policy may deny the call; providing justification helps if approval is required.".into(),
            parameters: schema(
                r##"{
				"type": "object",
				"properties": {
					"path": {"type": "string", "description": "File path to write."},
					"content": {"type": "string", "description": "Complete file content."},
					"justification": {"type": "string", "description": "Why this call is needed if approval is required."}
				},
				"required": ["path", "content"]
			}"##,
            ),
        },
        ToolSpec {
            name: "apply_patch".into(),
            description: "Edit files with a patch: '*** Begin Patch', then '*** Add File: <path>' (+lines), '*** Delete File: <path>', or '*** Update File: <path>' (optional '*** Move to: <path>') with '@@' chunks of ' ' context, '-' removed, and '+' added lines, then '*** End Patch'. Prefer this for editing existing files. The sandbox policy may deny the call; providing justification helps if approval is required.".into(),
            parameters: schema(
                r##"{
				"type": "object",
				"properties": {
					"patch": {"type": "string", "description": "Complete patch text from *** Begin Patch to *** End Patch."},
					"justification": {"type": "string", "description": "Why this call is needed if approval is required."}
				},
				"required": ["patch"]
			}"##,
            ),
        },
    ]
}

#[async_trait::async_trait]
impl ToolExecutor for BuiltinToolExecutor {
    fn tools(&self) -> Vec<ToolSpec> {
        builtin_tools()
    }

    async fn execute(
        &self,
        ctx: &RunContext,
        lease: &mut SessionLease,
        call: &ToolCall,
        hooks: &RunHooks,
    ) -> ToolResult {
        // Go `safeExecToolCall`: recover a panic raised while the async tool
        // body is POLLING (including after `.await` points), not merely while
        // the future is created. `AssertUnwindSafe` is required because the
        // future borrows `&mut SessionLease`.
        let future = self.exec_tool_call(ctx, lease, call, hooks);
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(result) => result,
            Err(payload) => ToolResult {
                output: format!(
                    "internal error: tool execution panicked: {}",
                    // `payload.as_ref()` downcasts the panic VALUE inside the
                    // Box; `&payload` would coerce to the Box's own `dyn Any`.
                    panic_message(payload.as_ref())
                ),
                is_error: true,
            },
        }
    }
}

/// Go `json.Unmarshal` target for `execToolCall`'s arguments.
#[derive(Deserialize, Default)]
struct Args {
    #[serde(default)]
    command: String,
    #[serde(default)]
    timeout_seconds: i64,
    #[serde(default)]
    path: String,
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    limit: i64,
    #[serde(default)]
    content: String,
    #[serde(default)]
    patch: String,
    #[serde(default)]
    justification: String,
}

/// Go `fmt.Sprintf("...%v", recovered)` for a panic payload. `panic!("lit")`
/// carries `&'static str`, formatted `panic!` carries `String`.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else {
        "Box<dyn Any>".to_string()
    }
}

/// Go `ctx.Err() != nil`: cancelled OR the deadline already elapsed.
fn ctx_expired(ctx: &RunContext) -> bool {
    ctx.cancel.is_cancelled() || ctx.deadline.is_some_and(|d| Instant::now() >= d)
}

async fn deadline_sleep(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

/// Enforce the run deadline / cancellation around the approval wait (the one
/// await with no built-in watchdog). On expiry the approval is not granted.
async fn wait_approval(ctx: &RunContext, approver: &dyn Approver, req: ApprovalRequest) -> bool {
    tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => false,
        _ = deadline_sleep(ctx.deadline) => false,
        v = approver.approve(ctx, req) => v,
    }
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "shell" => ToolKind::Shell,
        "read_file" => ToolKind::ReadFile,
        "write_file" => ToolKind::WriteFile,
        "apply_patch" => ToolKind::ApplyPatch,
        other => ToolKind::Other(other.to_string()),
    }
}

/// Go `escalationsFor`: approving a shell call in a confining sandbox runs it
/// without the kernel sandbox. Danger-full-access never confined it.
fn escalations_for(tool: &str, sandbox: Sandbox) -> Option<&'static str> {
    if tool != "shell" || sandbox == Sandbox::DangerFullAccess {
        return None;
    }
    Some("runs the command without the kernel sandbox")
}

fn policy_request(
    tool: ToolKind,
    command: Option<String>,
    path: Option<String>,
    cwd: &Path,
    bound_cwd: Option<&BoundDirectory>,
) -> PolicyRequest {
    PolicyRequest {
        tool,
        command,
        path,
        cwd: cwd.to_path_buf(),
        bound_cwd: bound_cwd.map(|b| b.path()),
    }
}

/// Go `policy.BoundPath`: validate `target` beneath `cwd` and return the held
/// root path the kernel-confined `FS` must use. Rust `bound_path` yields a path
/// UNDER `cwd`; it must be re-based under `BoundDirectory::path()`
/// (`/proc/self/fd/<fd>` on Linux) or every `openat2(RESOLVE_BENEATH)` op hits
/// EXDEV.
fn confined_path(cwd: &Path, target: &str, bound: &BoundDirectory) -> Result<PathBuf, PolicyError> {
    let bp = bound_path(cwd, Path::new(target))?;
    let relative = bp.path.strip_prefix(cwd).unwrap_or(bp.path.as_path());
    Ok(bound.path().join(relative))
}

/// Go `filepath.Clean`, lexically.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            Component::Normal(s) => out.push(s),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Go `displayPath`: a human-readable path for summary / rollout records.
fn display_path(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        lexical_clean(path)
    } else {
        lexical_clean(&cwd.join(path))
    }
}

/// Distinguish a `BoundPath` mapping failure (Go returns without a
/// `patch_apply_end` record) from a plan/commit failure (Go records one).
enum ApplyFailure {
    Mapping(String),
    Execute(String),
}

/// Plan a patch against the chosen `FS`, commit transactionally, and rewrite
/// confined paths back to display paths (Go `PlanFS` + `CommitFS` +
/// `displayPath` rewrite).
fn plan_and_commit(
    cwd: &Path,
    hunks: &[Hunk],
    confined: bool,
    bound_cwd: Option<&BoundDirectory>,
) -> Result<Vec<Change>, ApplyFailure> {
    let os = OS;
    let mut planned = hunks.to_vec();
    let fs: &dyn FS;
    if confined {
        let bc = bound_cwd.expect("a confined patch always has a bound cwd");
        for h in &mut planned {
            h.path = confined_path(cwd, &h.path, bc)
                .map_err(|e| ApplyFailure::Mapping(e.message))?
                .to_string_lossy()
                .into_owned();
            if !h.move_to.is_empty() {
                h.move_to = confined_path(cwd, &h.move_to, bc)
                    .map_err(|e| ApplyFailure::Mapping(e.message))?
                    .to_string_lossy()
                    .into_owned();
            }
        }
        // Kernel-confined FS on Linux, ordinary paths elsewhere.
        fs = bc.fs().unwrap_or(&os);
    } else {
        fs = &os;
    }

    let patch = Patch {
        cwd: cwd.to_path_buf(),
        hunks: planned,
    };
    let mut changes = plan_fs(fs, &patch).map_err(|e| ApplyFailure::Execute(e.message))?;
    commit_fs(fs, &changes).map_err(|e| ApplyFailure::Execute(e.message))?;

    if confined {
        for (i, change) in changes.iter_mut().enumerate() {
            if let Some(h) = hunks.get(i) {
                change.path = display_path(cwd, &h.path);
                if !h.move_to.is_empty() {
                    change.move_to = Some(display_path(cwd, &h.move_to));
                }
            }
        }
    }
    Ok(changes)
}

/// Write `content` to `target` through `fs`: create parent directories then
/// write with mode 0644, mirroring `tools::WriteFile` / `Beneath.WriteFile`.
///
/// This is deliberately **synchronous** (no `.await`): `tools::write_file`'s
/// future captures a `&dyn FS`, which is not `Send`, and the `ToolExecutor`
/// trait requires a `Send` future. Calling `&dyn FS` methods directly keeps the
/// executor's future `Send` while matching the tool's behavior exactly.
fn write_target(fs: &dyn FS, target: &Path, content: &str) -> Result<usize, ToolError> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs.mkdir_all(parent, 0o755)
        .and_then(|_| fs.write(target, content.as_bytes(), 0o644))
        .map(|_| content.len())
        .map_err(|e| ToolError::internal(e.to_string()))
}

/// Go `defer emitExecEnd`: the end event is emitted on every normal return AND
/// while a panic unwinds through the tool body. Its mutable fields stand in for
/// Go's `*exitCode` / `toolErr` captured by the deferred closure.
struct ExecEnd {
    emitter: Arc<dyn Emitter>,
    thread_id: String,
    call_id: String,
    tool: String,
    exit_code: Option<i32>,
    tool_err: Option<String>,
    paths: Option<Vec<String>>,
}

impl Drop for ExecEnd {
    fn drop(&mut self) {
        let mut event = json!({
            "type": "exec_command_end",
            "call_id": self.call_id,
            "tool": self.tool,
        });
        if let Some(code) = self.exit_code {
            event["exit_code"] = json!(code);
        }
        // Go `emitExecEnd` sets `paths` only when the slice is non-nil (a
        // valid apply_patch always has at least one path).
        if let Some(paths) = &self.paths
            && !paths.is_empty()
        {
            event["paths"] = json!(paths);
        }
        if let Some(err) = &self.tool_err {
            event["error"] = json!(err);
        }
        self.emitter.emit(&self.thread_id, event);
    }
}

/// The executor does not know the runner's per-run `turn_id` (a local of
/// `RunLocked`), so `patch_apply_end` carries the empty string for that field;
/// every other field matches Go `recordPatchApplied` exactly.
const PATCH_APPLY_TURN_ID: &str = "";

impl BuiltinToolExecutor {
    #[allow(clippy::too_many_lines)]
    async fn exec_tool_call(
        &self,
        ctx: &RunContext,
        lease: &mut SessionLease,
        call: &ToolCall,
        hooks: &RunHooks,
    ) -> ToolResult {
        // Go `execToolCall`: an already-finished context never executes or asks.
        if ctx_expired(ctx) {
            return ToolResult {
                output: CANCELLED_TOOL_RESULT.to_string(),
                is_error: true,
            };
        }

        let args: Args = match serde_json::from_str(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolResult {
                    output: format!(
                        "invalid tool arguments: {}; received: {}",
                        e,
                        truncate_utf8(&call.arguments, 200)
                    ),
                    is_error: true,
                };
            }
        };

        let name = call.name.as_str();
        match name {
            "shell" | "read_file" | "write_file" | "apply_patch" => {}
            _ => {
                return ToolResult {
                    output: format!("unknown tool: {name}"),
                    is_error: true,
                };
            }
        }

        // Owned configuration up front so `lease.rollout` can be borrowed
        // mutably for the apply_patch record without an overlapping borrow.
        let (sandbox, approval, cwd) = {
            let o = lease.options();
            (o.sandbox, o.approval, o.cwd.clone())
        };
        let bound_cwd = lease.bound_cwd.clone();
        let root_paths = lease.root_paths.clone();
        let bind_err = lease.bind_err.clone();
        let thread_id = lease.id().to_string();
        let emitter = hooks.emitter.clone();
        let approver = hooks.approver.clone();

        // Go: workspace-write write_file / apply_patch need a healthy bound cwd.
        if sandbox == Sandbox::WorkspaceWrite && (name == "write_file" || name == "apply_patch") {
            if let Some(e) = &bind_err {
                return ToolResult {
                    output: format!("error: {e}"),
                    is_error: true,
                };
            }
            let Some(bc) = &bound_cwd else {
                return ToolResult {
                    output: "error: working directory is not bound".to_string(),
                    is_error: true,
                };
            };
            if let Err(e) = bc.check() {
                return ToolResult {
                    output: format!("error: {e}"),
                    is_error: true,
                };
            }
        }

        // Parse a patch and build the per-call policy requests. apply_patch is
        // checked as one `write_file` request per touched path.
        let mut parsed_patch: Option<Patch> = None;
        let paths: Option<Vec<String>> = if name == "apply_patch" {
            let p = match parse_patch(&args.patch) {
                Ok(p) => p,
                Err(e) => {
                    return ToolResult {
                        output: format!("error: invalid patch: {}", e.message),
                        is_error: true,
                    };
                }
            };
            let ps = p.paths();
            parsed_patch = Some(p);
            Some(ps)
        } else {
            None
        };

        let requests: Vec<PolicyRequest> = if let Some(ps) = &paths {
            ps.iter()
                .map(|p| {
                    policy_request(
                        ToolKind::WriteFile,
                        None,
                        Some(p.clone()),
                        &cwd,
                        bound_cwd.as_deref(),
                    )
                })
                .collect()
        } else {
            vec![policy_request(
                tool_kind(name),
                if name == "shell" {
                    Some(args.command.clone())
                } else {
                    None
                },
                if args.path.is_empty() {
                    None
                } else {
                    Some(args.path.clone())
                },
                &cwd,
                bound_cwd.as_deref(),
            )]
        };

        // Go `authorize`: any Deny rejects the call; all AskApproval reasons
        // and paths are combined into a single approval prompt.
        let mut reasons: Vec<String> = Vec::new();
        let mut approval_paths: Vec<String> = Vec::new();
        for req in &requests {
            let (decision, reason) = evaluate_with_reason(req, sandbox, approval, &cwd);
            match decision {
                Decision::Deny => {
                    return ToolResult {
                        output: format!("operation denied by sandbox policy: {reason}"),
                        is_error: true,
                    };
                }
                Decision::AskApproval => {
                    reasons.push(reason);
                    if let Some(p) = &req.path
                        && !p.is_empty()
                    {
                        approval_paths.push(p.clone());
                    }
                }
                Decision::Allow => {}
            }
        }
        let mut approved = false;
        if !reasons.is_empty() {
            let mut approval_reason = reasons.join("; ");
            if !args.justification.is_empty() {
                approval_reason = format!(
                    "{approval_reason} (model justification: {})",
                    args.justification
                );
            }
            if let Some(esc) = escalations_for(name, sandbox) {
                approval_reason = format!("{approval_reason} [approving this {esc}]");
            }
            let req = ApprovalRequest {
                thread_id: thread_id.clone(),
                tool: name.to_string(),
                command: if args.command.is_empty() {
                    None
                } else {
                    Some(args.command.clone())
                },
                path: if approval_paths.is_empty() {
                    None
                } else {
                    Some(approval_paths.join(", "))
                },
                reason: approval_reason,
            };
            approved = wait_approval(ctx, approver.as_ref(), req).await;
            if !approved {
                return ToolResult {
                    output: "operation denied: approval was not granted".to_string(),
                    is_error: true,
                };
            }
        }

        // begin / end events. The ExecEnd guard is created BEFORE the begin
        // emit so a panic there still produces the end event (Go defer).
        let mut begin = json!({
            "type": "exec_command_begin",
            "call_id": &call.id,
            "tool": name,
        });
        match name {
            "shell" => begin["command"] = json!(&args.command),
            "apply_patch" => {
                if let Some(ps) = &paths {
                    begin["paths"] = json!(ps);
                }
            }
            _ => begin["path"] = json!(&args.path),
        }
        let mut end = ExecEnd {
            emitter: emitter.clone(),
            thread_id: thread_id.clone(),
            call_id: call.id.clone(),
            tool: name.to_string(),
            exit_code: if name == "shell" { Some(-1) } else { None },
            tool_err: None,
            paths: paths.clone(),
        };
        emitter.emit(&thread_id, begin);

        let result: ToolResult = match name {
            "shell" => {
                // Go: timeout = TimeoutSeconds * time.Second (non-positive is
                // clamped to the 60s default by the shell tool).
                let timeout = if args.timeout_seconds > 0 {
                    Some(Duration::from_secs(args.timeout_seconds as u64))
                } else {
                    None
                };
                let sandboxed = sandbox != Sandbox::DangerFullAccess;
                let sr: ShellResult = if sandboxed && !approved {
                    if let Some(e) = &bind_err {
                        // Go: err = bindErr, code = -1, out = "".
                        ShellResult {
                            output: String::new(),
                            exit_code: -1,
                            error: Some(e.clone()),
                            is_error: true,
                        }
                    } else {
                        let self_exe = std::env::current_exe().unwrap_or_default();
                        let launcher = LandlockLauncher::new(self_exe, root_paths.clone());
                        // An approved escalation omits the launcher entirely.
                        run_shell(ctx, &cwd, &args.command, timeout, Some(&launcher)).await
                    }
                } else {
                    run_shell(ctx, &cwd, &args.command, timeout, None).await
                };
                end.exit_code = Some(sr.exit_code);
                end.tool_err = sr.error.clone();
                let mut out = format!("exit code: {}\n{}", sr.exit_code, sr.output);
                if let Some(e) = &sr.error {
                    out.push_str(&format!("\n[error: {e}]"));
                }
                ToolResult {
                    output: out,
                    is_error: sr.is_error,
                }
            }
            "read_file" => {
                match read_file_range(ctx, &cwd, &args.path, args.offset, args.limit).await {
                    Ok(content) => ToolResult {
                        output: content,
                        is_error: false,
                    },
                    Err(e) => {
                        end.tool_err = Some(e.message.clone());
                        ToolResult {
                            output: format!("error: {}", e.message),
                            is_error: true,
                        }
                    }
                }
            }
            "write_file" => {
                let confined = sandbox == Sandbox::WorkspaceWrite && !approved;
                let written: Result<usize, ToolError> = if confined {
                    let bc = bound_cwd.as_deref().unwrap();
                    match confined_path(&cwd, &args.path, bc) {
                        Ok(target) => {
                            let os = OS;
                            let fs: &dyn FS = bc.fs().unwrap_or(&os);
                            write_target(fs, &target, &args.content)
                        }
                        Err(pe) => Err(ToolError::internal(pe.message)),
                    }
                } else {
                    // Go `tools.WriteFile(cwd, path, content)`: absolute paths
                    // verbatim, relative paths joined to cwd and cleaned.
                    let resolved = if Path::new(&args.path).is_absolute() {
                        PathBuf::from(&args.path)
                    } else {
                        lexical_clean(&cwd.join(&args.path))
                    };
                    write_target(&OS, &resolved, &args.content)
                };
                match written {
                    Ok(n) => ToolResult {
                        output: format!("wrote {n} bytes to {}", args.path),
                        is_error: false,
                    },
                    Err(e) => {
                        end.tool_err = Some(e.message.clone());
                        ToolResult {
                            output: format!("error: {}", e.message),
                            is_error: true,
                        }
                    }
                }
            }
            "apply_patch" => {
                let patch = parsed_patch.unwrap();
                let confined = sandbox == Sandbox::WorkspaceWrite && !approved;
                match plan_and_commit(&cwd, &patch.hunks, confined, bound_cwd.as_deref()) {
                    // Go: a BoundPath mapping failure returns WITHOUT recording.
                    Err(ApplyFailure::Mapping(m)) => {
                        end.tool_err = Some(m.clone());
                        ToolResult {
                            output: format!("error: {m}"),
                            is_error: true,
                        }
                    }
                    Err(ApplyFailure::Execute(m)) => {
                        let r = format!("error: {m}");
                        end.tool_err = Some(m);
                        if let Some(rollout) = lease.rollout.as_mut() {
                            rollout.record_patch_applied(
                                PATCH_APPLY_TURN_ID,
                                &call.id,
                                false,
                                "",
                                &r,
                                &[],
                            );
                        }
                        ToolResult {
                            output: r,
                            is_error: true,
                        }
                    }
                    Ok(changes) => {
                        let r = summary(&changes);
                        if let Some(rollout) = lease.rollout.as_mut() {
                            rollout.record_patch_applied(
                                PATCH_APPLY_TURN_ID,
                                &call.id,
                                true,
                                &r,
                                "",
                                &changes,
                            );
                        }
                        ToolResult {
                            output: r,
                            is_error: false,
                        }
                    }
                }
            }
            _ => unreachable!("tool name was checked above"),
        };
        result
    }
}
