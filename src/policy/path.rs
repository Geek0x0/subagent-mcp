//! Policy decision matrix and path confinement.
//!
//! Ports `Evaluate`, `withinSandbox`, `insideRelative` and `BoundPath` from Go
//! `internal/policy/policy.go` (subagent-mcp v0.13.1). Path checks reject every
//! `..` component before touching the filesystem, resolve symlinks for the
//! existing part of the target, reattach a missing tail, and compare the
//! resolved target against the resolved working directory.
//!
//! The shared enums and request type live in [`super`]; this module adds only
//! behavior.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::lexer::analyze_shell;
use super::{ApprovalPolicy, Decision, PolicyError, PolicyRequest, Sandbox, ToolKind};
use crate::util::fs::eval_symlinks;
use crate::util::json::go_quote;

/// A target checked beneath a bound root, ready for the caller to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundPath {
    pub path: PathBuf,
}

/// Go `Evaluate`: combine the sandbox and approval rules for one request.
///
/// Returns only the decision. [`evaluate_with_reason`] is the full parity
/// interface and is what the tool executor uses to build approval text.
pub fn evaluate(
    req: &PolicyRequest,
    sandbox: Sandbox,
    approval: ApprovalPolicy,
    cwd: &Path,
) -> Decision {
    evaluate_with_reason(req, sandbox, approval, cwd).0
}

/// Go `Evaluate`: the decision plus the denial/approval reason.
///
/// The reason is empty exactly when the decision is [`Decision::Allow`].
pub fn evaluate_with_reason(
    req: &PolicyRequest,
    sandbox: Sandbox,
    approval: ApprovalPolicy,
    cwd: &Path,
) -> (Decision, String) {
    let trusted = is_trusted(req);
    let (in_sandbox, mut reason) = within_sandbox(sandbox, req, cwd);

    match approval {
        ApprovalPolicy::Untrusted => {
            if trusted {
                return (Decision::Allow, String::new());
            }
            if reason.is_empty() {
                reason =
                    "untrusted policy requires approval for non-whitelisted operations".to_string();
            }
            (Decision::AskApproval, reason)
        }
        ApprovalPolicy::Never => {
            if in_sandbox {
                (Decision::Allow, String::new())
            } else {
                (Decision::Deny, reason)
            }
        }
        ApprovalPolicy::OnRequest | ApprovalPolicy::OnFailure => {
            if in_sandbox {
                (Decision::Allow, String::new())
            } else {
                (Decision::AskApproval, reason)
            }
        }
    }
}

/// Go `withinSandbox`: whether the sandbox itself permits the request, plus
/// the exact reason when it does not.
pub fn within_sandbox(sandbox: Sandbox, req: &PolicyRequest, cwd: &Path) -> (bool, String) {
    match sandbox {
        Sandbox::ReadOnly => {
            if is_trusted(req) {
                (true, String::new())
            } else {
                (
                    false,
                    "operation is not permitted by the read-only sandbox".to_string(),
                )
            }
        }
        Sandbox::WorkspaceWrite => match &req.tool {
            ToolKind::ReadFile | ToolKind::Shell => (true, String::new()),
            ToolKind::WriteFile => {
                let target = Path::new(req.path.as_deref().unwrap_or(""));
                match inside_relative_bound(cwd, target, req.bound_cwd.as_deref()) {
                    Ok(_) => (true, String::new()),
                    Err(err) => (false, err.message),
                }
            }
            ToolKind::ApplyPatch | ToolKind::Other(_) => (
                false,
                format!(
                    "tool {} is not permitted by the workspace-write sandbox",
                    go_quote(tool_name(&req.tool))
                ),
            ),
        },
        Sandbox::DangerFullAccess => (true, String::new()),
    }
}

/// Go `isWhitelistedShell`, adapted to Task 14's lexer contract.
///
/// A lexical hazard (`Err`) is not trusted; a reasoned allowlist denial
/// (`Ok { allowed: false, .. }`) is not trusted either.
pub fn is_whitelisted_shell(command: &str) -> bool {
    analyze_shell(command).is_ok_and(|analysis| analysis.allowed)
}

fn is_trusted(req: &PolicyRequest) -> bool {
    match req.tool {
        ToolKind::ReadFile => true,
        ToolKind::Shell => is_whitelisted_shell(req.command.as_deref().unwrap_or("")),
        _ => false,
    }
}

fn tool_name(tool: &ToolKind) -> &str {
    match tool {
        ToolKind::Shell => "shell",
        ToolKind::ReadFile => "read_file",
        ToolKind::WriteFile => "write_file",
        ToolKind::ApplyPatch => "apply_patch",
        ToolKind::Other(name) => name,
    }
}

/// Go `BoundPath`: resolve `target` beneath `root` and return the held-root
/// path the caller must use for the actual operation.
pub fn bound_path(root: &Path, target: &Path) -> Result<BoundPath, PolicyError> {
    // Go returns this error for a nil `*sandbox.Directory`; an empty root is the
    // natural path-only sentinel for the same "no bound working directory" case.
    if root.as_os_str().is_empty() {
        return Err(policy_error("working directory is not bound"));
    }
    let relative = inside_relative(root, target)?;
    Ok(BoundPath {
        path: lexical_clean(&root.join(relative)),
    })
}

/// Go `insideRelative`: the relative path from `cwd` to `target`, after
/// resolving symlinks, or a reason the target is not confined.
pub fn inside_relative(cwd: &Path, target: &Path) -> Result<PathBuf, PolicyError> {
    inside_relative_bound(cwd, target, None)
}

fn inside_relative_bound(
    cwd: &Path,
    target: &Path,
    bound: Option<&Path>,
) -> Result<PathBuf, PolicyError> {
    reject_parent_components(target)?;

    let cwd_absolute = go_abs(cwd).map_err(|err| {
        policy_error(format!(
            "resolve working directory {}: {}",
            quote_path(cwd),
            err
        ))
    })?;

    let resolved_cwd = match bound {
        Some(bound) => eval_symlinks(bound).map_err(|err| {
            policy_error(format!(
                "resolve working directory {}: {}",
                quote_path(bound),
                err
            ))
        })?,
        None => eval_symlinks(&cwd_absolute).map_err(|err| {
            policy_error(format!(
                "resolve working directory {}: {}",
                quote_path(cwd),
                err
            ))
        })?,
    };

    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        cwd_absolute.join(target)
    };
    let target = lexical_clean(&joined);

    let (ancestor, missing_parts) = existing_ancestor(&target)?;
    let resolved_ancestor = eval_symlinks(&ancestor).map_err(|err| {
        policy_error(format!(
            "resolve target ancestor {}: {}",
            quote_path(&ancestor),
            err
        ))
    })?;
    let mut resolved_target = resolved_ancestor;
    for part in missing_parts.iter().rev() {
        resolved_target.push(part);
    }

    let relative = filepath_rel(&resolved_cwd, &resolved_target).map_err(|reason| {
        policy_error(format!(
            "compare target {} with working directory {}: {}",
            quote_path(&target),
            quote_path(&resolved_cwd),
            reason
        ))
    })?;
    if relative.components().next() == Some(Component::ParentDir) {
        return Err(policy_error(format!(
            "target {} resolves outside working directory {}",
            quote_path(&target),
            quote_path(&resolved_cwd)
        )));
    }

    // Go re-checks the bound directory after resolution. The Rust request
    // carries a path rather than an inode, so re-resolve it and require the
    // same answer; the inode-level `Check` arrives with `sandbox::BoundDirectory`.
    if let Some(bound) = bound {
        let rechecked = eval_symlinks(bound).map_err(|err| {
            policy_error(format!(
                "resolve working directory {}: {}",
                quote_path(bound),
                err
            ))
        })?;
        if rechecked != resolved_cwd {
            return Err(policy_error(format!(
                "bound directory {} no longer resolves to the session's original directory",
                quote_path(bound)
            )));
        }
    }

    Ok(relative)
}

fn reject_parent_components(target: &Path) -> Result<(), PolicyError> {
    for component in target.components() {
        if component == Component::ParentDir {
            return Err(policy_error(format!(
                "target {} contains a parent-directory component",
                quote_path(target)
            )));
        }
    }
    Ok(())
}

/// Go `filepath.Abs` for an absolute or relative path: join the current
/// directory when needed, then clean lexically.
fn go_abs(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(lexical_clean(&absolute))
}

/// Go `filepath.Clean`: remove `.` components and resolve `..` lexically.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Walk up from `target` until an existing component is found, collecting the
/// missing tail in leaf-to-root order.
fn existing_ancestor(target: &Path) -> Result<(PathBuf, Vec<OsString>), PolicyError> {
    let mut ancestor = target.to_path_buf();
    let mut missing_parts = Vec::new();

    loop {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(_) => break,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let Some(parent) = ancestor.parent() else {
                    return Err(no_existing_ancestor(target));
                };
                if parent == ancestor {
                    return Err(no_existing_ancestor(target));
                }
                if let Some(name) = ancestor.file_name() {
                    missing_parts.push(name.to_os_string());
                }
                ancestor = parent.to_path_buf();
            }
            Err(err) => {
                return Err(policy_error(format!(
                    "inspect target ancestor {}: {}",
                    quote_path(&ancestor),
                    err
                )));
            }
        }
    }

    Ok((ancestor, missing_parts))
}

fn no_existing_ancestor(target: &Path) -> PolicyError {
    policy_error(format!(
        "no existing ancestor for target {}",
        quote_path(target)
    ))
}

/// Go `filepath.Rel` for two absolute, cleaned paths.
fn filepath_rel(base: &Path, target: &Path) -> Result<PathBuf, String> {
    let base_components: Vec<Component<'_>> = base.components().collect();
    let target_components: Vec<Component<'_>> = target.components().collect();

    if !prefixes_match(&base_components, &target_components) {
        return Err(format!(
            "Rel: can't make {} relative to {}",
            target.display(),
            base.display()
        ));
    }

    let mut common = 0;
    while common < base_components.len()
        && common < target_components.len()
        && base_components[common] == target_components[common]
    {
        common += 1;
    }

    let mut relative = PathBuf::new();
    for _ in common..base_components.len() {
        relative.push("..");
    }
    for component in &target_components[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    Ok(relative)
}

fn prefixes_match(base: &[Component<'_>], target: &[Component<'_>]) -> bool {
    match (base.first(), target.first()) {
        (Some(Component::Prefix(a)), Some(Component::Prefix(b))) => a == b,
        (Some(Component::Prefix(_)), _) | (_, Some(Component::Prefix(_))) => false,
        _ => true,
    }
}

fn quote_path(path: &Path) -> String {
    go_quote(&path.to_string_lossy())
}

fn policy_error(message: impl Into<String>) -> PolicyError {
    PolicyError {
        message: message.into(),
    }
}
