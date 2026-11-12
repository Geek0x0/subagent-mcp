//! System-prompt construction (Go `internal/agent/agentsmd.go` +
//! `internal/server/server.go` prompt assembly).
//!
//! `system_prompt` composes the base instructions (default or overridden),
//! discovered `AGENTS.md` files, and developer instructions exactly as Go does.
//! `load_agents_md` ports `LoadAgentsMD`: it resolves symlinks in cwd, walks
//! from the nearest `.git` ancestor down to cwd, and concatenates the files
//! into `<agents_md path=...>` blocks capped at 32 KiB of content.

use std::io;
use std::path::{Path, PathBuf};

use crate::util::fs::eval_symlinks;
use crate::util::json::go_quote;

/// Go `agentsMDMaxBytes`.
pub const AGENTS_MD_MAX_BYTES: usize = 32 * 1024;

/// The built-in base system instructions (Go `DefaultSystemPrompt`). Kept
/// byte-for-byte identical, including the absence of a trailing newline.
pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are subagent-mcp, a coding agent. You work inside a fixed working directory using four tools:

- shell: run a bash command in the working directory
- read_file: read a file (relative paths resolve against the working directory)
- write_file: create or overwrite a whole file (parent directories are created)
- apply_patch: edit files with a *** Begin Patch / *** End Patch patch; prefer it for changing existing files

Work autonomously on the task you are given: inspect what you need, make the smallest change that satisfies the request, and verify it when possible. Some calls may be denied by the sandbox policy or the user; when that happens, adapt your approach or explain the blocker instead of repeating the same call. When the task is done, reply WITHOUT any tool call: summarize what you did, list changed files, and how you verified the result."#;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PromptError {
    pub message: String,
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PromptError {}

fn prompt_error(message: impl Into<String>) -> PromptError {
    PromptError {
        message: message.into(),
    }
}

/// Build the full system prompt (Go `handle_start` in `internal/server/server.go`).
///
/// `base` overrides the built-in default when non-empty; `None` or `Some("")`
/// select [`DEFAULT_SYSTEM_PROMPT`]. `AGENTS.md` content (when present) and
/// `developer` instructions (when non-empty) are appended, each preceded by a
/// blank-line separator, in that order.
pub fn system_prompt(
    base: Option<&str>,
    developer: Option<&str>,
    cwd: &Path,
) -> Result<String, PromptError> {
    let mut out = match base {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => DEFAULT_SYSTEM_PROMPT.to_string(),
    };

    let agents_md = load_agents_md(cwd)?;
    if !agents_md.is_empty() {
        out.push_str("\n\n");
        out.push_str(&agents_md);
    }

    if let Some(d) = developer
        && !d.is_empty()
    {
        out.push_str("\n\n");
        out.push_str(d);
    }

    Ok(out)
}

/// Go `LoadAgentsMD`: collect `AGENTS.md` files from the repository root (the
/// nearest `.git` ancestor of cwd, or cwd itself when outside a repo) down to
/// cwd, each wrapped in an `<agents_md>` block, capped at [`AGENTS_MD_MAX_BYTES`]
/// bytes of content.
pub fn load_agents_md(cwd: &Path) -> Result<String, PromptError> {
    // Go `filepath.EvalSymlinks(cwd)`.
    let dir =
        eval_symlinks(cwd).map_err(|e| prompt_error(format!("resolve cwd for AGENTS.md: {e}")))?;

    // Go `repo.Root(dir)`, else `root = dir`.
    let root = repo_root(&dir).unwrap_or_else(|| dir.clone());

    // dirs = [cwd, ..., root] (cwd first, root last).
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut current = dir.clone();
    loop {
        dirs.push(current.clone());
        if current == root {
            break;
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }

    let mut out = String::new();
    let mut remaining = AGENTS_MD_MAX_BYTES;

    // Iterate root -> cwd (Go iterates `dirs` in reverse).
    for d in dirs.iter().rev() {
        if remaining == 0 {
            break;
        }
        let path = d.join("AGENTS.md");
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(prompt_error(format!("read {}: {}", path.display(), e)));
            }
        };

        let truncated = data.len() > remaining;
        let mut content = data;
        if truncated {
            content.truncate(remaining);
            // Back off only a rune split by the cut (Go `utf8.UTFMax-1` passes);
            // invalid bytes earlier in the file are kept as-is.
            for _ in 0..3 {
                if content.is_empty() || !trailing_rune_is_error_size_one(&content) {
                    break;
                }
                content.pop();
            }
        }

        remaining = remaining.saturating_sub(content.len());
        if truncated {
            remaining = 0;
        }

        if !out.is_empty() {
            out.push_str("\n\n");
        }
        // Go `fmt.Fprintf(&out, "<agents_md path=%q>\n%s\n", path, content)`.
        out.push_str(&format!(
            "<agents_md path={}>\n",
            go_quote(&path.to_string_lossy())
        ));
        // Rust strings must be valid UTF-8; Go may keep raw invalid bytes, so
        // lossy-convert just the file content (this only affects files with
        // invalid UTF-8, never the framing or quoting).
        out.push_str(&String::from_utf8_lossy(&content));
        out.push('\n');
        if truncated {
            out.push_str(&format!(
                "[AGENTS.md content truncated at {AGENTS_MD_MAX_BYTES} bytes]\n"
            ));
        }
        out.push_str("</agents_md>");
    }

    Ok(out)
}

/// Go `repo.Root`: nearest ancestor (inclusive) containing a `.git` entry.
/// `symlink_metadata` is Go `os.Lstat`, so a `.git` symlink counts too.
fn repo_root(dir: &Path) -> Option<PathBuf> {
    let mut current = dir.to_path_buf();
    loop {
        if std::fs::symlink_metadata(current.join(".git")).is_ok() {
            return Some(current);
        }
        match current.parent() {
            Some(parent) if parent != current && !parent.as_os_str().is_empty() => {
                current = parent.to_path_buf();
            }
            _ => return None,
        }
    }
}

fn is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// Whether Go's `utf8.DecodeLastRuneInString` would return `(RuneError, 1)` for
/// the tail of `content`. That is exactly the condition Go uses to back off a
/// byte after an oversized byte cut: the last rune is an incomplete or invalid
/// UTF-8 sequence. A valid trailing rune (ASCII, a complete multibyte rune, or
/// even a literal U+FFFD) returns `false` and stops the back-off loop.
fn trailing_rune_is_error_size_one(content: &[u8]) -> bool {
    if content.is_empty() {
        return false;
    }
    // Scan back up to utf8.UTFMax-1 = 3 continuation bytes to find the start
    // byte of the last rune.
    let end = content.len();
    let mut start = end - 1;
    let mut steps = 0;
    while start > 0 && is_continuation(content[start]) && steps < 3 {
        start -= 1;
        steps += 1;
    }
    // The tail must decode as valid UTF-8 for the last rune to be complete.
    std::str::from_utf8(&content[start..]).is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompt_has_no_trailing_newline() {
        assert!(!DEFAULT_SYSTEM_PROMPT.ends_with('\n'));
        assert_eq!(AGENTS_MD_MAX_BYTES, 32768);
    }

    #[test]
    fn trailing_rune_backoff_matches_go() {
        // A complete trailing rune is not backed off.
        assert!(!trailing_rune_is_error_size_one("中".as_bytes()));
        assert!(!trailing_rune_is_error_size_one(b"abc"));
        // A split 3-byte rune is backed off.
        assert!(trailing_rune_is_error_size_one(&[0xE4, 0xB8]));
        assert!(trailing_rune_is_error_size_one(&[0xE4]));
        // A literal U+FFFD (EF BF BD) is valid and not backed off.
        assert!(!trailing_rune_is_error_size_one(&[0xEF, 0xBF, 0xBD]));
    }
}
