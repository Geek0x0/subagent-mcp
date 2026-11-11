/// Go-compatible `os.SameFile`: true when both paths resolve to the same
/// underlying file (device + inode on Unix). On non-Unix platforms fall back to
/// comparing canonical paths, which preserves the observable behavior for all
/// tested cases without depending on platform-specific std APIs.
pub fn same_file(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    fn inner(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let ma = std::fs::metadata(a)?;
        let mb = std::fs::metadata(b)?;
        Ok(ma.dev() == mb.dev() && ma.ino() == mb.ino())
    }

    #[cfg(not(unix))]
    fn inner(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
        Ok(std::fs::canonicalize(a)? == std::fs::canonicalize(b)?)
    }

    inner(a, b)
}

/// Go-compatible `filepath.EvalSymlinks`: resolve every symbolic link and
/// return a cleaned path. `std::fs::canonicalize` also requires the target to
/// exist and resolves symlink loops, matching EvalSymlinks' error behavior;
/// unlike EvalSymlinks it always returns an absolute path (Go preserves a
/// relative path when no component is an absolute symlink).
pub fn eval_symlinks(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_two_paths_same_inode() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        // A path spelling that resolves to the very same inode (no link needed,
        // so this stays valid on every platform).
        let with_dot = dir.path().join(".").join("a.txt");
        assert!(same_file(&file, &with_dot).unwrap());
    }

    #[test]
    fn same_file_distinct_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        assert!(!same_file(&a, &b).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn eval_symlinks_resolves_target() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"x").unwrap();
        let link = dir.path().join("link.txt");
        symlink(&target, &link).unwrap();
        let resolved = eval_symlinks(&link).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&target).unwrap());
    }
}
