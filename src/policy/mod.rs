//! Policy types and behavior.
//!
//! This module owns the shared enums, request struct and error type so that
//! the lexer and path modules can add behavior without a type dependency
//! between them. The shell lexer lives in [`lexer`]; the decision matrix and
//! path confinement live in [`path`].

pub mod lexer;
pub mod path;

pub use lexer::{LexError, ShellAnalysis, analyze_shell};
pub use path::{
    BoundPath, bound_path, evaluate, evaluate_with_reason, inside_relative, within_sandbox,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl Sandbox {
    /// Rank by how much the mode grants; a server-side ceiling keeps the
    /// minimum of the requested and configured ranks.
    pub fn rank(self) -> u8 {
        match self {
            Sandbox::ReadOnly => 0,
            Sandbox::WorkspaceWrite => 1,
            Sandbox::DangerFullAccess => 2,
        }
    }

    /// The wire spelling accepted by the `start` tool.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read-only" => Some(Sandbox::ReadOnly),
            "workspace-write" => Some(Sandbox::WorkspaceWrite),
            "danger-full-access" => Some(Sandbox::DangerFullAccess),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Sandbox::ReadOnly => "read-only",
            Sandbox::WorkspaceWrite => "workspace-write",
            Sandbox::DangerFullAccess => "danger-full-access",
        }
    }

    /// Clamp to `ceiling` when the requested mode grants more access.
    pub fn clamp_to(self, ceiling: Option<Self>) -> Self {
        match ceiling {
            Some(ceiling) if self.rank() > ceiling.rank() => ceiling,
            _ => self,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    OnFailure,
    Never,
}

impl ApprovalPolicy {
    /// Rank by how few prompts the policy emits; `Never` is the most
    /// permissive, so a server-side ceiling clamps towards lower ranks.
    pub fn rank(self) -> u8 {
        match self {
            ApprovalPolicy::Untrusted => 0,
            ApprovalPolicy::OnRequest => 1,
            ApprovalPolicy::OnFailure => 2,
            ApprovalPolicy::Never => 3,
        }
    }

    /// The wire spelling accepted by the `start` tool.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "untrusted" => Some(ApprovalPolicy::Untrusted),
            "on-request" => Some(ApprovalPolicy::OnRequest),
            "on-failure" => Some(ApprovalPolicy::OnFailure),
            "never" => Some(ApprovalPolicy::Never),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalPolicy::Untrusted => "untrusted",
            ApprovalPolicy::OnRequest => "on-request",
            ApprovalPolicy::OnFailure => "on-failure",
            ApprovalPolicy::Never => "never",
        }
    }

    /// Clamp to `ceiling` when the requested policy asks for fewer prompts.
    pub fn clamp_to(self, ceiling: Option<Self>) -> Self {
        match ceiling {
            Some(ceiling) if self.rank() > ceiling.rank() => ceiling,
            _ => self,
        }
    }
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
