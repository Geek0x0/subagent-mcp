//! Patch planning and commit with snapshot/rollback. Ports Go
//! `internal/patch/apply.go`. All content is handled as raw bytes.

use std::collections::HashMap;
use std::fs::Metadata;
use std::io;
use std::path::{Component, Path, PathBuf};

use super::{Change, ChangeKind, Chunk, Patch, PatchError, bytes_to_string, err};
use crate::util::json::go_quote;

/// The filesystem a patch is planned against and committed to. `OS` is the
/// default; a caller that must keep every access beneath one directory supplies
/// an implementation that enforces it in the kernel, so no path component can
/// be swapped for a symlink between a check and the use.
pub trait FS {
    fn stat(&self, path: &Path) -> io::Result<Metadata>;
    fn lstat(&self, path: &Path) -> io::Result<Metadata>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn readlink(&self, path: &Path) -> io::Result<PathBuf>;
    fn mkdir_all(&self, path: &Path, mode: u32) -> io::Result<()>;
    fn write(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()>;
    fn remove(&self, path: &Path) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;
    fn chmod(&self, path: &Path, mode: u32) -> io::Result<()>;
}

/// The ordinary filesystem.
pub struct OS;

impl FS for OS {
    fn stat(&self, path: &Path) -> io::Result<Metadata> {
        std::fs::metadata(path)
    }

    fn lstat(&self, path: &Path) -> io::Result<Metadata> {
        std::fs::symlink_metadata(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn readlink(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(path)
    }

    fn mkdir_all(&self, path: &Path, _mode: u32) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn write(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        write_file_os(path, data, mode)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        remove_os(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        symlink_os(target, link)
    }

    fn chmod(&self, path: &Path, mode: u32) -> io::Result<()> {
        chmod_os(path, mode)
    }
}

/// Go `os.WriteFile`: open with create/truncate and `perm` (only on creation),
/// preserving an existing file's mode.
fn write_file_os(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true).mode(mode);
        let mut file = opts.open(path)?;
        file.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::write(path, data)
    }
}

/// Go `os.Remove`: remove a file, an empty directory, or a symlink (the link
/// itself, not its target).
fn remove_os(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(info) if info.is_dir() => std::fs::remove_dir(path),
        _ => std::fs::remove_file(path),
    }
}

fn symlink_os(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Ok(())
    }
}

fn chmod_os(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Go `FileMode.Perm()`: just the nine permission bits.
fn perm_bits(info: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        info.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = info;
        0o644
    }
}

/// Go `errors.Is(err, fs.ErrNotExist)` for the cases this port exercises.
fn is_not_exist(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// An authored (subagent-mcp) error message, carried as an io error internally.
fn authored(message: String) -> io::Error {
    io::Error::other(message)
}

// ---------------------------------------------------------------------------
// Plan (in-memory overlay)
// ---------------------------------------------------------------------------

struct FileState {
    exists: bool,
    content: Vec<u8>,
}

/// Compute every resulting file in memory; write nothing. Sections touching the
/// same path compose in order, keyed by their cleaned absolute spelling.
pub fn plan_fs(fs: &dyn FS, patch: &Patch) -> Result<Vec<Change>, PatchError> {
    let cwd = &patch.cwd;
    let mut files: HashMap<PathBuf, FileState> = HashMap::new();
    let mut changes = Vec::with_capacity(patch.hunks.len());

    for hunk in &patch.hunks {
        let path = resolve(cwd, &hunk.path);
        match hunk.kind {
            ChangeKind::Add => {
                if let Some(state) = files.get(&path) {
                    if state.exists {
                        return Err(err(format!("add {}: file already exists", hunk.path)));
                    }
                } else {
                    check_target(fs, &path, &format!("add {}", hunk.path), false)?;
                }
                files.insert(
                    path.clone(),
                    FileState {
                        exists: true,
                        content: hunk.content.clone(),
                    },
                );
                changes.push(Change {
                    kind: ChangeKind::Add,
                    path,
                    move_to: None,
                    new_content: Some(hunk.content.clone()),
                    diff: bytes_to_string(&hunk.content),
                });
            }
            ChangeKind::Delete => {
                if let Some(state) = files.get(&path) {
                    if !state.exists {
                        return Err(err(format!("delete {}: file does not exist", hunk.path)));
                    }
                } else {
                    let info = fs
                        .stat(&path)
                        .map_err(|e| err(format!("delete {}: {}", hunk.path, e)))?;
                    if info.is_dir() {
                        return Err(err(format!("delete {}: is a directory", hunk.path)));
                    }
                }
                files.insert(
                    path.clone(),
                    FileState {
                        exists: false,
                        content: Vec::new(),
                    },
                );
                changes.push(Change {
                    kind: ChangeKind::Delete,
                    path,
                    move_to: None,
                    new_content: None,
                    diff: String::new(),
                });
            }
            ChangeKind::Update => {
                let original = planner_read(fs, &files, &path)
                    .map_err(|e| err(format!("update {}: {}", hunk.path, e)))?;
                let (content, diff) = apply_chunks(&original, &hunk.chunks)
                    .map_err(|e| err(format!("update {}: {}", hunk.path, e)))?;
                let mut change = Change {
                    kind: ChangeKind::Update,
                    path: path.clone(),
                    move_to: None,
                    new_content: Some(content.clone()),
                    diff,
                };
                if !hunk.move_to.is_empty() {
                    let move_to = resolve(cwd, &hunk.move_to);
                    change.move_to = Some(move_to.clone());
                    if move_to != path {
                        if let Some(state) = files.get(&move_to) {
                            if state.exists {
                                return Err(err(format!(
                                    "move {}: file already exists",
                                    hunk.move_to
                                )));
                            }
                        } else {
                            check_target(fs, &move_to, &format!("move {}", hunk.move_to), true)?;
                        }
                        files.insert(
                            path.clone(),
                            FileState {
                                exists: false,
                                content: Vec::new(),
                            },
                        );
                        files.insert(
                            move_to,
                            FileState {
                                exists: true,
                                content: content.clone(),
                            },
                        );
                    }
                }
                if change.move_to.is_none() || change.move_to.as_ref() == Some(&path) {
                    files.insert(
                        path,
                        FileState {
                            exists: true,
                            content,
                        },
                    );
                }
                changes.push(change);
            }
        }
    }
    Ok(changes)
}

/// Planned content of `path`: an earlier section's result, or the file on disk.
fn planner_read(
    fs: &dyn FS,
    files: &HashMap<PathBuf, FileState>,
    path: &Path,
) -> Result<Vec<u8>, String> {
    if let Some(state) = files.get(path) {
        if !state.exists {
            return Err("file does not exist".to_string());
        }
        return Ok(state.content.clone());
    }
    fs.read(path).map_err(|e| e.to_string())
}

/// Reject, before anything is written, a target Commit could not write as a
/// regular file.
fn check_target(
    fs: &dyn FS,
    path: &Path,
    label: &str,
    allow_existing: bool,
) -> Result<(), PatchError> {
    match fs.lstat(path) {
        Ok(_) if !allow_existing => Err(err(format!("{}: file already exists", label))),
        Ok(info) if info.is_dir() => Err(err(format!("{}: is a directory", label))),
        Ok(_) => Ok(()),
        Err(e) if is_not_exist(&e) => Ok(()),
        Err(e) => Err(err(format!("{}: {}", label, e))),
    }
}

/// Go `resolve`: absolute paths are cleaned, relative ones joined to cwd.
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        lexical_clean(p)
    } else {
        lexical_clean(&cwd.join(p))
    }
}

/// Go `filepath.Clean`, lexically.
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

// ---------------------------------------------------------------------------
// applyChunks (byte-oriented)
// ---------------------------------------------------------------------------

struct Replacement {
    start: usize,
    old_len: usize,
    lines: Vec<Vec<u8>>,
    raw: Vec<Vec<u8>>,
}

fn apply_chunks(original: &[u8], chunks: &[Chunk]) -> Result<(Vec<u8>, String), String> {
    // A file containing any CRLF is treated as CRLF throughout.
    let crlf = contains(original, b"\r\n");
    let normalized: Vec<u8> = if crlf {
        replace_all(original, b"\r\n", b"\n")
    } else {
        original.to_vec()
    };
    let trailing_newline = normalized.is_empty() || normalized.ends_with(b"\n");
    let lines: Vec<Vec<u8>> = if normalized.is_empty() {
        Vec::new()
    } else {
        let trimmed = if normalized.ends_with(b"\n") {
            &normalized[..normalized.len() - 1]
        } else {
            &normalized[..]
        };
        split_bytes(trimmed, b'\n')
    };

    let mut replacements: Vec<Replacement> = Vec::new();
    let mut position = 0usize;
    for chunk in chunks {
        if !chunk.header.is_empty() {
            let at = seek(&lines, std::slice::from_ref(&chunk.header), position, false)
                .ok_or_else(|| {
                    format!(
                        "context header {} not found",
                        go_quote(&bytes_to_string(&chunk.header))
                    )
                })?;
            position = at + 1;
        }
        if chunk.old.is_empty() {
            let at = if chunk.header.is_empty() {
                lines.len()
            } else {
                position
            };
            replacements.push(Replacement {
                start: at,
                old_len: 0,
                lines: chunk.new.clone(),
                raw: chunk.raw.clone(),
            });
            position = at;
            continue;
        }
        let at = seek(&lines, &chunk.old, position, chunk.eof).ok_or_else(|| {
            format!(
                "could not find chunk starting with {}",
                go_quote(&bytes_to_string(&chunk.old[0]))
            )
        })?;
        replacements.push(Replacement {
            start: at,
            old_len: chunk.old.len(),
            lines: chunk.new.clone(),
            raw: chunk.raw.clone(),
        });
        position = at + chunk.old.len();
    }

    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut diff = String::new();
    let mut previous = 0usize;
    let mut offset: isize = 0;
    for r in &replacements {
        if previous <= r.start {
            out.extend_from_slice(&lines[previous..r.start]);
        }
        out.extend(r.lines.iter().cloned());
        let new_start = r.start as isize + 1 + offset;
        diff.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            r.start + 1,
            r.old_len,
            new_start,
            r.lines.len()
        ));
        for line in &r.raw {
            diff.push_str(&bytes_to_string(line));
            diff.push('\n');
        }
        offset += r.lines.len() as isize - r.old_len as isize;
        previous = r.start + r.old_len;
    }
    if previous <= lines.len() {
        out.extend_from_slice(&lines[previous..]);
    }

    let mut result: Vec<u8> = Vec::new();
    for (i, line) in out.iter().enumerate() {
        if i > 0 {
            result.push(b'\n');
        }
        result.extend_from_slice(line);
    }
    if trailing_newline && !out.is_empty() {
        result.push(b'\n');
    }
    if crlf {
        result = replace_all(&result, b"\n", b"\r\n");
    }
    Ok((result, diff))
}

/// Go `seek`: exact, trailing-whitespace-insensitive, then surrounding-
/// whitespace-insensitive comparison; `eof` anchors the match at the end.
fn seek(lines: &[Vec<u8>], pattern: &[Vec<u8>], start: usize, eof: bool) -> Option<usize> {
    for norm in 0..3usize {
        if eof {
            if pattern.len() <= lines.len() {
                let at = lines.len() - pattern.len();
                if at >= start && match_at(lines, pattern, at, norm) {
                    return Some(at);
                }
            }
        } else {
            let mut at = start;
            while at + pattern.len() <= lines.len() {
                if match_at(lines, pattern, at, norm) {
                    return Some(at);
                }
                at += 1;
            }
        }
    }
    None
}

fn match_at(lines: &[Vec<u8>], pattern: &[Vec<u8>], at: usize, norm: usize) -> bool {
    for (i, want) in pattern.iter().enumerate() {
        if normalize(&lines[at + i], norm) != normalize(want, norm) {
            return false;
        }
    }
    true
}

/// Go normalizers: identity, TrimRight(" \t"), TrimSpace.
fn normalize(line: &[u8], norm: usize) -> Vec<u8> {
    match norm {
        1 => {
            let mut end = line.len();
            while end > 0 && (line[end - 1] == b' ' || line[end - 1] == b'\t') {
                end -= 1;
            }
            line[..end].to_vec()
        }
        2 => match std::str::from_utf8(line) {
            Ok(s) => s.trim().as_bytes().to_vec(),
            Err(_) => {
                let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
                let mut start = 0;
                let mut end = line.len();
                while start < end && is_ws(line[start]) {
                    start += 1;
                }
                while end > start && is_ws(line[end - 1]) {
                    end -= 1;
                }
                line[start..end].to_vec()
            }
        },
        _ => line.to_vec(),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || haystack.windows(needle.len()).any(|w| w == needle)
}

fn replace_all(input: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return input.to_vec();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        if i + from.len() <= input.len() && &input[i..i + from.len()] == from {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

/// Go `strings.Split(s, "\n")` for a single-byte separator.
fn split_bytes(s: &[u8], sep: u8) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &b) in s.iter().enumerate() {
        if b == sep {
            out.push(s[start..i].to_vec());
            start = i + 1;
        }
    }
    out.push(s[start..].to_vec());
    out
}

// ---------------------------------------------------------------------------
// Commit (snapshots + reverse rollback)
// ---------------------------------------------------------------------------

/// What a path looked like before Commit first touched it.
#[derive(Clone, Default)]
struct Snapshot {
    trashed: Option<PathBuf>,
    exists: bool,
    link: Option<PathBuf>,
    data: Option<Vec<u8>>,
    mode: u32,
}

struct CommitTx<'a> {
    fs: &'a dyn FS,
    saved: HashMap<PathBuf, Snapshot>,
    order: Vec<PathBuf>,
    dirs: Vec<PathBuf>,
    trash: Vec<PathBuf>,
    serial: i32,
}

impl<'a> CommitTx<'a> {
    fn new(fs: &'a dyn FS) -> Self {
        CommitTx {
            fs,
            saved: HashMap::new(),
            order: Vec::new(),
            dirs: Vec::new(),
            trash: Vec::new(),
            serial: 0,
        }
    }

    fn apply(&mut self, change: &Change) -> io::Result<()> {
        match change.kind {
            ChangeKind::Add => {
                self.tx_write(&change.path, change.new_content.as_deref().unwrap_or(&[]))
            }
            ChangeKind::Delete => self.tx_remove(&change.path),
            ChangeKind::Update => {
                let target = change.move_to.as_deref().unwrap_or(change.path.as_path());
                self.tx_write(target, change.new_content.as_deref().unwrap_or(&[]))?;
                match &change.move_to {
                    Some(mt) if mt != &change.path => self.tx_remove(&change.path),
                    _ => Ok(()),
                }
            }
        }
    }

    fn save(&mut self, path: &Path) -> io::Result<()> {
        if self.saved.contains_key(path) {
            return Ok(());
        }
        let info = match self.fs.lstat(path) {
            Ok(i) => Some(i),
            Err(e) if is_not_exist(&e) => None,
            Err(e) => return Err(e),
        };
        let mut snap = Snapshot::default();
        if let Some(info) = info {
            if info.is_dir() {
                return Err(authored(format!("{}: is a directory", path.display())));
            }
            snap.exists = true;
            snap.mode = perm_bits(&info);
            if info.file_type().is_symlink() {
                snap.link = Some(self.fs.readlink(path)?);
            }
            match self.fs.read(path) {
                Ok(data) => snap.data = Some(data),
                Err(_) => snap.data = None,
            }
            if snap.link.is_none() && snap.data.is_none() {
                return Err(authored(format!(
                    "{}: cannot read the file to back it up",
                    path.display()
                )));
            }
        }
        self.saved.insert(path.to_path_buf(), snap);
        self.order.push(path.to_path_buf());
        Ok(())
    }

    fn tx_write(&mut self, path: &Path, content: &[u8]) -> io::Result<()> {
        self.save(path)?;
        self.mkdir_all(&go_dir(path))?;
        self.fs.write(path, content, 0o644)
    }

    /// Delete a file by setting it aside, so it can be put back exactly even if
    /// unreadable. The set-aside copies are deleted once Commit succeeds.
    fn tx_remove(&mut self, path: &Path) -> io::Result<()> {
        if self.saved.contains_key(path) {
            return self.fs.remove(path);
        }
        let info = self.fs.lstat(path)?;
        if info.is_dir() {
            return Err(authored(format!("{}: is a directory", path.display())));
        }
        self.serial += 1;
        let trash = go_dir(path).join(format!(
            ".{}.subagent-mcp-trash-{}-{}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id(),
            self.serial
        ));
        self.fs.rename(path, &trash)?;
        self.saved.insert(
            path.to_path_buf(),
            Snapshot {
                trashed: Some(trash.clone()),
                exists: true,
                ..Snapshot::default()
            },
        );
        self.order.push(path.to_path_buf());
        self.trash.push(trash);
        Ok(())
    }

    /// Record directories created for a write so rollback can remove them.
    fn mkdir_all(&mut self, dir: &Path) -> io::Result<()> {
        let mut missing: Vec<PathBuf> = Vec::new();
        let mut d = dir.to_path_buf();
        loop {
            match self.fs.lstat(&d) {
                Err(e) if is_not_exist(&e) => missing.push(d.clone()),
                _ => break,
            }
            let parent = go_dir(&d);
            if parent == d {
                break;
            }
            d = parent;
        }
        let result = self.fs.mkdir_all(dir, 0o755);
        self.dirs.extend(missing);
        result
    }

    fn finish(&self) {
        for trash in &self.trash {
            let _ = self.fs.remove(trash);
        }
    }

    /// Restore every saved path in reverse order and remove created
    /// directories. Returns the restore failures, if any.
    fn rollback(&self) -> Vec<String> {
        let mut failures = Vec::new();
        for path in self.order.iter().rev() {
            let snap = &self.saved[path];
            if let Err(e) = restore(snap, self.fs, path) {
                failures.push(e.to_string());
            }
        }
        for dir in &self.dirs {
            // Best effort: a directory that is no longer empty is not ours.
            let _ = self.fs.remove(dir);
        }
        failures
    }
}

fn restore(snap: &Snapshot, fs: &dyn FS, path: &Path) -> io::Result<()> {
    if let Some(trashed) = &snap.trashed {
        remove_ignore_not_exist(fs, path)?;
        return fs.rename(trashed, path);
    }
    if !snap.exists {
        return remove_ignore_not_exist(fs, path);
    }
    if let Some(link) = &snap.link {
        let need_recreate = match fs.lstat(path) {
            Ok(info) => {
                !info.file_type().is_symlink()
                    || readlink_or_empty(fs, path).as_deref() != Some(link.as_path())
            }
            Err(_) => true,
        };
        if need_recreate {
            remove_ignore_not_exist(fs, path)?;
            fs.symlink(link, path)?;
        }
        let Some(data) = &snap.data else {
            return Ok(());
        };
        return fs.write(path, data, 0o644);
    }
    let data = snap.data.as_deref().unwrap_or(&[]);
    fs.write(path, data, snap.mode)?;
    fs.chmod(path, snap.mode)
}

fn remove_ignore_not_exist(fs: &dyn FS, path: &Path) -> io::Result<()> {
    match fs.remove(path) {
        Ok(()) => Ok(()),
        Err(e) if is_not_exist(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

fn readlink_or_empty(fs: &dyn FS, path: &Path) -> Option<PathBuf> {
    fs.readlink(path).ok()
}

/// Go `filepath.Dir` for the cleaned absolute paths this engine plans.
fn go_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Write planned changes in order. On any failure, undo the steps already
/// taken, so every touched file is left as before. A failed undo is reported
/// in the returned error.
pub fn commit_fs(fs: &dyn FS, changes: &[Change]) -> Result<(), PatchError> {
    let mut tx = CommitTx::new(fs);
    for change in changes {
        if let Err(e) = tx.apply(change) {
            let cause = e.to_string();
            let failures = tx.rollback();
            return Err(if failures.is_empty() {
                err(cause)
            } else {
                err(format!(
                    "{} (rollback failed, files may be left modified: {})",
                    cause,
                    failures.join("\n")
                ))
            });
        }
    }
    tx.finish();
    Ok(())
}
