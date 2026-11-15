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
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use tokio::io::AsyncReadExt;

use crate::mcp::ToolError;
use crate::tools::shell::{DEFAULT_SHELL_TIMEOUT, MAX_OUTPUT_BYTES};
use crate::util::fs::lexical_clean;
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
    // Check the path before opening it. `open(2)` on a FIFO blocks until a
    // writer appears, so a blocking open would hang the tool (and its worker
    // thread) before any post-open check could reject the file.
    let metadata = tokio::fs::metadata(&path).await.map_err(io_error)?;
    if !metadata.is_file() {
        return Err(ToolError::internal(format!(
            "refusing to read a non-regular file: {}",
            path.display()
        )));
    }
    let file = tokio::fs::File::open(&path).await.map_err(io_error)?;
    // Go checks the opened descriptor (not the path) so `/proc/self`,
    // `/proc/thread-self` and symlinks are seen by their real name.
    if is_own_process_file(&file) {
        return Err(ToolError::internal(
            "refusing to read the subagent-mcp process's own /proc entries",
        ));
    }
    // Re-check on the opened descriptor: the path may have been replaced
    // between the stat above and the open (a symlink swap or a FIFO swap).
    let metadata = file.metadata().await.map_err(io_error)?;
    // Only regular files are readable: a FIFO, character device, socket or
    // directory can block the read forever or expose another program's stream
    // (`/dev/stdin`, `/proc/self/fd/0`). A symlink was already followed by
    // `open`, so this describes the object actually opened.
    if !metadata.is_file() {
        return Err(ToolError::internal(format!(
            "refusing to read a non-regular file: {}",
            path.display()
        )));
    }
    let total_size = metadata.len() as i64;
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
    /// Bytes of `buf` already returned. Keeping a cursor instead of
    /// `split_off`/`drain`-ing on every line makes a whole-line scan linear:
    /// the old code copied the entire tail of the buffer once per line.
    start: usize,
    eof: bool,
}

impl SliceReader {
    fn new(file: tokio::fs::File) -> Self {
        Self {
            file,
            buf: Vec::with_capacity(READ_BUFFER),
            start: 0,
            eof: false,
        }
    }

    /// Bytes buffered but not yet returned.
    fn available(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Drop the consumed prefix once it dominates the buffer.
    fn compact(&mut self) {
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start >= READ_BUFFER {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }

    /// Go `bufio.Reader.ReadSlice('\n')`.
    async fn read_slice(&mut self) -> io::Result<Slice> {
        loop {
            if let Some(offset) = self.buf[self.start..].iter().position(|&b| b == b'\n') {
                let end = self.start + offset + 1;
                let data = self.buf[self.start..end].to_vec();
                self.start = end;
                self.compact();
                return Ok(Slice {
                    data,
                    err: SliceErr::Newline,
                });
            }
            if self.eof {
                let data = self.buf[self.start..].to_vec();
                self.buf.clear();
                self.start = 0;
                return Ok(Slice {
                    data,
                    err: SliceErr::Eof,
                });
            }
            if self.available() >= READ_BUFFER {
                let data = self.buf[self.start..].to_vec();
                self.buf.clear();
                self.start = 0;
                return Ok(Slice {
                    data,
                    err: SliceErr::BufferFull,
                });
            }
            // Not full, no newline, no pending EOF: fill more (one read).
            let want = READ_BUFFER - self.available();
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
            if self.available() > 0 {
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
    crate::patch::check_write_target(fs, &resolved).map_err(io_error)?;
    fs.write(&resolved, content.as_bytes(), 0o644)
        .map_err(io_error)?;
    Ok(content.len())
}
