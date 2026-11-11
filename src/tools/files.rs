//! File tools: `read_file` / `write_file` primitives (Task 19).
//!
//! Port of `internal/tools/tools.go`: `ReadFileRange`, `readFileRange`,
//! `isOwnProcessFile`, `isProtectedFile`, `resolveForComparison`, `WriteFile`,
//! `resolvePath`. Task 23B composes the returned `String` / byte count into the
//! agent's tool results.
//!
//! The scrubbed-env table is **not** duplicated here: [`set_scrubbed_env`] is a
//! re-export of the single process-wide state owned by [`crate::tools::shell`]
//! (Task 17). Only the protected-file table is new global state.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use tokio::io::AsyncReadExt;

use crate::mcp::ToolError;
use crate::tools::shell::{DEFAULT_SHELL_TIMEOUT, MAX_OUTPUT_BYTES};
use crate::util::run::RunContext;

/// Go `bufio.NewReaderSize(file, MaxOutputBytes+1)` buffer size.
const READ_BUFFER: usize = MAX_OUTPUT_BYTES + 1;

/// Go `SetScrubbedEnv` — delegate to Task 17's single source of truth.
pub use crate::tools::shell::set_scrubbed_env;

/// Go `protected`. Compared after resolving symlinks (see
/// [`resolve_for_comparison`]).
static PROTECTED: LazyLock<RwLock<Vec<PathBuf>>> = LazyLock::new(|| RwLock::new(Vec::new()));

/// Go `SetProtectedFiles`: the files `read_file_range` refuses to read,
/// compared after resolving symlinks. Relative paths are made absolute against
/// the process working directory, matching Go's `filepath.Abs`.
pub fn set_protected_files(paths: Vec<PathBuf>) {
    let mut guard = PROTECTED.write().unwrap();
    guard.clear();
    for path in paths {
        // Go keeps the original spelling when filepath.Abs fails; the
        // comparison resolver cleans it either way.
        let absolute = go_abs(&path).unwrap_or_else(|_| path.clone());
        guard.push(resolve_for_comparison(&absolute));
    }
}

fn is_protected_file(resolved: &Path) -> bool {
    let candidate = resolve_for_comparison(resolved);
    PROTECTED.read().unwrap().contains(&candidate)
}

/// Go `resolveForComparison`: follow symlinks when possible so a link pointing
/// at a protected file compares equal to the file itself; on failure (e.g. the
/// path does not exist) fall back to a lexical `filepath.Clean`.
fn resolve_for_comparison(path: &Path) -> PathBuf {
    match crate::util::fs::eval_symlinks(path) {
        Ok(resolved) => resolved,
        Err(_) => lexical_clean(path),
    }
}

/// Go `filepath.Abs` for an absolute or relative path: join the process
/// working directory when needed, then clean lexically.
fn go_abs(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(lexical_clean(path))
    } else {
        Ok(lexical_clean(&std::env::current_dir()?.join(path)))
    }
}

/// Go `resolvePath`: an absolute path is returned verbatim (its spelling is
/// preserved); a relative path is joined to `cwd` and cleaned.
fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        lexical_clean(&cwd.join(p))
    }
}

/// Go `filepath.Clean`: drop `.` components, resolve `..` lexically, collapse
/// repeated separators. Rust's `Path::components` already normalizes `.` and
/// repeated separators, so only `..` needs explicit handling.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
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

fn io_error(err: io::Error) -> ToolError {
    ToolError::internal(err.to_string())
}

fn to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ---------------------------------------------------------------------------
// ReadFileRange
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Reason {
    /// Read timeout or parent run deadline (`context.DeadlineExceeded`).
    Deadline,
    /// Parent cancellation (`context.Canceled`).
    Canceled,
}

/// Cancel on the first of: the 60 s read timeout, the parent run deadline, or
/// parent cancellation. Go checks `DeadlineExceeded` before `Canceled`, so the
/// select is biased toward the deadline arms.
async fn watchdog(ctx: &RunContext) -> Reason {
    tokio::select! {
        biased;
        _ = tokio::time::sleep(DEFAULT_SHELL_TIMEOUT) => {}
        _ = async {
            match ctx.deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        } => {}
        _ = ctx.cancel.cancelled() => { return Reason::Canceled; }
    }
    Reason::Deadline
}

fn reason_error(reason: Reason) -> ToolError {
    match reason {
        // Go: `fmt.Errorf("file read timed out: %w", readCtx.Err())`.
        Reason::Deadline => ToolError::internal("file read timed out: context deadline exceeded"),
        // Go: `return "", readCtx.Err()` for context.Canceled.
        Reason::Canceled => ToolError::internal("context canceled"),
    }
}

/// Go `ReadFileRange`: read at most `limit` lines from the 1-based `offset`,
/// capped at [`MAX_OUTPUT_BYTES`] bytes, honoring cancellation / a 60 s read
/// timeout. A non-positive offset means line 1 and a non-positive limit means
/// no line limit.
pub async fn read_file_range(
    ctx: &RunContext,
    cwd: &Path,
    path: &str,
    offset: i64,
    limit: i64,
) -> Result<String, ToolError> {
    // Go checks the protected table first, before opening the file.
    let resolved = resolve_path(cwd, path);
    if is_protected_file(&resolved) {
        return Err(ToolError::internal(format!(
            "refusing to read a protected subagent-mcp file: {}",
            resolved.display()
        )));
    }
    let offset = if offset < 1 { 1 } else { offset };

    // Race the whole open+read against the watchdog, exactly like Go's
    // `context.WithTimeout(ctx, DefaultShellTimeout)` select loop. Dropping the
    // read future abandons a blocked open/read (Go abandons its goroutine).
    tokio::select! {
        biased;
        reason = watchdog(ctx) => Err(reason_error(reason)),
        result = read_inner(resolved, offset, limit) => result,
    }
}

async fn read_inner(path: PathBuf, offset: i64, limit: i64) -> Result<String, ToolError> {
    let file = tokio::fs::File::open(&path).await.map_err(io_error)?;
    // Go checks the opened descriptor (not the path) so `/proc/self`,
    // `/proc/thread-self` and symlinks are seen by their real name.
    if is_own_process_file(&file) {
        return Err(ToolError::internal(
            "refusing to read the subagent-mcp process's own /proc entries",
        ));
    }
    let total_size = file
        .metadata()
        .await
        .map(|meta| meta.len() as i64)
        .unwrap_or(0);
    let mut reader = SliceReader::new(file);
    read_file_range_stream(&mut reader, offset, limit, total_size).await
}

/// Go `isOwnProcessFile`: read the fd's `/proc/self/fd/<fd>` link and refuse
/// anything under this process's `/proc/<pid>` tree. On non-Unix (or where
/// `/proc` is unavailable) there are no such entries to protect.
fn is_own_process_file(file: &tokio::fs::File) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let target = match std::fs::read_link(format!("/proc/self/fd/{}", fd)) {
            Ok(target) => target,
            Err(_) => return false,
        };
        let own = format!("/proc/{}", std::process::id());
        let text = target.to_string_lossy().into_owned();
        text == own || text.starts_with(&format!("{}/", own))
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        false
    }
}

/// The `err` accompanying a `ReadSlice` result.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SliceErr {
    /// `err == nil`: a whole line ending in `\n` was returned.
    Newline,
    /// `bufio.ErrBufferFull`: a buffer-sized chunk of a long line.
    BufferFull,
    /// `io.EOF`: the final line (or nothing).
    Eof,
}

struct Slice {
    data: Vec<u8>,
    err: SliceErr,
}

/// A `bufio.Reader`-equivalent with a fixed `READ_BUFFER` buffer.
/// Mirrors `bufio.Reader.ReadSlice('\n')` and `Peek(1)` without ever reading
/// more than the cap (so a sparse/huge file is bounded to a single chunk).
struct SliceReader {
    file: tokio::fs::File,
    buf: Vec<u8>,
    eof: bool,
}

impl SliceReader {
    fn new(file: tokio::fs::File) -> Self {
        Self {
            file,
            buf: Vec::with_capacity(READ_BUFFER),
            eof: false,
        }
    }

    /// Go `bufio.Reader.ReadSlice('\n')`.
    async fn read_slice(&mut self) -> io::Result<Slice> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let rest = self.buf.split_off(pos + 1);
                let data = std::mem::replace(&mut self.buf, rest);
                return Ok(Slice {
                    data,
                    err: SliceErr::Newline,
                });
            }
            if self.eof {
                let data = std::mem::take(&mut self.buf);
                return Ok(Slice {
                    data,
                    err: SliceErr::Eof,
                });
            }
            if self.buf.len() >= READ_BUFFER {
                let data = std::mem::take(&mut self.buf);
                return Ok(Slice {
                    data,
                    err: SliceErr::BufferFull,
                });
            }
            // Not full, no newline, no pending EOF: fill more (one read).
            let want = READ_BUFFER - self.buf.len();
            let mut chunk = vec![0u8; want];
            let n = self.file.read(&mut chunk).await?;
            if n == 0 {
                self.eof = true;
            } else {
                self.buf.extend_from_slice(&chunk[..n]);
            }
        }
    }

    /// Go `bufio.Reader.Peek(1) != nil`: is at least one more byte available?
    async fn has_more(&mut self) -> io::Result<bool> {
        loop {
            if !self.buf.is_empty() {
                return Ok(true);
            }
            if self.eof {
                return Ok(false);
            }
            let mut chunk = vec![0u8; READ_BUFFER];
            let n = self.file.read(&mut chunk).await?;
            if n == 0 {
                self.eof = true;
            } else {
                self.buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

/// Go `readFileRange`: stream whole lines from `offset`, stopping at the
/// output cap or the line limit. Ports the skip loop, the whole-line cap, the
/// single-over-cap-first-line behavior, and all continuation markers exactly.
async fn read_file_range_stream(
    reader: &mut SliceReader,
    offset: i64,
    limit: i64,
    total_size: i64,
) -> Result<String, ToolError> {
    let mut line_no: i64 = 1;
    let mut newlines: i64 = 0;
    let mut bytes_read: i64 = 0;

    // Skip lines before `offset`.
    while line_no < offset {
        let slice = reader.read_slice().await.map_err(io_error)?;
        bytes_read += slice.data.len() as i64;
        match slice.err {
            SliceErr::Newline => {
                newlines += 1;
                line_no += 1;
            }
            SliceErr::BufferFull => {}
            SliceErr::Eof => {
                let mut lines = newlines;
                if !slice.data.is_empty() {
                    lines += 1;
                }
                return Ok(format!(
                    "[offset {} is past the end of the file ({} lines)]",
                    offset, lines
                ));
            }
        }
    }

    let mut out: Vec<u8> = Vec::with_capacity(MAX_OUTPUT_BYTES);
    let mut returned: i64 = 0;
    loop {
        if limit > 0 && returned >= limit {
            if !reader.has_more().await.map_err(io_error)? {
                return Ok(to_string(&out));
            }
            return Ok(format!(
                "{}\n[more lines follow; continue with offset={}]",
                to_string(&out),
                line_no
            ));
        }

        let slice = reader.read_slice().await.map_err(io_error)?;
        bytes_read += slice.data.len() as i64;
        let line = slice.data;
        let err = slice.err;

        if line.is_empty() {
            // End of file at a line boundary.
            if out.is_empty() && offset > 1 {
                return Ok(format!(
                    "[offset {} is past the end of the file ({} lines)]",
                    offset,
                    offset - 1
                ));
            }
            return Ok(to_string(&out));
        }
        if out.len() + line.len() > MAX_OUTPUT_BYTES {
            // Keep whole lines when at least one whole line was returned; when
            // a single line alone exceeds the cap, keep its first
            // MAX_OUTPUT_BYTES bytes and resume after it.
            if out.is_empty() {
                out.extend_from_slice(&line[..MAX_OUTPUT_BYTES]);
                line_no += 1;
            }
            let size = std::cmp::max(total_size, bytes_read);
            return Ok(format!(
                "{}\n[content truncated: {} bytes total; continue with offset={}]",
                to_string(&out),
                size,
                line_no
            ));
        }
        out.extend_from_slice(&line);
        if err == SliceErr::Newline {
            returned += 1;
            line_no += 1;
        }
        if err == SliceErr::Eof {
            // The final line without a trailing newline was returned in full.
            return Ok(to_string(&out));
        }
    }
}

// ---------------------------------------------------------------------------
// WriteFile
// ---------------------------------------------------------------------------

/// Go `WriteFile`: create parent directories with mode 0755 and write the file
/// with mode 0644, returning the number of bytes written. When a bound
/// (`workspace-write`) filesystem is supplied, the target is confined beneath
/// `cwd` with `policy::path::bound_path` and all writes go through that `FS`.
pub async fn write_file(
    cwd: &Path,
    path: &str,
    content: &str,
    bound: Option<&dyn crate::patch::FS>,
) -> Result<usize, ToolError> {
    let target = Path::new(path);
    let resolved = match bound {
        Some(_) => {
            crate::policy::path::bound_path(cwd, target)
                .map_err(|err| ToolError::internal(err.message))?
                .path
        }
        None => resolve_path(cwd, path),
    };
    let parent = resolved.parent().unwrap_or_else(|| Path::new("."));

    let default_fs = crate::patch::OS;
    let fs: &dyn crate::patch::FS = match bound {
        Some(fs) => fs,
        None => &default_fs,
    };
    fs.mkdir_all(parent, 0o755).map_err(io_error)?;
    fs.write(&resolved, content.as_bytes(), 0o644)
        .map_err(io_error)?;
    Ok(content.len())
}
