//! Repository layout read directly from `.git` files, without invoking `git`
//! or depending on `PATH`. Ports Go `internal/repo/repo.go` (v0.13.1).
//!
//! `git_info` combines Go's `repo.Root` (walk ancestors for a `.git` entry)
//! with `repo.Head` (read `HEAD`, loose refs, `packed-refs`, and `commondir`).
//! Unknown values are `None` (Go's empty strings); every read/parse failure
//! yields empty fields.

use std::path::{Path, PathBuf};

/// The checked-out branch and commit for the repository containing `cwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: Option<String>,
    pub commit: Option<String>,
}

impl GitInfo {
    fn empty() -> Self {
        Self {
            branch: None,
            commit: None,
        }
    }
}

/// Go's empty string maps to `None`; a non-empty value maps to `Some`.
fn opt(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Return the branch and commit for the nearest ancestor of `cwd` (inclusive)
/// that contains a `.git` entry. Empty on any read/parse failure.
pub fn git_info(cwd: &Path) -> GitInfo {
    match root(cwd) {
        Some(root) => head(&root),
        None => GitInfo::empty(),
    }
}

/// Nearest ancestor of `dir` (inclusive) containing a `.git` entry, matching
/// Go `repo.Root`. `symlink_metadata` is Go `os.Lstat`, so a `.git` symlink
/// counts too.
fn root(dir: &Path) -> Option<PathBuf> {
    let mut current = dir.to_path_buf();
    loop {
        if std::fs::symlink_metadata(current.join(".git")).is_ok() {
            return Some(current);
        }
        let parent = current.parent()?;
        if parent == current || parent.as_os_str().is_empty() {
            return None;
        }
        current = parent.to_path_buf();
    }
}

/// Read branch and commit from the repository rooted at `root`, matching Go
/// `repo.Head`.
fn head(root: &Path) -> GitInfo {
    let git_dot = root.join(".git");
    let mut git_dir = git_dot.clone();

    // A `.git` file (worktree / submodule) holds `gitdir: <target>`.
    if let Ok(data) = std::fs::read(&git_dot) {
        let raw = String::from_utf8_lossy(&data).into_owned();
        let target = raw.strip_prefix("gitdir:").unwrap_or(&raw).trim();
        let p = PathBuf::from(target);
        git_dir = if p.is_absolute() { p } else { root.join(p) };
    }

    let head_bytes = match std::fs::read(git_dir.join("HEAD")) {
        Ok(b) => b,
        Err(_) => return GitInfo::empty(),
    };
    let head_text = String::from_utf8_lossy(&head_bytes).into_owned();
    let head_trimmed = head_text.trim();

    let ref_path = match head_trimmed.strip_prefix("ref: ") {
        Some(r) => r,
        // Detached HEAD: branch unknown, commit is the raw hash.
        None => {
            return GitInfo {
                branch: None,
                commit: opt(head_trimmed),
            };
        }
    };
    let branch = ref_path.strip_prefix("refs/heads/").unwrap_or(ref_path);

    // `commondir` redirects shared refs to the main repository (worktrees).
    let mut common_dir = git_dir.clone();
    if let Ok(data) = std::fs::read(git_dir.join("commondir")) {
        let text = String::from_utf8_lossy(&data).into_owned();
        let c = text.trim();
        let p = PathBuf::from(c);
        common_dir = if p.is_absolute() { p } else { git_dir.join(p) };
    }

    // Loose refs live under either git_dir or common_dir.
    for dir in [git_dir.as_path(), common_dir.as_path()] {
        if let Ok(data) = std::fs::read(dir.join(ref_path)) {
            let hash = String::from_utf8_lossy(&data);
            return GitInfo {
                branch: opt(branch),
                commit: opt(hash.trim()),
            };
        }
    }

    // Fall back to packed-refs in the common dir.
    if let Ok(data) = std::fs::read(common_dir.join("packed-refs")) {
        let text = String::from_utf8_lossy(&data);
        for line in text.split('\n') {
            let line = line.trim();
            // Go `strings.Cut(trimmed, " ")`: split at the first space.
            if let Some((hash, name)) = line.split_once(' ')
                && name == ref_path
            {
                return GitInfo {
                    branch: opt(branch),
                    commit: opt(hash),
                };
            }
        }
    }

    GitInfo {
        branch: opt(branch),
        commit: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// Go `testutil.GitAncestor`: is any ancestor (inclusive) a git repo?
    fn has_git_ancestor(dir: &Path) -> bool {
        let mut cur = dir.to_path_buf();
        loop {
            if std::fs::symlink_metadata(cur.join(".git")).is_ok() {
                return true;
            }
            match cur.parent() {
                Some(p) if p != cur && !p.as_os_str().is_empty() => cur = p.to_path_buf(),
                _ => return false,
            }
        }
    }

    fn empty() -> GitInfo {
        GitInfo {
            branch: None,
            commit: None,
        }
    }

    #[test]
    fn loose_ref_returns_branch_and_commit() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.path().join(".git/refs/heads/main"), "abc123\n");
        assert_eq!(
            git_info(root.path()),
            GitInfo {
                branch: Some("main".into()),
                commit: Some("abc123".into()),
            }
        );
    }

    #[test]
    fn packed_ref_returns_branch_and_commit() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join(".git/HEAD"), "ref: refs/heads/dev\n");
        write(
            &root.path().join(".git/packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted\ndef456 refs/heads/dev\n",
        );
        assert_eq!(
            git_info(root.path()),
            GitInfo {
                branch: Some("dev".into()),
                commit: Some("def456".into()),
            }
        );
    }

    #[test]
    fn detached_head_returns_commit_only() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join(".git/HEAD"), "0123abcd\n");
        assert_eq!(
            git_info(root.path()),
            GitInfo {
                branch: None,
                commit: Some("0123abcd".into()),
            }
        );
    }

    #[test]
    fn worktree_commondir_resolves_refs() {
        let main = tempfile::tempdir().unwrap();
        let worktree_git_dir = main.path().join(".git/worktrees/wt");
        write(&worktree_git_dir.join("HEAD"), "ref: refs/heads/feature\n");
        write(&worktree_git_dir.join("commondir"), "../..\n");
        write(&main.path().join(".git/refs/heads/feature"), "fea7\n");
        let wt = tempfile::tempdir().unwrap();
        write(
            &wt.path().join(".git"),
            &format!("gitdir: {}\n", worktree_git_dir.display()),
        );
        assert_eq!(
            git_info(wt.path()),
            GitInfo {
                branch: Some("feature".into()),
                commit: Some("fea7".into()),
            }
        );
    }

    #[test]
    fn nested_cwd_walks_to_repo_root() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join(".git/HEAD"), "ref: refs/heads/main\n");
        write(&root.path().join(".git/refs/heads/main"), "abc123\n");
        let nested = root.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            git_info(&nested),
            GitInfo {
                branch: Some("main".into()),
                commit: Some("abc123".into()),
            }
        );
    }

    #[test]
    fn branch_without_commit_returns_none_commit() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join(".git/HEAD"), "ref: refs/heads/main\n");
        assert_eq!(
            git_info(root.path()),
            GitInfo {
                branch: Some("main".into()),
                commit: None,
            }
        );
    }

    #[test]
    fn git_dir_without_head_returns_empty() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        assert_eq!(git_info(root.path()), empty());
    }

    #[test]
    fn non_repo_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        // A temp dir is only a non-repo when no ancestor holds a .git.
        if !has_git_ancestor(dir.path()) {
            assert_eq!(git_info(dir.path()), empty());
        }
    }
}
