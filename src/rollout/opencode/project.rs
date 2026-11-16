//! Mapping a session's working directory to an OpenCode project.

use std::path::{Path, PathBuf};

use crate::util::fs::eval_symlinks;

/// One OpenCode project: its id and the directory it is rooted at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    pub id: String,
    pub canonical: PathBuf,
}

/// The project a session in `cwd` belongs to, or `None`.
///
/// Symlinks in `cwd` are resolved first. A project whose `canonical` equals the
/// git root (when given) or the resolved `cwd` wins; otherwise the nearest
/// ancestor directory that is a project. OpenCode's global project (rooted at
/// the filesystem root) is never returned: a session outside every real project
/// is skipped rather than filed under it.
pub fn pick_project<'a>(
    cwd: &Path,
    git_root: Option<&Path>,
    projects: &'a [ProjectRef],
) -> Option<&'a ProjectRef> {
    let resolved = eval_symlinks(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let root = git_root.map(|r| eval_symlinks(r).unwrap_or_else(|_| r.to_path_buf()));
    let find = |dir: &Path| {
        projects
            .iter()
            .find(|p| p.canonical.parent().is_some() && p.canonical == dir)
    };
    if let Some(project) = root.as_deref().and_then(find) {
        return Some(project);
    }
    resolved.ancestors().find_map(find)
}
