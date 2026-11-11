//! Shared policy types.
//!
//! This module owns the enums, request struct and error type so that later
//! tasks (Task 14 lexer, Task 15 matrix/path) can add behavior without a type
//! dependency on those tasks. Behavior (evaluation, lexing, path resolution)
//! is intentionally not defined here.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    OnFailure,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Decision {
    Allow,
    AskApproval,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ToolKind {
    Shell,
    ReadFile,
    WriteFile,
    ApplyPatch,
    Other(String),
}

#[derive(Clone, Debug)]
pub struct PolicyRequest {
    pub tool: ToolKind,
    pub command: Option<String>,
    pub path: Option<String>,
    pub cwd: std::path::PathBuf,
    pub bound_cwd: Option<std::path::PathBuf>,
}

#[derive(Clone, Debug)]
pub struct PolicyError {
    pub message: String,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_variants_are_distinct() {
        assert_eq!(Decision::Allow, Decision::Allow);
        assert_ne!(Decision::Allow, Decision::Deny);
        assert_ne!(Decision::AskApproval, Decision::Deny);
    }

    #[test]
    fn shared_types_construct() {
        let req = PolicyRequest {
            tool: ToolKind::Shell,
            command: Some("ls".into()),
            path: None,
            cwd: std::path::PathBuf::from("/tmp"),
            bound_cwd: None,
        };
        assert_eq!(req.tool, ToolKind::Shell);
        assert_eq!(
            ToolKind::Other("custom".into()),
            ToolKind::Other("custom".into())
        );

        let err = PolicyError {
            message: "working directory is not bound".into(),
        };
        assert_eq!(err.to_string(), "working directory is not bound");

        assert_ne!(Sandbox::ReadOnly, Sandbox::WorkspaceWrite);
        assert_ne!(ApprovalPolicy::Never, ApprovalPolicy::OnRequest);
    }
}
