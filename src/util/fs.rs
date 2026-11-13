/// Go-compatible `os.SameFile`: true when both paths resolve to the same
/// underlying file (device + inode on Unix, volume + file index on Windows).
pub fn same_file(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    fn inner(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let ma = std::fs::metadata(a)?;
        let mb = std::fs::metadata(b)?;
        Ok(ma.dev() == mb.dev() && ma.ino() == mb.ino())
    }

    #[cfg(windows)]
    fn inner(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
        Ok(
            windows_file_identity(&open_identity(a)?)?
                == windows_file_identity(&open_identity(b)?)?,
        )
    }

    #[cfg(not(any(unix, windows)))]
    fn inner(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
        Ok(std::fs::canonicalize(a)? == std::fs::canonicalize(b)?)
    }

    inner(a, b)
}

/// Open files or directories for identity checks, following symlinks like Stat.
#[cfg(windows)]
pub(crate) fn open_identity(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(windows)]
pub(crate) fn windows_file_identity(file: &std::fs::File) -> std::io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns a valid handle; info is a correctly sized out parameter.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

/// Go-compatible `filepath.EvalSymlinks`: resolve every symbolic link and
/// return a cleaned path. `std::fs::canonicalize` also requires the target to
/// exist and resolves symlink loops, matching EvalSymlinks' error behavior;
/// unlike EvalSymlinks it always returns an absolute path (Go preserves a
/// relative path when no component is an absolute symlink).
pub fn eval_symlinks(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path)
}

/// Lexical filepath.Clean: collapse dots without following symbolic links.
pub fn lexical_clean(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn lexical_clean_keeps_windows_prefixes() {
        use std::path::Path;
        for (input, expected) in [
            (r"C:\a\..\b\", r"C:\b"),
            (r"\\server\share\a\..\b", r"\\server\share\b"),
            (r"C:a\..\b", r"C:b"),
        ] {
            assert_eq!(lexical_clean(Path::new(input)), Path::new(expected));
        }
    }

    #[cfg(windows)]
    #[test]
    fn same_file_recognizes_hardlinks() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"data").unwrap();
        std::fs::hard_link(&a, &b).unwrap();
        assert!(same_file(&a, &b).unwrap());
    }

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
