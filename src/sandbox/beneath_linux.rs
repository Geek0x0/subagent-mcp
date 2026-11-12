//! `BoundDirectory` + the `openat2(RESOLVE_BENEATH)` filesystem (beneath_linux.go).
//!
//! Every access is a single `openat2`/`*at` call relative to the held directory
//! fd with `RESOLVE_BENEATH`, so the kernel refuses any path that would leave
//! the directory — even if a concurrent process swaps an intermediate component
//! for a symlink between the caller's check and the use. In-tree symlinks keep
//! working.

use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, MAIN_SEPARATOR_STR, Path, PathBuf};

use rustix::fs::{
    AtFlags, CWD, Mode, OFlags, ResolveFlags, chmodat, mkdirat, openat, openat2, readlinkat,
    renameat, symlinkat, unlinkat,
};
use rustix::io::{Errno, dup};

use crate::patch::FS;
use crate::util::json::go_quote;

/// Holds the directory inode selected when a session is created.
pub struct BoundDirectory {
    /// Absolute bound path (Go `d.path`), used by [`BoundDirectory::check`].
    pub path: PathBuf,
    /// The held `O_PATH | O_DIRECTORY | O_CLOEXEC` root fd.
    pub fd: OwnedFd,
}

fn quote(path: &Path) -> String {
    go_quote(&path.to_string_lossy())
}

fn io_err(message: String) -> io::Error {
    io::Error::other(message)
}

/// Go `filepath.Clean`, lexically.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn clean_abs(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(lexical_clean(&absolute))
}

impl BoundDirectory {
    /// Go `BindDirectory`: bind the absolute path's directory inode with
    /// `open(path, O_PATH | O_DIRECTORY | O_CLOEXEC)`.
    pub fn open(path: &Path) -> io::Result<Self> {
        let absolute = clean_abs(path)?;
        let fd = openat(
            CWD,
            &absolute,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::from_raw_mode(0),
        )?;
        Ok(Self { path: absolute, fd })
    }

    /// The held fd as a `BorrowedFd` (for Task 23B's `command_fd` plumbing).
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Go `Path()`: reach the held inode even when its original name is gone.
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.fd.as_raw_fd()))
    }

    /// Go `Check`: the original path must still name the bound inode.
    pub fn check(&self) -> io::Result<()> {
        let bound = File::from(dup(&self.fd)?).metadata().map_err(|e| {
            io_err(format!(
                "bound directory {} is unavailable: {}",
                quote(&self.path),
                e
            ))
        })?;
        let current = std::fs::metadata(&self.path).map_err(|e| {
            io_err(format!(
                "bound directory {} is no longer reachable: {}",
                quote(&self.path),
                e
            ))
        })?;
        if bound.dev() != current.dev() || bound.ino() != current.ino() {
            return Err(io_err(format!(
                "bound directory {} no longer names the session's original directory",
                quote(&self.path)
            )));
        }
        Ok(())
    }

    /// Go `FS()`: the kernel-enforced `Beneath` filesystem (Linux only).
    pub fn fs(&self) -> Option<&dyn FS> {
        Some(self)
    }

    fn root_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Turn a path under `path()` into a relative path beneath the held fd.
    fn rel(&self, name: &Path) -> io::Result<PathBuf> {
        let root = self.path();
        let clean = lexical_clean(name);
        if clean == root {
            return Ok(PathBuf::from("."));
        }
        match clean.strip_prefix(&root) {
            Ok(rest) if !rest.as_os_str().is_empty() => Ok(rest.to_path_buf()),
            _ => Err(io::Error::from_raw_os_error(libc::EXDEV)),
        }
    }

    /// `openat2(root, rel, flags | O_CLOEXEC, mode, RESOLVE_BENEATH)`.
    fn open_rel(&self, rel: &Path, flags: OFlags, mode: u32) -> io::Result<OwnedFd> {
        let fd = openat2(
            self.root_fd(),
            rel,
            flags | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode),
            ResolveFlags::BENEATH,
        )?;
        Ok(fd)
    }

    /// Open the directory holding `rel`, returning it with the final name.
    fn parent(&self, rel: &Path) -> io::Result<(OwnedFd, OsString)> {
        let base = rel
            .file_name()
            .map(|s| s.to_os_string())
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let dir = rel.parent().unwrap_or(Path::new("."));
        let dir = if dir.as_os_str().is_empty() || dir == Path::new(".") {
            Path::new(".")
        } else {
            dir
        };
        let fd = if dir == Path::new(".") {
            dup(&self.fd)?
        } else {
            self.open_rel(dir, OFlags::PATH | OFlags::DIRECTORY, 0)?
        };
        Ok((fd, base))
    }

    fn mkdir_all_rel(&self, rel: &Path, mode: u32) -> io::Result<()> {
        if rel == Path::new(".") || rel.as_os_str().is_empty() {
            return Ok(());
        }
        let mut prefix = PathBuf::new();
        for part in rel.components() {
            prefix.push(part.as_os_str());
            match self.open_rel(&prefix, OFlags::PATH | OFlags::DIRECTORY, 0) {
                Ok(fd) => {
                    drop(fd);
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let (parent_fd, base) = self.parent(&prefix)?;
            match mkdirat(parent_fd.as_fd(), &base, Mode::from_raw_mode(mode)) {
                Ok(()) => {}
                Err(Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
            drop(parent_fd);
        }
        Ok(())
    }
}

impl FS for BoundDirectory {
    fn stat(&self, path: &Path) -> io::Result<Metadata> {
        let rel = self.rel(path)?;
        let fd = self.open_rel(&rel, OFlags::PATH, 0)?;
        File::from(fd).metadata()
    }

    fn lstat(&self, path: &Path) -> io::Result<Metadata> {
        let rel = self.rel(path)?;
        let fd = self.open_rel(&rel, OFlags::PATH | OFlags::NOFOLLOW, 0)?;
        File::from(fd).metadata()
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let rel = self.rel(path)?;
        let fd = self.open_rel(&rel, OFlags::RDONLY, 0)?;
        let mut file = File::from(fd);
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        Ok(data)
    }

    fn readlink(&self, path: &Path) -> io::Result<PathBuf> {
        let rel = self.rel(path)?;
        let (parent_fd, base) = self.parent(&rel)?;
        let target = readlinkat(parent_fd.as_fd(), &base, Vec::new())?;
        Ok(PathBuf::from(OsString::from_vec(target.into_bytes())))
    }

    fn mkdir_all(&self, path: &Path, mode: u32) -> io::Result<()> {
        let rel = self.rel(path)?;
        self.mkdir_all_rel(&rel, mode)
    }

    fn write(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        let rel = self.rel(path)?;
        let dir = rel.parent().unwrap_or(Path::new("."));
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        self.mkdir_all_rel(dir, 0o755)?;
        let fd = self.open_rel(&rel, OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC, mode)?;
        let mut file = File::from(fd);
        file.write_all(data)?;
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        let rel = self.rel(path)?;
        let (parent_fd, base) = self.parent(&rel)?;
        let result = unlinkat(parent_fd.as_fd(), &base, AtFlags::empty());
        match result {
            Ok(()) => Ok(()),
            Err(e @ (Errno::ISDIR | Errno::PERM)) => {
                if unlinkat(parent_fd.as_fd(), &base, AtFlags::REMOVEDIR).is_ok() {
                    Ok(())
                } else {
                    Err(e.into())
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let old_rel = self.rel(from)?;
        let new_rel = self.rel(to)?;
        let (old_parent, old_base) = self.parent(&old_rel)?;
        let (new_parent, new_base) = self.parent(&new_rel)?;
        renameat(old_parent.as_fd(), &old_base, new_parent.as_fd(), &new_base)?;
        Ok(())
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        let rel = self.rel(link)?;
        let (parent_fd, base) = self.parent(&rel)?;
        symlinkat(target, parent_fd.as_fd(), &base)?;
        Ok(())
    }

    fn chmod(&self, path: &Path, mode: u32) -> io::Result<()> {
        let rel = self.rel(path)?;
        let (parent_fd, base) = self.parent(&rel)?;
        chmodat(
            parent_fd.as_fd(),
            &base,
            Mode::from_raw_mode(mode),
            AtFlags::empty(),
        )?;
        Ok(())
    }
}
