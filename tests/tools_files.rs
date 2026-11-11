//! File tools integration tests (Task 19).
//!
//! Port of the Go file cases in `internal/tools/tools_test.go` and
//! `internal/tools/procfile_test.go`, plus the workspace-write bound-write
//! case. The Go baseline keeps the scrubbed-env and protected-file tables in
//! process-global state (and the relative-protected case changes the process
//! working directory), so every test takes `SERIAL` and `serial()` clears the
//! protected table up front.
//!
//! `/proc`-specific cases are gated to Linux (that is where `/proc/self/fd`
//! exists); the symlink case is Unix-only.

use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use subagent_mcp::patch::{FS, OS};
use subagent_mcp::tools::files::{
    read_file_range, set_protected_files, set_scrubbed_env, write_file,
};
use subagent_mcp::tools::shell::{scrubbed_env, shell_argv};
use subagent_mcp::util::run::RunContext;
use tokio_util::sync::CancellationToken;

/// Go `MaxOutputBytes` (independent golden, copied verbatim).
const MAX: usize = 16 * 1024;

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Acquire the global lock and start from an empty protected-file table.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().await;
    set_protected_files(Vec::new());
    guard
}

fn ctx() -> RunContext {
    RunContext {
        cancel: CancellationToken::new(),
        deadline: None,
    }
}

async fn read(cwd: &Path, path: &str, offset: i64, limit: i64) -> Result<String, String> {
    read_file_range(&ctx(), cwd, path, offset, limit)
        .await
        .map_err(|err| err.message)
}

fn write_fixture(path: &Path, data: &[u8]) {
    fs::write(path, data).unwrap();
}

// ---------------------------------------------------------------------------
// ReadFile / ReadFileRange behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_file_resolves_relative_path() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("example.txt"), b"hello");

    let got = read(dir.path(), "example.txt", 0, 0).await.unwrap();
    assert_eq!(got, "hello");
}

#[tokio::test]
async fn read_file_truncates_large_file() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let size = 20000usize;
    write_fixture(&dir.path().join("large.txt"), &vec![b'a'; size]);

    let got = read(dir.path(), "large.txt", 0, 0).await.unwrap();
    assert!(
        got.len() > MAX && got.len() < MAX + 100,
        "len = {}",
        got.len()
    );
    assert!(got.ends_with("[content truncated: 20000 bytes total; continue with offset=2]"));
}

#[tokio::test]
async fn read_file_bounds_sparse_large_file() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sparse-large.txt");
    let file = fs::File::create(&path).unwrap();
    file.set_len(256 * 1024 * 1024).unwrap();
    drop(file);

    let start = Instant::now();
    let got = read(dir.path(), "sparse-large.txt", 0, 0).await.unwrap();
    let elapsed = start.elapsed();

    assert!(got.ends_with("[content truncated: 268435456 bytes total; continue with offset=2]"));
    assert!(elapsed < Duration::from_secs(1), "elapsed = {:?}", elapsed);
}

#[tokio::test]
async fn read_file_returns_error_for_missing_file() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();

    let err = read(dir.path(), "missing.txt", 0, 0).await.unwrap_err();
    assert!(!err.is_empty(), "IO failure must carry a message");
}

const LINES: &str = "l1\nl2\nl3\nl4\nl5\n";

#[tokio::test]
async fn read_file_range_cases() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("lines.txt"), LINES.as_bytes());

    let cases: &[(i64, i64, &str)] = &[
        (
            2,
            2,
            "l2\nl3\n\n[more lines follow; continue with offset=4]",
        ),
        (4, 0, "l4\nl5\n"),
        (0, 0, "l1\nl2\nl3\nl4\nl5\n"),
        (9, 0, "[offset 9 is past the end of the file (5 lines)]"),
        (6, 0, "[offset 6 is past the end of the file (5 lines)]"),
        (1, 5, "l1\nl2\nl3\nl4\nl5\n"),
    ];
    for &(offset, limit, want) in cases {
        let got = read(dir.path(), "lines.txt", offset, limit).await.unwrap();
        assert_eq!(got, want, "offset={} limit={}", offset, limit);
    }
}

#[tokio::test]
async fn read_file_range_normalizes_offset_and_limit() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("lines.txt"), LINES.as_bytes());

    // offset < 1 becomes 1; non-positive limit is unlimited.
    for &(offset, limit) in &[(0i64, 0i64), (-5, 0), (-1, -3), (1, 0), (1, -9)] {
        let got = read(dir.path(), "lines.txt", offset, limit).await.unwrap();
        assert_eq!(got, LINES, "offset={} limit={}", offset, limit);
    }
}

#[tokio::test]
async fn read_file_range_byte_cap_keeps_whole_lines() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let mut content = String::new();
    for i in 1..=3000 {
        content.push_str(&format!("{:09}\n", i));
    }
    write_fixture(&dir.path().join("many-lines.txt"), content.as_bytes());

    let got = read(dir.path(), "many-lines.txt", 1, 0).await.unwrap();
    let marker = "\n[content truncated: 30000 bytes total; continue with offset=1639]";
    assert!(
        got.ends_with(marker),
        "result = {:?}",
        &got[got.len().saturating_sub(80)..]
    );
    let head = &got[..got.len() - marker.len()];
    assert!(head.len() <= MAX);
    assert!(head.ends_with('\n'), "content does not end in a whole line");
    assert_eq!(head.len(), 1638 * 10);

    let next = read(dir.path(), "many-lines.txt", 1639, 0).await.unwrap();
    assert!(next.starts_with(&format!("{:09}\n", 1639)));
}

#[tokio::test]
async fn read_file_range_single_huge_line() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("huge-line.txt"), &vec![b'a'; 20000]);

    let got = read(dir.path(), "huge-line.txt", 1, 0).await.unwrap();
    let marker = "\n[content truncated: 20000 bytes total; continue with offset=2]";
    assert!(got.ends_with(marker));
    let line = &got[..got.len() - marker.len()];
    assert_eq!(line.len(), MAX);
    assert_eq!(line, "a".repeat(MAX));
}

// ---------------------------------------------------------------------------
// Protected files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_file_preserves_path_spelling_in_refusal() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let protected = dir.path().join("config.toml");
    write_fixture(&protected, b"x");
    set_protected_files(vec![protected.clone()]);

    let err = read(dir.path(), "config.toml", 0, 0).await.unwrap_err();
    assert_eq!(
        err,
        format!(
            "refusing to read a protected subagent-mcp file: {}",
            protected.display()
        )
    );
}

#[cfg(unix)]
#[tokio::test]
async fn read_file_refuses_protected_files() {
    use std::os::unix::fs::symlink;
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let protected = dir.path().join("config.toml");
    write_fixture(&protected, b"x");
    set_protected_files(vec![protected.clone()]);

    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("link.toml");
    symlink(&protected, &link).unwrap();

    for path in [&protected, &link] {
        let err = read(dir.path(), path.to_str().unwrap(), 0, 0)
            .await
            .unwrap_err();
        assert!(err.contains("refusing to read"), "path {:?}: {}", path, err);
    }
    // Relative spelling is refused too.
    let err = read(dir.path(), "config.toml", 0, 0).await.unwrap_err();
    assert!(err.contains("refusing to read"));

    // An unrelated file is still readable.
    let other = dir.path().join("other.txt");
    write_fixture(&other, b"ok");
    assert_eq!(
        read(dir.path(), other.to_str().unwrap(), 0, 0)
            .await
            .unwrap(),
        "ok"
    );
}

struct CwdRestore(PathBuf);
impl Drop for CwdRestore {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[tokio::test]
async fn read_file_refuses_relative_protected_path() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("config.toml"), b"x");

    let original = std::env::current_dir().unwrap();
    let _restore = CwdRestore(original);
    std::env::set_current_dir(dir.path()).unwrap();

    set_protected_files(vec![PathBuf::from("config.toml")]);
    let err = read(dir.path(), "config.toml", 0, 0).await.unwrap_err();
    assert!(err.contains("refusing to read"), "{}", err);
}

// ---------------------------------------------------------------------------
// Own /proc entries
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[tokio::test]
async fn read_file_refuses_own_proc_entries() {
    let _g = serial().await;
    let pid = std::process::id().to_string();
    let paths = vec![
        "/proc/self/environ".to_string(),
        "/proc/thread-self/environ".to_string(),
        format!("/proc/{}/environ", pid),
        format!("/proc/{}/cmdline", pid),
        format!("/proc/{}/task/{}/environ", pid, pid),
        "/tmp/../proc/self/environ".to_string(),
    ];
    for path in paths {
        let err = read(Path::new("/"), &path, 0, 0).await.unwrap_err();
        assert!(err.contains("own /proc"), "path {}: {}", path, err);
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn read_file_still_reads_other_proc_entries() {
    let _g = serial().await;
    let got = read(Path::new("/"), "/proc/self/../version", 0, 0)
        .await
        .unwrap();
    assert!(!got.is_empty());
}

// ---------------------------------------------------------------------------
// Cancellation / timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_file_honors_run_deadline() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("f.txt"), b"hello");

    let ctx = RunContext {
        cancel: CancellationToken::new(),
        deadline: Some(tokio::time::Instant::now() - Duration::from_secs(1)),
    };
    let err = read_file_range(&ctx, dir.path(), "f.txt", 0, 0)
        .await
        .unwrap_err();
    assert!(err.message.contains("timed out"), "{}", err.message);
}

#[tokio::test]
async fn read_file_honors_cancellation() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("f.txt"), b"hello");

    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = RunContext {
        cancel,
        deadline: None,
    };
    let err = read_file_range(&ctx, dir.path(), "f.txt", 0, 0)
        .await
        .unwrap_err();
    assert_eq!(err.message, "context canceled");
}

// ---------------------------------------------------------------------------
// WriteFile
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_file_creates_nested_and_overwrites() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let abs = dir.path().join("a/b/c.txt");

    assert_eq!(
        write_file(dir.path(), "a/b/c.txt", "first", None)
            .await
            .unwrap(),
        5
    );
    assert_eq!(fs::read_to_string(&abs).unwrap(), "first");

    assert_eq!(
        write_file(dir.path(), "a/b/c.txt", "second", None)
            .await
            .unwrap(),
        6
    );
    assert_eq!(fs::read_to_string(&abs).unwrap(), "second");
}

#[tokio::test]
async fn write_file_returns_byte_length() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let content = "héllo"; // 6 UTF-8 bytes, matching Go len(string).
    let abs = dir.path().join("f.txt");

    let n = write_file(dir.path(), "f.txt", content, None)
        .await
        .unwrap();
    assert_eq!(n, content.len());
    assert_eq!(fs::read(&abs).unwrap(), content.as_bytes());
}

#[tokio::test]
async fn write_file_absolute_path_stays_absolute() {
    let _g = serial().await;
    let cwd = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let abs = other.path().join("file.txt");

    assert_eq!(
        write_file(cwd.path(), abs.to_str().unwrap(), "x", None)
            .await
            .unwrap(),
        1
    );
    assert_eq!(fs::read_to_string(&abs).unwrap(), "x");
    assert!(!cwd.path().join("file.txt").exists());
}

/// A `patch::FS` that records the mkdir/write calls (and the modes passed) and
/// delegates the real work to the OS backend, so the bound-write contract can
/// be asserted without depending on umask.
struct RecordingFs {
    mkdirs: Mutex<Vec<(PathBuf, u32)>>,
    writes: Mutex<Vec<(PathBuf, Vec<u8>, u32)>>,
}

impl RecordingFs {
    fn new() -> Self {
        Self {
            mkdirs: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
        }
    }
}

impl FS for RecordingFs {
    fn stat(&self, p: &Path) -> io::Result<Metadata> {
        OS.stat(p)
    }
    fn lstat(&self, p: &Path) -> io::Result<Metadata> {
        OS.lstat(p)
    }
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        OS.read(p)
    }
    fn readlink(&self, p: &Path) -> io::Result<PathBuf> {
        OS.readlink(p)
    }
    fn mkdir_all(&self, p: &Path, mode: u32) -> io::Result<()> {
        self.mkdirs.lock().unwrap().push((p.to_path_buf(), mode));
        OS.mkdir_all(p, mode)
    }
    fn write(&self, p: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        self.writes
            .lock()
            .unwrap()
            .push((p.to_path_buf(), data.to_vec(), mode));
        OS.write(p, data, mode)
    }
    fn remove(&self, p: &Path) -> io::Result<()> {
        OS.remove(p)
    }
    fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
        OS.rename(a, b)
    }
    fn symlink(&self, a: &Path, b: &Path) -> io::Result<()> {
        OS.symlink(a, b)
    }
    fn chmod(&self, p: &Path, mode: u32) -> io::Result<()> {
        OS.chmod(p, mode)
    }
}

#[tokio::test]
async fn write_file_uses_bound_fs_with_modes() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let rec = RecordingFs::new();

    let n = write_file(dir.path(), "sub/dir/file.txt", "hello", Some(&rec))
        .await
        .unwrap();
    assert_eq!(n, 5);

    let mkdirs = rec.mkdirs.lock().unwrap();
    assert_eq!(mkdirs.len(), 1, "one mkdir_all");
    assert_eq!(mkdirs[0].1, 0o755, "mkdir mode");
    assert!(
        mkdirs[0].0.starts_with(dir.path()),
        "mkdir confined: {:?}",
        mkdirs[0].0
    );
    drop(mkdirs);

    let writes = rec.writes.lock().unwrap();
    assert_eq!(writes.len(), 1, "one write");
    assert_eq!(writes[0].1, b"hello");
    assert_eq!(writes[0].2, 0o644, "write mode");
    assert!(
        writes[0].0.starts_with(dir.path()),
        "write confined: {:?}",
        writes[0].0
    );
    drop(writes);

    assert_eq!(
        fs::read_to_string(dir.path().join("sub/dir/file.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn write_file_bound_rejects_parent_traversal() {
    let _g = serial().await;
    let dir = tempfile::tempdir().unwrap();
    let rec = RecordingFs::new();

    let err = write_file(dir.path(), "../escape.txt", "x", Some(&rec))
        .await
        .unwrap_err();
    assert!(
        err.message.contains("parent-directory") || err.message.contains("outside"),
        "{}",
        err.message
    );
    assert!(rec.mkdirs.lock().unwrap().is_empty());
    assert!(rec.writes.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Scrubbed env delegation (single source of truth with Task 17)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_scrubbed_env_delegates_to_single_registry() {
    let _g = serial().await;
    // SAFETY: SERIAL is held, so no other test mutates the environment.
    unsafe { std::env::set_var("TASK19_SECRET", "leak") };

    // Setting through files::set_scrubbed_env must mutate the SAME state that
    // tools::shell reads (Task 17 owns it; Task 19 must not add a second one).
    set_scrubbed_env(
        vec!["TASK19_SECRET".to_string()],
        vec!["SUBAGENT_MCP_".to_string()],
    );
    let env = scrubbed_env();
    assert!(
        !env.iter()
            .any(|(k, _)| k.as_os_str() == OsStr::new("TASK19_SECRET"))
    );
    let argv = shell_argv("echo hi");
    assert!(
        argv.iter()
            .any(|a| a.as_os_str() == OsStr::new("TASK19_SECRET"))
    );

    // Restore the global defaults for this test binary.
    set_scrubbed_env(Vec::new(), vec!["SUBAGENT_MCP_".to_string()]);
    // SAFETY: SERIAL is held.
    unsafe { std::env::remove_var("TASK19_SECRET") };
}
