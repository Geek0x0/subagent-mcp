//! Linux Landlock helper (`landlock_linux.go`).
//!
//! `V10.BestEffort().RestrictPaths(...)` is reproduced with raw, stable uapi
//! syscalls so the exact per-ABI access-right set from [`crate::sandbox::plan`]
//! is enforced: read everywhere, writes only on the roots and device files,
//! `WithRefer` only on ABI >= 2. The kernel ABI is required to be >= 1 first —
//! BestEffort would otherwise silently no-op on a kernel without Landlock.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustix::fs::{CWD, Mode, OFlags, openat};
use rustix::io::Errno;

use crate::sandbox::{DEVICE_FILES, Error, RulesetPlan, err, parse_args, plan, writable_roots};

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_uint = 1;
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;

// The pure `A_*` constants in the parent module are stable uapi bit values.
// Pin them at compile time to the `landlock` crate's canonical definitions so
// a stray transcription error can never silently weaken the sandbox.
const _: () = {
    use landlock::AccessFs as A;
    assert!(A::Execute as u64 == crate::sandbox::A_EXECUTE);
    assert!(A::WriteFile as u64 == crate::sandbox::A_WRITE_FILE);
    assert!(A::ReadFile as u64 == crate::sandbox::A_READ_FILE);
    assert!(A::ReadDir as u64 == crate::sandbox::A_READ_DIR);
    assert!(A::RemoveDir as u64 == crate::sandbox::A_REMOVE_DIR);
    assert!(A::RemoveFile as u64 == crate::sandbox::A_REMOVE_FILE);
    assert!(A::MakeChar as u64 == crate::sandbox::A_MAKE_CHAR);
    assert!(A::MakeDir as u64 == crate::sandbox::A_MAKE_DIR);
    assert!(A::MakeReg as u64 == crate::sandbox::A_MAKE_REG);
    assert!(A::MakeSock as u64 == crate::sandbox::A_MAKE_SOCK);
    assert!(A::MakeFifo as u64 == crate::sandbox::A_MAKE_FIFO);
    assert!(A::MakeBlock as u64 == crate::sandbox::A_MAKE_BLOCK);
    assert!(A::MakeSym as u64 == crate::sandbox::A_MAKE_SYM);
    assert!(A::Refer as u64 == crate::sandbox::A_REFER);
    assert!(A::Truncate as u64 == crate::sandbox::A_TRUNCATE);
    assert!(A::IoctlDev as u64 == crate::sandbox::A_IOCTL_DEV);
    assert!(A::ResolveUnix as u64 == crate::sandbox::A_RESOLVE_UNIX);
};

/// Minimal `struct landlock_ruleset_attr` (ABI v1 size). The kernel zero-extends
/// missing trailing fields, so net/scoped rights stay unhandled — exactly the
/// FS-only sandbox the Go helper builds.
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

/// `struct landlock_path_beneath_attr` is packed (12 bytes) in the uapi.
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Go `LandlockGetABIVersion`: the `LANDLOCK_CREATE_RULESET_VERSION` probe.
pub fn landlock_abi() -> io::Result<i32> {
    // SAFETY: version probe passes a NULL attr and zero size; only `flags` is read.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if v < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(v as i32)
}

fn create_ruleset(handled: u64) -> io::Result<OwnedFd> {
    let attr = RulesetAttr {
        handled_access_fs: handled,
    };
    // SAFETY: `attr` outlives the syscall; size is the struct's byte length.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Add one `LANDLOCK_RULE_PATH_BENEATH` rule for `path`. `ignore_missing`
/// mirrors `RWFiles(...).IgnoreIfMissing()` for optional device files.
fn add_rule(
    ruleset_fd: RawFd,
    path: &Path,
    allowed: u64,
    ignore_missing: bool,
) -> Result<(), Error> {
    let parent_fd = match openat(
        CWD,
        path,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::from_raw_mode(0),
    ) {
        Ok(fd) => fd,
        Err(e) if ignore_missing && e == Errno::NOENT => return Ok(()),
        Err(e) => return Err(err(format!("landlock unavailable: {e}"))),
    };
    let attr = PathBeneathAttr {
        allowed_access: allowed,
        parent_fd: parent_fd.as_raw_fd(),
    };
    // SAFETY: `attr` and `parent_fd` outlive the syscall; rule_type/flags are u32.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset_fd,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0u32,
        )
    };
    drop(parent_fd);
    if rc < 0 {
        return Err(err(format!(
            "landlock unavailable: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn restrict(roots: &[PathBuf], p: &RulesetPlan) -> Result<(), Error> {
    let ruleset_fd =
        create_ruleset(p.handled).map_err(|e| err(format!("landlock unavailable: {e}")))?;
    let rfd = ruleset_fd.as_raw_fd();

    // RODirs("/"): read/execute everywhere.
    add_rule(rfd, Path::new("/"), p.readable, false)?;
    // RWDirs(roots...) [.WithRefer() when ABI >= 2].
    for root in roots {
        add_rule(rfd, root, p.writable, false)?;
    }
    // RWFiles(device_files...).IgnoreIfMissing().
    for device in DEVICE_FILES {
        add_rule(rfd, Path::new(device), p.device, true)?;
    }

    // no_new_privs is required before landlock_restrict_self.
    if unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(err(format!(
            "landlock unavailable: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: rfd is a valid ruleset fd; flags are zero.
    let rc = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, rfd, 0u32) };
    if rc < 0 {
        return Err(err(format!(
            "landlock unavailable: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Go `run`: restrict the current process and `exec` argv; returns only on error.
pub(crate) fn run(args: &[OsString]) -> Result<(), Error> {
    let parsed = parse_args(args)?;

    if parsed.cwd_fd >= 0 {
        // SAFETY: cwd_fd is a validated directory fd.
        if unsafe { libc::fchdir(parsed.cwd_fd) } != 0 {
            return Err(err(format!(
                "sandbox helper: enter bound cwd: {}",
                io::Error::last_os_error()
            )));
        }
    }

    // BestEffort would be a no-op on kernels without Landlock, so require >= 1.
    let abi = landlock_abi().map_err(|e| err(format!("landlock unavailable: {e}")))?;
    if abi < 1 {
        return Err(err("landlock unavailable: ABI < 1"));
    }

    let p = plan(abi);
    // Go: append([]string(nil), parsed.roots...) + /proc/self/fd/<fd> for --rw-fd.
    let roots = writable_roots(&parsed.roots, &parsed.root_fds);
    restrict(&roots, &p)?;

    // Close the passed directory handles before exec; the rules hold the inodes.
    for fd in &parsed.root_fds {
        // SAFETY: fd is an owned child fd being released before exec.
        unsafe { libc::close(*fd) };
    }
    if parsed.cwd_fd >= 0 {
        // SAFETY: fd is an owned child fd being released before exec.
        unsafe { libc::close(parsed.cwd_fd) };
    }

    // Go: exec.LookPath(argv[0]) + syscall.Exec. Rust `Command::exec` uses
    // execvp (PATH lookup) and inherits the (already scrubbed) environment.
    let mut cmd = Command::new(&parsed.argv[0]);
    cmd.args(&parsed.argv[1..]);
    let e = cmd.exec();
    Err(err(e.to_string()))
}
