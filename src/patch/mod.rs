//! Codex `apply_patch` engine: parser, in-memory plan, and snapshot/rollback
//! commit.
//!
//! Ports Go `internal/patch/parse.go` and `internal/patch/apply.go`. The parser
//! accepts only the Codex patch format (`*** Begin Patch` / `*** End Patch`).
//! `plan_fs` composes every section against an in-memory overlay and writes
//! nothing; `commit_fs` writes changes in order and restores every touched path
//! if any step fails. All file content is handled as raw bytes, matching Go's
//! byte-oriented `string` operations.

pub mod apply;
pub mod parse;

use std::fmt;
use std::path::PathBuf;

pub use apply::{FS, OS, commit_fs, plan_fs};
pub use parse::parse_patch;

/// Go `patch.Kind`. Shared by hunks and planned changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Add,
    Delete,
    Update,
}

impl ChangeKind {
    /// Go `Kind.String()`, consumed by the rollout recorder.
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Add => "add",
            ChangeKind::Delete => "delete",
            ChangeKind::Update => "update",
        }
    }
}

/// Go `patch.Chunk`. Lines are raw bytes so file content need not be UTF-8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub header: Vec<u8>,
    pub old: Vec<Vec<u8>>,
    pub new: Vec<Vec<u8>>,
    pub raw: Vec<Vec<u8>>,
    pub eof: bool,
}

/// Go `patch.Hunk`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub kind: ChangeKind,
    pub path: String,
    pub move_to: String,
    pub content: Vec<u8>,
    pub chunks: Vec<Chunk>,
}

/// A parsed patch. `cwd` is the base directory relative paths resolve against
/// during planning; callers set it to the session working directory (absolute
/// paths in the patch are already resolved, matching Go's `BoundPath` path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch {
    pub cwd: PathBuf,
    pub hunks: Vec<Hunk>,
}

impl Patch {
    /// Go `patch.Paths`: every path the patch touches, as written, in order.
    pub fn paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        for hunk in &self.hunks {
            paths.push(hunk.path.clone());
            if !hunk.move_to.is_empty() {
                paths.push(hunk.move_to.clone());
            }
        }
        paths
    }
}

/// Go `patch.Change`. `None` `move_to`/`new_content` map to Go's `""` sentinel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub kind: ChangeKind,
    pub path: PathBuf,
    pub move_to: Option<PathBuf>,
    pub new_content: Option<Vec<u8>>,
    pub diff: String,
}

/// The crate's error convention: a message string, `Display`, and `Error`.
#[derive(Clone, Debug)]
pub struct PatchError {
    pub message: String,
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PatchError {}

pub(crate) fn err(message: impl Into<String>) -> PatchError {
    PatchError {
        message: message.into(),
    }
}

/// Byte content rendered as text. Patch-derived bytes are valid UTF-8; the
/// lossy fallback only guards against unexpected invalid input.
pub(crate) fn bytes_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Go `patch.Summary`: the Codex-style success message.
pub fn summary(changes: &[Change]) -> String {
    let mut out = String::from("Success. Updated the following files:");
    for change in changes {
        let letter = match change.kind {
            ChangeKind::Add => "A",
            ChangeKind::Delete => "D",
            ChangeKind::Update => "M",
        };
        let path = change.move_to.as_deref().unwrap_or(change.path.as_path());
        out.push_str(&format!("\n{} {}", letter, path.display()));
    }
    out
}
