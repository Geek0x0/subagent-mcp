#![cfg(target_os = "linux")]

//! Linux sandbox tests (Task 20): the Landlock helper argv/parser, the
//! `openat2(RESOLVE_BENEATH)` bound filesystem, and the `main` helper dispatch.
//!
//! Ported from `internal/sandbox/sandbox_test.go` and
//! `internal/sandbox/beneath_linux_test.go`. The helper-restricts-writes cases
//! re-exec the real `subagent-mcp` binary (whose `main` dispatches
//! `sandbox::maybe_run_helper()`); on kernels without Landlock they skip, just
//! as the Go suite does via `testutil.LandlockUnavailable`.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use subagent_mcp::patch::FS;
use subagent_mcp::sandbox::BoundDirectory;
use subagent_mcp::sandbox::{
    A_IOCTL_DEV, A_REFER, A_RESOLVE_UNIX, A_TRUNCATE, DEVICE_FILES, HELPER_ARG, HelperArgs,
    RulesetPlan, Sandbox, available, command, command_fd, dedupe_paths, device_files, landlock_abi,
    parse_args, parse_directory_fd, plan, writable_roots,
};
use subagent_mcp::tools::process::CommandSpec;
use subagent_mcp::tools::shell::SandboxLauncher;

fn os(v: &[&str]) -> Vec<OsString> {
    v.iter().map(|s| OsString::from(*s)).collect()
}

fn exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_subagent-mcp"))
}

// ---------------------------------------------------------------------------
// Helper argv forms (Go TestCommand / TestCommandFD)
// ---------------------------------------------------------------------------

#[test]
fn command_builds_go_argv() {
    let cmd = command(
        Path::new("/bin/subagent-mcp"),
        &[PathBuf::from("/a"), PathBuf::from("/b")],
        &os(&["bash", "-lc", "echo hi"]),
    );
    assert_eq!(cmd.get_program(), OsStr::new("/bin/subagent-mcp"));
    let want: Vec<OsString> = os(&[
        HELPER_ARG, "--rw", "/a", "--rw", "/b", "--", "bash", "-lc", "echo hi",
    ]);
    assert_eq!(
        cmd.get_args().collect::<Vec<_>>(),
        want.iter().collect::<Vec<_>>()
    );
}

#[test]
fn command_fd_builds_go_argv() {
    let cmd = command_fd(
        Path::new("/bin/subagent-mcp"),
        3,
        &[4, 5],
        &os(&["bash", "-c", "pwd"]),
    );
    assert_eq!(cmd.get_program(), OsStr::new("/bin/subagent-mcp"));
    let want: Vec<OsString> = os(&[
        HELPER_ARG, "--cwd-fd", "3", "--rw-fd", "4", "--rw-fd", "5", "--", "bash", "-c", "pwd",
    ]);
    assert_eq!(
        cmd.get_args().collect::<Vec<_>>(),
        want.iter().collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Helper parser: exact Go error strings (Go TestParseArgsErrors)
// ---------------------------------------------------------------------------

#[test]
fn parse_args_error_strings() {
    let cases: &[(&[&str], &str)] = &[
        (&[], "sandbox helper: missing -- separator"),
        (&["--rw"], "sandbox helper: --rw requires a path"),
        (&["--rw", "/a"], "sandbox helper: missing -- separator"),
        (&["--"], "sandbox helper: missing command after --"),
        (
            &["--rw-fd"],
            "sandbox helper: --rw-fd requires a directory fd",
        ),
        (
            &["--cwd-fd"],
            "sandbox helper: --cwd-fd requires a directory fd",
        ),
        (
            &["--bogus", "--", "true"],
            "sandbox helper: unexpected argument \"--bogus\"",
        ),
    ];
    for (args, want) in cases {
        let got = parse_args(&os(args)).unwrap_err().to_string();
        assert_eq!(got, *want, "args={args:?}");
    }
}

#[test]
fn parse_args_fds_valid_and_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let dir_file = File::open(dir.path()).unwrap();
    let dir_fd = dir_file.as_raw_fd();
    let file = tempfile::NamedTempFile::new().unwrap();
    let file_fd = file.as_raw_fd();
    let dir_fd_s = dir_fd.to_string();
    let file_fd_s = file_fd.to_string();

    // Valid: one cwd fd, one rw fd (same dir fd), one --rw path, and a command.
    let parsed: HelperArgs = parse_args(&os(&[
        "--cwd-fd", &dir_fd_s, "--rw-fd", &dir_fd_s, "--rw", "/path", "--", "true",
    ]))
    .unwrap();
    assert_eq!(parsed.cwd_fd, dir_fd);
    assert_eq!(parsed.root_fds, vec![dir_fd]);
    assert_eq!(parsed.roots, vec![PathBuf::from("/path")]);
    assert_eq!(parsed.argv, os(&["true"]));

    // Invalid fd values: exact error strings where the message is deterministic.
    assert_eq!(
        parse_args(&os(&["--cwd-fd", "bad", "--", "true"]))
            .unwrap_err()
            .to_string(),
        "sandbox helper: invalid directory fd \"bad\""
    );
    assert_eq!(
        parse_args(&os(&["--cwd-fd", "2", "--", "true"]))
            .unwrap_err()
            .to_string(),
        "sandbox helper: invalid directory fd \"2\""
    );
    assert_eq!(
        parse_args(&os(&["--cwd-fd", &file_fd_s, "--", "true"]))
            .unwrap_err()
            .to_string(),
        format!("sandbox helper: fd {} is not a directory", file_fd)
    );
    assert_eq!(
        parse_args(&os(&["--rw-fd", &file_fd_s, "--", "true"]))
            .unwrap_err()
            .to_string(),
        format!("sandbox helper: fd {} is not a directory", file_fd)
    );
    assert_eq!(
        parse_args(&os(&[
            "--cwd-fd", &dir_fd_s, "--cwd-fd", &dir_fd_s, "--", "true"
        ]))
        .unwrap_err()
        .to_string(),
        "sandbox helper: duplicate --cwd-fd"
    );
    assert!(
        parse_args(&os(&["--rw-fd", "999999", "--", "true"]))
            .unwrap_err()
            .to_string()
            .starts_with("sandbox helper: inspect directory fd 999999: ")
    );
    // A non-existent fd fails the inspect step.
    assert!(
        parse_directory_fd(OsStr::new("999999"))
            .unwrap_err()
            .to_string()
            .starts_with("sandbox helper: inspect directory fd 999999: ")
    );
}

// ---------------------------------------------------------------------------
// Sandbox::wrap rewrites CommandSpec, leaving the original command as the tail
// ---------------------------------------------------------------------------

#[test]
fn sandbox_wrap_rewrites_spec() {
    let sandbox = Sandbox::new(
        PathBuf::from("/bin/subagent-mcp"),
        vec![PathBuf::from("/a"), PathBuf::from("/b")],
    );
    let mut spec = CommandSpec {
        program: OsString::from("bash"),
        args: os(&["-lc", "echo hi"]),
        cwd: Some(PathBuf::from("/work")),
        env: vec![(OsString::from("K"), OsString::from("V"))],
        stdout: Stdio::piped(),
        stderr: Stdio::piped(),
        inherited_fds: Vec::new(),
    };
    sandbox.wrap(&mut spec);
    assert_eq!(spec.program, OsString::from("/bin/subagent-mcp"));
    let want: Vec<OsString> = os(&[
        HELPER_ARG, "--rw", "/a", "--rw", "/b", "--", "bash", "-lc", "echo hi",
    ]);
    assert_eq!(spec.args, want);
    // wrap only rewrites program/args; the rest of the spec is untouched.
    assert_eq!(spec.cwd, Some(PathBuf::from("/work")));
    assert_eq!(spec.env, vec![(OsString::from("K"), OsString::from("V"))]);
}

// ---------------------------------------------------------------------------
// Pure ABI 1/2/3 landlock plan (portable, no kernel needed)
// ---------------------------------------------------------------------------

#[test]
fn plan_abi_1_no_refer_no_truncate() {
    let p: RulesetPlan = plan(1);
    assert_eq!(p.abi, 1);
    assert!(!p.refer);
    assert_eq!(p.handled, (1u64 << 13) - 1); // v1: 13 rights
    assert_eq!(p.writable & A_REFER, 0);
    assert_eq!(p.writable & A_TRUNCATE, 0);
    assert_eq!(p.device & A_TRUNCATE, 0);
    assert_eq!(p.device & A_IOCTL_DEV, 0);
    // RWFiles grants no directory-only rights.
    assert_eq!(p.device & subagent_mcp::sandbox::A_READ_DIR, 0);
    assert_eq!(p.handled & A_RESOLVE_UNIX, 0);
}

#[test]
fn plan_abi_2_grants_refer() {
    let p = plan(2);
    assert!(p.refer);
    assert_eq!(p.handled, (1u64 << 14) - 1);
    assert_eq!(p.writable & A_REFER, A_REFER);
    assert_eq!(p.handled & A_REFER, A_REFER);
    assert_eq!(p.writable & A_TRUNCATE, 0);
    assert_eq!(p.handled & A_TRUNCATE, 0);
}

#[test]
fn plan_abi_3_grants_truncate() {
    let p = plan(3);
    assert!(p.refer);
    assert_eq!(p.handled, (1u64 << 15) - 1);
    assert_eq!(p.handled & A_TRUNCATE, A_TRUNCATE);
    assert_eq!(p.writable & A_TRUNCATE, A_TRUNCATE);
    assert_eq!(p.device & A_TRUNCATE, A_TRUNCATE);
    assert_eq!(p.handled & A_IOCTL_DEV, 0);
    assert_eq!(p.handled & A_RESOLVE_UNIX, 0);
}

#[test]
fn plan_abi_8_and_9_masks() {
    let p8 = plan(8);
    assert_eq!(p8.handled, (1u64 << 16) - 1); // up to ioctl_dev
    assert_eq!(p8.handled & A_IOCTL_DEV, A_IOCTL_DEV);
    assert_eq!(p8.handled & A_RESOLVE_UNIX, 0);
    // ioctl_dev is handled but never granted by any rule (matches go-landlock).
    assert_eq!(p8.writable & A_IOCTL_DEV, 0);
    assert_eq!(p8.device & A_IOCTL_DEV, 0);
    let p9 = plan(9);
    assert_eq!(p9.handled, (1u64 << 17) - 1);
    assert_eq!(p9.handled & A_RESOLVE_UNIX, A_RESOLVE_UNIX);
    assert_eq!(p9.writable & A_RESOLVE_UNIX, 0);
}

#[test]
fn plan_abi_0_is_unavailable() {
    let p = plan(0);
    assert!(!p.refer);
    assert_eq!(p.handled, 0);
    assert_eq!(p.writable, 0);
    assert_eq!(p.readable, 0);
    assert_eq!(p.device, 0);
}

// ---------------------------------------------------------------------------
// Device files and (path) dedupe helpers
// ---------------------------------------------------------------------------

#[test]
fn device_files_exact() {
    assert_eq!(
        DEVICE_FILES,
        &[
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/random",
            "/dev/urandom",
            "/dev/tty"
        ]
    );
    assert_eq!(device_files(), DEVICE_FILES);
}

#[test]
fn dedupe_paths_and_writable_roots() {
    assert_eq!(
        dedupe_paths(&[
            PathBuf::from("/a"),
            PathBuf::from("/a"),
            PathBuf::from("/b"),
        ]),
        vec![PathBuf::from("/a"), PathBuf::from("/b")]
    );
    // Explicit roots plus /proc/self/fd/<fd>, deduped by path.
    assert_eq!(
        writable_roots(
            &[
                PathBuf::from("/a"),
                PathBuf::from("/a"),
                PathBuf::from("/b")
            ],
            &[3, 4]
        ),
        vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/proc/self/fd/3"),
            PathBuf::from("/proc/self/fd/4"),
        ]
    );
    // Duplicate fd numbers collapse to one /proc/self/fd path.
    assert_eq!(
        writable_roots(&[], &[3, 3]),
        vec![PathBuf::from("/proc/self/fd/3")]
    );
    // An empty root set stays empty (read-only mode).
    assert!(writable_roots(&[], &[]).is_empty());
}

/// Task 20's root helpers dedupe by raw string spelling only — no cleaning,
/// symlink resolution, or `os.SameFile` inode dedup (that is Task 22's job).
/// This pins the Go-faithful string-spelling + `/proc/self/fd` behavior; the
/// real cwd//tmp/$TMPDIR/configured default root set and session-root inode
/// dedup are deliberately NOT built or tested here (Task 22 handoff).
#[test]
fn writable_roots_dedupes_by_string_spelling_and_fd_paths() {
    // Different spellings of one place are distinct entries: dedupe is a
    // HashSet over the raw OsString, not over inodes or cleaned paths.
    let given = vec![
        PathBuf::from("/a"),
        PathBuf::from("/a/"), // different spelling -> kept
        PathBuf::from("/a"),  // exact duplicate -> dropped
        PathBuf::from("/b"),
    ];
    let spellings: Vec<String> = dedupe_paths(&given)
        .iter()
        .map(|p| p.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert_eq!(spellings, vec!["/a", "/a/", "/b"]);

    // writable_roots = explicit roots (first-seen, deduped) then one
    // /proc/self/fd/<fd> per fd number, in one final dedupe pass.
    let roots = writable_roots(
        &[
            PathBuf::from("/x"),
            PathBuf::from("/x"),
            PathBuf::from("/proc/self/fd/3"),
        ],
        &[3, 4],
    );
    assert_eq!(
        roots,
        vec![
            PathBuf::from("/x"),
            PathBuf::from("/proc/self/fd/3"),
            PathBuf::from("/proc/self/fd/4"),
        ]
    );

    // An fd-derived root spelled exactly like an explicit root collapses.
    assert_eq!(
        writable_roots(&[PathBuf::from("/proc/self/fd/5")], &[5]),
        vec![PathBuf::from("/proc/self/fd/5")]
    );
    // Empty roots + no fds => read-only mode (no writable roots).
    assert!(writable_roots(&[], &[]).is_empty());
}

#[test]
fn landlock_abi_is_consistent_with_available() {
    match landlock_abi() {
        Ok(abi) => {
            assert!(abi >= 1);
            assert!(available());
        }
        Err(_) => assert!(!available()),
    }
}

// ---------------------------------------------------------------------------
// Binary helper: main dispatches the helper before cli/tokio, fail-closed 126
// ---------------------------------------------------------------------------

#[test]
fn binary_helper_bad_args_exit_126() {
    let out = std::process::Command::new(exe())
        .arg(HELPER_ARG)
        .arg("--bad")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(126), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("subagent-mcp:"), "stderr = {:?}", stderr);
    assert!(
        stderr.contains("sandbox helper: unexpected argument"),
        "stderr = {:?}",
        stderr
    );
}

// ---------------------------------------------------------------------------
// Real helper (skipped when the kernel has no Landlock)
// ---------------------------------------------------------------------------

fn run_helper(roots: &[PathBuf], argv: &[&str]) -> std::process::Output {
    command(exe(), roots, &os(argv)).output().unwrap()
}

#[test]
fn helper_restricts_writes() {
    if !available() {
        eprintln!("skip: landlock unavailable");
        return;
    }
    let inside = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let inside_s = inside.path().to_string_lossy().into_owned();
    let outside_s = outside.path().to_string_lossy().into_owned();

    let ok_script = format!(
        "touch {inside_s}/ok && echo x > /dev/null && echo y > /dev/stdout && cat /etc/passwd > /dev/null"
    );
    let out = run_helper(&[inside.path().to_path_buf()], &["bash", "-c", &ok_script]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(inside.path().join("ok").exists());

    let bad_script = format!("touch {outside_s}/bad");
    let out = run_helper(&[inside.path().to_path_buf()], &["bash", "-c", &bad_script]);
    assert_ne!(out.status.code(), Some(0), "{out:?}");
    assert!(!outside.path().join("bad").exists(), "outside file created");
}

#[test]
fn helper_allows_cross_directory_link_inside_root() {
    if !available() {
        eprintln!("skip: landlock unavailable");
        return;
    }
    let inside = tempfile::tempdir().unwrap();
    let script = format!(
        "cd {} && mkdir a b && touch a/x && ln a/x b/x",
        inside.path().to_string_lossy()
    );
    let out = run_helper(&[inside.path().to_path_buf()], &["bash", "-c", &script]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn helper_denies_device_directory_writes() {
    if !available() {
        eprintln!("skip: landlock unavailable");
        return;
    }
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.is_dir()) {
        eprintln!("skip: /dev/shm unavailable");
        return;
    }
    let probe =
        PathBuf::from("/dev/shm").join(format!("subagent-mcp-probe-{}", std::process::id()));
    let _ = fs::remove_file(&probe);
    let script = format!("touch {}", probe.display());
    let out = run_helper(&[], &["bash", "-c", &script]);
    let _ = fs::remove_file(&probe);
    assert_ne!(out.status.code(), Some(0), "{out:?}");
}

#[test]
fn helper_empty_roots_is_read_only() {
    if !available() {
        eprintln!("skip: landlock unavailable");
        return;
    }
    let outside = tempfile::tempdir().unwrap();
    // No --rw roots: writes anywhere on the filesystem are denied.
    let bad_script = format!("touch {}/bad", outside.path().to_string_lossy());
    let out = run_helper(&[], &["bash", "-c", &bad_script]);
    assert_ne!(out.status.code(), Some(0), "{out:?}");
    assert!(!outside.path().join("bad").exists());
    // Reads and exec, and device writes, still work.
    let ok = run_helper(
        &[],
        &[
            "bash",
            "-c",
            "cat /etc/passwd > /dev/null && echo z > /dev/null",
        ],
    );
    assert_eq!(ok.status.code(), Some(0), "{ok:?}");
}

#[test]
fn helper_workspace_write_default_root_set() {
    if !available() {
        eprintln!("skip: landlock unavailable");
        return;
    }
    // Task 22 computes the actual default root set; here we verify the helper
    // honors a representative set (cwd + configured roots) and denies outside.
    let cwd = tempfile::tempdir().unwrap();
    let configured = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    // The two writes inside the root set are allowed; a write outside is denied.
    let allowed = format!(
        "touch {}/in_cwd && touch {}/in_configured",
        cwd.path().to_string_lossy(),
        configured.path().to_string_lossy(),
    );
    let out = run_helper(
        &[cwd.path().to_path_buf(), configured.path().to_path_buf()],
        &["bash", "-c", &allowed],
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(cwd.path().join("in_cwd").exists());
    assert!(configured.path().join("in_configured").exists());

    let denied = run_helper(
        &[cwd.path().to_path_buf(), configured.path().to_path_buf()],
        &[
            "bash",
            "-c",
            &format!("touch {}/outside", outside.path().to_string_lossy()),
        ],
    );
    assert_ne!(denied.status.code(), Some(0), "{denied:?}");
    assert!(!outside.path().join("outside").exists());
}

// ---------------------------------------------------------------------------
// BoundDirectory / openat2(RESOLVE_BENEATH) FS (Go beneath_linux_test.go)
// ---------------------------------------------------------------------------

#[test]
fn bound_directory_operations() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    fs::create_dir(&root).unwrap();
    let dir = BoundDirectory::open(&root).unwrap();
    let fs: &dyn FS = dir.fs().unwrap();
    let p = |rel: &str| dir.path().join(rel);

    fs.write(&p("a/b/c.txt"), b"hello", 0o644).unwrap();
    assert_eq!(fs::read(root.join("a/b/c.txt")).unwrap(), b"hello");
    assert_eq!(fs.read(&p("a/b/c.txt")).unwrap(), b"hello");
    assert_eq!(fs.lstat(&p("a/b/c.txt")).unwrap().len(), 5);
    assert_eq!(
        fs.stat(&p("missing")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );

    fs.symlink(Path::new("c.txt"), &p("a/b/link")).unwrap();
    assert_eq!(fs.readlink(&p("a/b/link")).unwrap(), PathBuf::from("c.txt"));
    assert_eq!(fs.read(&p("a/b/link")).unwrap(), b"hello");

    fs.chmod(&p("a/b/c.txt"), 0o600).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(root.join("a/b/c.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    fs.rename(&p("a/b/c.txt"), &p("a/moved.txt")).unwrap();
    fs.remove(&p("a/moved.txt")).unwrap();
    assert_eq!(
        fs.remove(&p("a/moved.txt")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn bound_directory_refuses_to_leave_the_directory() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    let outside = base.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"s").unwrap();
    symlink(&outside, root.join("abs")).unwrap();
    symlink("../outside", root.join("rel")).unwrap();

    let dir = BoundDirectory::open(&root).unwrap();
    let fs: &dyn FS = dir.fs().unwrap();
    let p = |rel: &str| dir.path().join(rel);

    for rel in ["abs/new.txt", "rel/new.txt", "abs/secret", "rel/secret"] {
        assert!(
            fs.write(&p(rel), b"x", 0o644).is_err(),
            "write({rel}) escaped"
        );
        assert!(fs.read(&p(rel)).is_err(), "read({rel}) escaped");
    }
    assert!(!outside.join("new.txt").exists(), "file appeared outside");
    assert!(fs.remove(&p("abs/secret")).is_err(), "removed outside file");
    assert!(outside.join("secret").exists(), "outside secret vanished");
    assert!(
        fs.write(&outside.join("direct.txt"), b"x", 0o644).is_err(),
        "accepted a path outside the bound root"
    );
}

#[test]
fn bound_directory_check_inode_identity_and_path() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("root");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("marker"), b"original").unwrap();

    let dir = BoundDirectory::open(&root).unwrap();
    assert!(dir.check().is_ok());
    // path() reaches the held inode through /proc/self/fd/<fd>.
    assert_eq!(
        fs::read(dir.path().join("marker")).unwrap(),
        b"original",
        "path() does not reach the held inode"
    );

    // Replace the directory at the original name with a brand-new inode.
    let saved = base.path().join("saved");
    fs::rename(&root, &saved).unwrap();
    fs::create_dir(&root).unwrap();
    let err = dir.check().unwrap_err().to_string();
    assert!(
        err.contains("no longer names the session's original directory"),
        "err = {err}"
    );
    // The held fd still reaches the original inode (now at `saved`).
    assert_eq!(fs::read(dir.path().join("marker")).unwrap(), b"original");
}
