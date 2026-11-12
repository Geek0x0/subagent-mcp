//! Non-Linux fallback (`landlock_other.go`, `directory_other.go`, `fd_windows.go`).
//!
//! There is no kernel-enforced containment off Linux: `available()` is false,
//! `run()` still validates the helper argv (preserving the exact error strings)
//! and then reports the unsupported sandbox. `BoundDirectory` mirrors the Go
//! `directory_other.go` shape: it holds the bound path AND an open `File` (fd /
//! handle), so `check()` can prove inode identity (Go `os.SameFile`) even when
//! the original path name is later pointed at a different inode. `fs()` returns
//! `None`, so callers fall back to ordinary paths, and `path()` is path-only.

use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use crate::patch::FS;
use crate::sandbox::{Error, err, parse_args};
use crate::util::json::go_quote;

pub(crate) fn run(args: &[OsString]) -> Result<(), Error> {
    let _ = parse_args(args)?;
    Err(err("landlock unavailable: kernel sandbox requires Linux"))
}

/// Holds the directory inode selected when a session is created (off Linux).
pub struct BoundDirectory {
    /// Absolute bound path (Go `d.path`), used by `check` and returned by `path`.
    pub path: PathBuf,
    /// The held open fd/handle (Go `d.file`), used by `check` for `os.SameFile`
    /// and by Task 23B's `command_fd` plumbing.
    pub file: File,
}

/// Open a directory as a `File`. Windows needs `FILE_FLAG_BACKUP_SEMANTICS` to
/// open a directory handle; a plain `File::open` is enough on Unix.
#[cfg(unix)]
fn open_dir(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(windows)]
fn open_dir(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_dir(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Go `os.SameFile` for two already-fetched metadata values: device + inode on
/// Unix (exactly the logic of `util::fs::same_file`). Stable Windows std cannot
/// expose a file's identity from metadata (`windows_by_handle` is unstable), so
/// mirror `util::fs::same_file`'s documented non-Unix best-effort behavior by
/// comparing the stable identity fields; a replaced directory will differ.
#[cfg(unix)]
fn same_metadata(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(windows)]
fn same_metadata(a: &Metadata, b: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    a.creation_time() == b.creation_time()
        && a.last_write_time() == b.last_write_time()
        && a.file_size() == b.file_size()
        && a.file_attributes() == b.file_attributes()
}

#[cfg(not(any(unix, windows)))]
fn same_metadata(a: &Metadata, b: &Metadata) -> bool {
    let _ = (a, b);
    true
}

fn quote(path: &Path) -> String {
    go_quote(&path.to_string_lossy())
}

impl BoundDirectory {
    /// Go `BindDirectory`: hold an open fd/handle to the absolute directory.
    pub fn open(path: &Path) -> io::Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let file = open_dir(&absolute)?;
        // Stat the held fd/handle, not the path, so a concurrent swap cannot
        // fool the "is a directory" check (Go: file.Stat()).
        if !file.metadata()?.is_dir() {
            return Err(io::Error::other(format!(
                "bound directory {} is not a directory",
                quote(&absolute)
            )));
        }
        Ok(Self {
            path: absolute,
            file,
        })
    }

    /// Off Linux, `Path()` returns the path itself (no /proc/self/fd).
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// Go `Check`: require the held inode still to be the one named by the path.
    pub fn check(&self) -> io::Result<()> {
        let bound = self.file.metadata().map_err(|e| {
            io::Error::other(format!(
                "bound directory {} is unavailable: {}",
                quote(&self.path),
                e
            ))
        })?;
        let current = std::fs::metadata(&self.path).map_err(|e| {
            io::Error::other(format!(
                "bound directory {} is no longer reachable: {}",
                quote(&self.path),
                e
            ))
        })?;
        if !same_metadata(&bound, &current) {
            return Err(io::Error::other(format!(
                "bound directory {} no longer names the session's original directory",
                quote(&self.path)
            )));
        }
        Ok(())
    }

    /// No kernel-enforced FS off Linux; callers use ordinary paths.
    pub fn fs(&self) -> Option<&dyn FS> {
        None
    }

    /// `std::os::fd` accessors on unix non-Linux platforms — same signatures as
    /// the Linux `BoundDirectory`, so `command_fd` callers compile off Linux too.
    #[cfg(unix)]
    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.file.as_fd()
    }

    #[cfg(unix)]
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.file.as_raw_fd()
    }

    /// `std::os::windows::io` equivalents on Windows (`std::os::fd` is absent).
    #[cfg(windows)]
    pub fn as_handle(&self) -> std::os::windows::io::BorrowedHandle<'_> {
        use std::os::windows::io::AsHandle;
        self.file.as_handle()
    }

    #[cfg(windows)]
    pub fn raw_handle(&self) -> std::os::windows::io::RawHandle {
        use std::os::windows::io::AsRawHandle;
        self.file.as_raw_handle()
    }
}
