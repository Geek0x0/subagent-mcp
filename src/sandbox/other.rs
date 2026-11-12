//! Non-Linux fallback (`landlock_other.go`, `directory_other.go`, `fd_windows.go`).
//!
//! There is no kernel-enforced containment off Linux: `available()` is false,
//! `run()` still validates the helper argv (preserving the exact error strings)
//! and then reports the unsupported sandbox, and `BoundDirectory` is a path-only
//! struct whose `fs()` returns `None` so callers fall back to ordinary paths.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use crate::patch::FS;
use crate::sandbox::{Error, err, parse_args};
use crate::util::json::go_quote;

pub(crate) fn run(args: &[OsString]) -> Result<(), Error> {
    let _ = parse_args(args)?;
    Err(err("landlock unavailable: kernel sandbox requires Linux"))
}

pub struct BoundDirectory {
    pub path: PathBuf,
}

impl BoundDirectory {
    pub fn open(path: &Path) -> io::Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let info = std::fs::metadata(&absolute)?;
        if !info.is_dir() {
            return Err(io::Error::other(format!(
                "bound directory {} is not a directory",
                go_quote(&absolute.to_string_lossy())
            )));
        }
        Ok(Self { path: absolute })
    }

    /// Off Linux, `Path()` returns the path itself (no held fd).
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn check(&self) -> io::Result<()> {
        std::fs::metadata(&self.path).map_err(|e| {
            io::Error::other(format!(
                "bound directory {} is no longer reachable: {}",
                go_quote(&self.path.to_string_lossy()),
                e
            ))
        })?;
        Ok(())
    }

    /// No kernel-enforced FS off Linux; callers use ordinary paths.
    pub fn fs(&self) -> Option<&dyn FS> {
        None
    }
}
