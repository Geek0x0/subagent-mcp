//! Sandbox helper, Landlock, and `openat2(RESOLVE_BENEATH)` bound filesystem.
//!
//! Port of Go `internal/sandbox`: the `subagent-mcp` binary re-executes itself
//! in a hidden helper mode ([`HELPER_ARG`]) that restricts writes with Landlock
//! before `exec`ing the real command. A [`BoundDirectory`] holds the session's
//! directory inode so file/patch tools can operate beneath it in the kernel.
//!
//! The helper parser, argv builders, the pure ABI "plan", and the device/root
//! helpers are platform-independent. The Landlock ruleset ([`landlock_linux`])
//! and the `openat2` filesystem ([`beneath_linux`]) are Linux-only; [`other`]
//! provides a path-only fallback so the public interface compiles everywhere.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::tools::process::CommandSpec;
use crate::tools::shell::SandboxLauncher;
use crate::util::json::go_quote;

/// Hidden `argv[1]` that turns the binary into the sandbox helper.
pub const HELPER_ARG: &str = "__sandbox-exec";

/// Device files that are always writable under the sandbox. The rest of /dev
/// (including /dev/shm and block devices) stays read-only.
pub const DEVICE_FILES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/tty",
];

/// The exact device-file allowlist, for callers and tests.
pub fn device_files() -> &'static [&'static str] {
    DEVICE_FILES
}

// ---------------------------------------------------------------------------
// Landlock FS access rights (stable `linux/landlock.h` uapi bit values).
// ---------------------------------------------------------------------------

pub const A_EXECUTE: u64 = 1 << 0;
pub const A_WRITE_FILE: u64 = 1 << 1;
pub const A_READ_FILE: u64 = 1 << 2;
pub const A_READ_DIR: u64 = 1 << 3;
pub const A_REMOVE_DIR: u64 = 1 << 4;
pub const A_REMOVE_FILE: u64 = 1 << 5;
pub const A_MAKE_CHAR: u64 = 1 << 6;
pub const A_MAKE_DIR: u64 = 1 << 7;
pub const A_MAKE_REG: u64 = 1 << 8;
pub const A_MAKE_SOCK: u64 = 1 << 9;
pub const A_MAKE_FIFO: u64 = 1 << 10;
pub const A_MAKE_BLOCK: u64 = 1 << 11;
pub const A_MAKE_SYM: u64 = 1 << 12;
pub const A_REFER: u64 = 1 << 13;
pub const A_TRUNCATE: u64 = 1 << 14;
pub const A_IOCTL_DEV: u64 = 1 << 15;
pub const A_RESOLVE_UNIX: u64 = 1 << 16;

// Go `accessFSRead`, `accessFSWrite`, `accessFile` (config.go).
const FS_READ: u64 = A_EXECUTE | A_READ_FILE | A_READ_DIR;
const FS_WRITE: u64 = A_WRITE_FILE
    | A_REMOVE_DIR
    | A_REMOVE_FILE
    | A_MAKE_CHAR
    | A_MAKE_DIR
    | A_MAKE_REG
    | A_MAKE_SOCK
    | A_MAKE_FIFO
    | A_MAKE_BLOCK
    | A_MAKE_SYM
    | A_TRUNCATE;
const FS_FILE: u64 = A_EXECUTE | A_WRITE_FILE | A_READ_FILE | A_TRUNCATE;
const FS_READ_WRITE: u64 = FS_READ | FS_WRITE;

/// The pure, per-ABI Landlock ruleset plan. Computing it separately from the
/// syscalls makes ABI 1/2/3 behavior unit-testable without changing the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RulesetPlan {
    /// The ABI this plan was computed for.
    pub abi: i32,
    /// Rights the ruleset handles (deny-by-default).
    pub handled: u64,
    /// `RODirs("/")` rights.
    pub readable: u64,
    /// `RWDirs(roots)` (+ `.WithRefer()` when `refer`) rights.
    pub writable: u64,
    /// `RWFiles(device_files)` rights.
    pub device: u64,
    /// Whether `.WithRefer()` is applied (ABI >= 2).
    pub refer: bool,
}

/// Go `V10.BestEffort().RestrictPaths(...)` reduced to pure bit arithmetic:
/// handle every right supported by `abi`, grant read everywhere, full access on
/// writable roots (`WithRefer` on ABI >= 2), and file-only access on devices.
pub fn plan(abi: i32) -> RulesetPlan {
    let handled = handled_mask(abi);
    let refer = abi >= 2;
    let readable = FS_READ & handled;
    let writable = (FS_READ_WRITE & handled) | if refer { A_REFER & handled } else { 0 };
    let device = (FS_READ_WRITE & FS_FILE) & handled;
    RulesetPlan {
        abi,
        handled,
        readable,
        writable,
        device,
        refer,
    }
}

/// Go `abiInfos[v].supportedAccessFS`: the FS rights a kernel of ABI `v` knows.
fn handled_mask(abi: i32) -> u64 {
    match abi {
        a if a <= 0 => 0,
        1 => (1 << 13) - 1,
        2 => (1 << 14) - 1,
        3 | 4 => (1 << 15) - 1,
        5..=8 => (1 << 16) - 1,
        _ => (1 << 17) - 1,
    }
}

// ---------------------------------------------------------------------------
// Root/dedupe helpers (shared by the helper and the tests)
// ---------------------------------------------------------------------------

/// Deduplicate paths by their string spelling, preserving first-seen order.
pub fn dedupe_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: HashSet<OsString> = HashSet::new();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        if seen.insert(p.as_os_str().to_os_string()) {
            out.push(p.clone());
        }
    }
    out
}

/// Build the helper's writable-root list: explicit `--rw` paths plus
/// `/proc/self/fd/<fd>` for every `--rw-fd`, deduplicated.
pub fn writable_roots(roots: &[PathBuf], root_fds: &[i32]) -> Vec<PathBuf> {
    let mut all = roots.to_vec();
    for fd in root_fds {
        all.push(PathBuf::from(format!("/proc/self/fd/{fd}")));
    }
    dedupe_paths(&all)
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Go helper error strings are plain messages; this carries them verbatim.
#[derive(Debug, Clone)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub(crate) fn err(message: impl Into<String>) -> Error {
    Error(message.into())
}

// ---------------------------------------------------------------------------
// Helper argv builders + public Command constructors
// ---------------------------------------------------------------------------

fn helper_args(roots: &[PathBuf], tail: &[OsString]) -> Vec<OsString> {
    let mut out = Vec::with_capacity(2 + 2 * roots.len() + tail.len());
    out.push(OsString::from(HELPER_ARG));
    for r in roots {
        out.push(OsString::from("--rw"));
        out.push(r.as_os_str().to_os_string());
    }
    out.push(OsString::from("--"));
    out.extend_from_slice(tail);
    out
}

fn helper_args_fd(cwd_fd: i32, root_fds: &[i32], tail: &[OsString]) -> Vec<OsString> {
    let mut out = vec![
        OsString::from(HELPER_ARG),
        OsString::from("--cwd-fd"),
        OsString::from(cwd_fd.to_string()),
    ];
    for fd in root_fds {
        out.push(OsString::from("--rw-fd"));
        out.push(OsString::from(fd.to_string()));
    }
    out.push(OsString::from("--"));
    out.extend_from_slice(tail);
    out
}

/// Go `sandbox.Command`: run `argv` through the helper with `roots` writable.
pub fn command(self_exe: &Path, roots: &[PathBuf], argv: &[OsString]) -> Command {
    let mut c = Command::new(self_exe);
    c.args(helper_args(roots, argv));
    c
}

/// Go `sandbox.CommandFD`: pass a held cwd and writable directory handles.
/// Descriptor numbers refer to child-inherited fds (ExtraFiles, starting at 3).
pub fn command_fd(self_exe: &Path, cwd_fd: i32, root_fds: &[i32], argv: &[OsString]) -> Command {
    let mut c = Command::new(self_exe);
    c.args(helper_args_fd(cwd_fd, root_fds, argv));
    c
}

// ---------------------------------------------------------------------------
// SandboxLauncher
// ---------------------------------------------------------------------------

/// Rewrites a [`CommandSpec`] so the child is the Landlock helper, leaving the
/// original command as the tail. Go `RunShellSandboxed` uses the `--rw` form.
pub struct Sandbox {
    pub self_exe: PathBuf,
    pub roots: Vec<PathBuf>,
}

impl Sandbox {
    pub fn new(self_exe: PathBuf, roots: Vec<PathBuf>) -> Self {
        Self { self_exe, roots }
    }
}

impl SandboxLauncher for Sandbox {
    fn wrap(&self, spec: &mut CommandSpec) {
        let mut tail = Vec::with_capacity(1 + spec.args.len());
        tail.push(spec.program.clone());
        tail.extend(spec.args.iter().cloned());
        spec.program = self.self_exe.as_os_str().to_os_string();
        spec.args = helper_args(&self.roots, &tail);
    }
}

/// Go `RunShellSandboxedBound`: the fd-based launcher. Instead of re-resolving
/// `--rw` paths at exec time (TOCTOU), it hands the session's HELD directory
/// descriptors to the helper via `--cwd-fd` / `--rw-fd` and arranges for those
/// exact descriptors to be inherited by the child (clearing FD_CLOEXEC in
/// `pre_exec`). `command_fd` builds the helper argv; `spec.cwd` is `/` because
/// the helper `fchdir()`s into the held cwd via `--cwd-fd`.
pub struct BoundSandbox {
    pub self_exe: PathBuf,
    pub cwd_fd: i32,
    pub root_fds: Vec<i32>,
}

impl BoundSandbox {
    pub fn new(self_exe: PathBuf, cwd_fd: i32, root_fds: Vec<i32>) -> Self {
        Self {
            self_exe,
            cwd_fd,
            root_fds,
        }
    }
}

impl SandboxLauncher for BoundSandbox {
    fn wrap(&self, spec: &mut CommandSpec) {
        let mut tail = Vec::with_capacity(1 + spec.args.len());
        tail.push(spec.program.clone());
        tail.extend(spec.args.iter().cloned());
        let cmd = command_fd(&self.self_exe, self.cwd_fd, &self.root_fds, &tail);
        spec.program = cmd.get_program().to_os_string();
        spec.args = cmd.get_args().map(|a| a.to_os_string()).collect();
        spec.cwd = Some(PathBuf::from("/"));
        // Inherit the exact held descriptor numbers. The bound cwd is also a
        // writable root, so `cwd_fd` can repeat in `root_fds`; dedupe so each
        // descriptor is cleared (and later closed) exactly once.
        let mut inherited = vec![self.cwd_fd];
        inherited.extend(self.root_fds.iter().copied());
        inherited.sort_unstable();
        inherited.dedup();
        spec.inherited_fds = inherited;
    }
}

// ---------------------------------------------------------------------------
// Helper argument parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperArgs {
    pub roots: Vec<PathBuf>,
    pub root_fds: Vec<i32>,
    /// -1 when `--cwd-fd` is absent.
    pub cwd_fd: i32,
    pub argv: Vec<OsString>,
}

/// Go `parseDirectoryFD`: require a decimal fd >= 3 that names a directory.
/// `fstat` is a Unix operation; off Unix the sandbox is unsupported anyway.
pub fn parse_directory_fd(value: &OsStr) -> Result<i32, Error> {
    #[cfg(target_os = "linux")]
    {
        let Some(s) = value.to_str() else {
            return Err(err(format!(
                "sandbox helper: invalid directory fd {}",
                go_quote(&value.to_string_lossy())
            )));
        };
        let fd: i32 = match s.parse::<i32>() {
            Ok(n) if n >= 3 => n,
            _ => {
                return Err(err(format!(
                    "sandbox helper: invalid directory fd {}",
                    go_quote(s)
                )));
            }
        };
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return Err(err(format!(
                "sandbox helper: inspect directory fd {}: {}",
                fd,
                std::io::Error::last_os_error()
            )));
        }
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(err(format!("sandbox helper: fd {} is not a directory", fd)));
        }
        Ok(fd)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = value;
        Err(err("landlock unavailable: kernel sandbox requires Linux"))
    }
}

/// Go `parseArgs`. All error strings are preserved verbatim (`%q` uses
/// [`go_quote`], Go's `strconv.Quote`).
pub fn parse_args(args: &[OsString]) -> Result<HelperArgs, Error> {
    let mut parsed = HelperArgs {
        roots: Vec::new(),
        root_fds: Vec::new(),
        cwd_fd: -1,
        argv: Vec::new(),
    };
    let mut i = 0usize;
    while i < args.len() {
        match args[i].to_str() {
            Some("--rw") => {
                if i + 1 >= args.len() {
                    return Err(err("sandbox helper: --rw requires a path"));
                }
                parsed.roots.push(PathBuf::from(args[i + 1].clone()));
                i += 2;
            }
            Some(flag @ ("--rw-fd" | "--cwd-fd")) => {
                if i + 1 >= args.len() {
                    return Err(err(format!(
                        "sandbox helper: {} requires a directory fd",
                        flag
                    )));
                }
                let fd = parse_directory_fd(args[i + 1].as_os_str())?;
                if flag == "--rw-fd" {
                    parsed.root_fds.push(fd);
                } else {
                    if parsed.cwd_fd >= 0 {
                        return Err(err("sandbox helper: duplicate --cwd-fd"));
                    }
                    parsed.cwd_fd = fd;
                }
                i += 2;
            }
            Some("--") => {
                if i + 1 >= args.len() {
                    return Err(err("sandbox helper: missing command after --"));
                }
                parsed.argv = args[i + 1..].to_vec();
                return Ok(parsed);
            }
            _ => {
                return Err(err(format!(
                    "sandbox helper: unexpected argument {}",
                    go_quote(&args[i].to_string_lossy())
                )));
            }
        }
    }
    Err(err("sandbox helper: missing -- separator"))
}

// ---------------------------------------------------------------------------
// Platform dispatch: helper execution + Landlock availability
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod beneath_linux;
#[cfg(target_os = "linux")]
mod landlock_linux;
#[cfg(not(target_os = "linux"))]
mod other;

#[cfg(target_os = "linux")]
pub use beneath_linux::BoundDirectory;
#[cfg(not(target_os = "linux"))]
pub use other::BoundDirectory;

#[cfg(target_os = "linux")]
pub use landlock_linux::landlock_abi;

#[cfg(target_os = "linux")]
use landlock_linux::run as platform_run;
#[cfg(not(target_os = "linux"))]
use other::run as platform_run;

/// Whether the running kernel supports Landlock (ABI >= 1).
pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        landlock_abi().is_ok_and(|abi| abi >= 1)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Go `MaybeRunHelper`. Call first in `main`, before `cli::run` and before any
/// Tokio runtime. Returns true when argv selected the helper; the caller then
/// exits 126. On success the helper `exec`s and never returns.
pub fn maybe_run_helper() -> bool {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.len() < 2 || args[1].as_os_str() != OsStr::new(HELPER_ARG) {
        return false;
    }
    match platform_run(&args[2..]) {
        // `exec` replaces the process on success; reaching here is a failure.
        Ok(()) => true,
        Err(e) => {
            eprintln!("subagent-mcp: {e}");
            true
        }
    }
}
