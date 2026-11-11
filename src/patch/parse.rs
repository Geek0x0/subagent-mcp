//! Codex patch parser. Ports Go `internal/patch/parse.go` exactly.

use std::path::PathBuf;

use super::{ChangeKind, Chunk, Hunk, Patch, PatchError, bytes_to_string, err};
use crate::util::json::go_quote;

const BEGIN_MARKER: &str = "*** Begin Patch";
const END_MARKER: &str = "*** End Patch";
const ADD_PREFIX: &str = "*** Add File: ";
const DELETE_PREFIX: &str = "*** Delete File: ";
const UPDATE_PREFIX: &str = "*** Update File: ";
const MOVE_PREFIX: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";

/// Parse a complete patch. It never touches the filesystem.
pub fn parse_patch(input: &str) -> Result<Patch, PatchError> {
    // Go: strings.TrimSpace(strings.ReplaceAll(text, "\r\n", "\n"))
    let replaced = input.replace("\r\n", "\n");
    let text = replaced.trim();
    let mut lines: Vec<&str> = text.split('\n').collect();

    // Heredoc wrapper: "<<..." ... "EOF".
    if lines.len() >= 2
        && lines[0].trim().starts_with("<<")
        && lines[lines.len() - 1].trim() == "EOF"
    {
        lines = lines[1..lines.len() - 1].to_vec();
    }

    if lines.len() < 2 || lines[0].trim() != BEGIN_MARKER {
        return Err(err(format!(
            "patch must start with {}",
            go_quote(BEGIN_MARKER)
        )));
    }
    if lines[lines.len() - 1].trim() != END_MARKER {
        return Err(err(format!("patch must end with {}", go_quote(END_MARKER))));
    }

    let body = &lines[1..lines.len() - 1];
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut i = 0usize;
    while i < body.len() {
        let line = body[i];
        if let Some(rest) = line.strip_prefix(ADD_PREFIX) {
            let mut hunk = Hunk {
                kind: ChangeKind::Add,
                path: rest.trim().to_string(),
                move_to: String::new(),
                content: Vec::new(),
                chunks: Vec::new(),
            };
            i += 1;
            let mut content: Vec<u8> = Vec::new();
            while i < body.len() {
                match body[i].strip_prefix('+') {
                    Some(r) => {
                        content.extend_from_slice(r.as_bytes());
                        content.push(b'\n');
                        i += 1;
                    }
                    None => break,
                }
            }
            hunk.content = content;
            hunks.push(hunk);
        } else if let Some(rest) = line.strip_prefix(DELETE_PREFIX) {
            hunks.push(Hunk {
                kind: ChangeKind::Delete,
                path: rest.trim().to_string(),
                move_to: String::new(),
                content: Vec::new(),
                chunks: Vec::new(),
            });
            i += 1;
        } else if let Some(rest) = line.strip_prefix(UPDATE_PREFIX) {
            let mut hunk = Hunk {
                kind: ChangeKind::Update,
                path: rest.trim().to_string(),
                move_to: String::new(),
                content: Vec::new(),
                chunks: Vec::new(),
            };
            i += 1;
            if i < body.len()
                && let Some(m) = body[i].strip_prefix(MOVE_PREFIX)
            {
                hunk.move_to = m.trim().to_string();
                i += 1;
            }
            let (chunks, next) = parse_chunks(body, i, &hunk.path)?;
            hunk.chunks = chunks;
            i = next;
            hunks.push(hunk);
        } else if line.trim().is_empty() {
            i += 1;
        } else {
            return Err(err(format!(
                "unexpected line {}; expected a file operation header",
                go_quote(line)
            )));
        }
    }

    if hunks.is_empty() {
        return Err(err("patch contains no file operations".to_string()));
    }
    for hunk in &hunks {
        if hunk.path.is_empty() {
            return Err(err("file operation has an empty path".to_string()));
        }
    }
    Ok(Patch {
        cwd: PathBuf::new(),
        hunks,
    })
}

fn parse_chunks(
    body: &[&str],
    mut i: usize,
    path: &str,
) -> Result<(Vec<Chunk>, usize), PatchError> {
    let mut chunks: Vec<Chunk> = Vec::new();
    loop {
        if i >= body.len() {
            break;
        }
        let line = body[i];
        if line == EOF_MARKER {
            if chunks.is_empty() {
                return Err(err(format!(
                    "{}: {} before any change line",
                    path,
                    go_quote(EOF_MARKER)
                )));
            }
            chunks.last_mut().unwrap().eof = true;
        } else if line.starts_with("*** ") {
            break;
        } else if line == "@@" || line.starts_with("@@ ") {
            let header = line.strip_prefix("@@").unwrap().trim().as_bytes().to_vec();
            chunks.push(Chunk {
                header,
                old: Vec::new(),
                new: Vec::new(),
                raw: Vec::new(),
                eof: false,
            });
        } else {
            if chunks.is_empty() {
                chunks.push(Chunk {
                    header: Vec::new(),
                    old: Vec::new(),
                    new: Vec::new(),
                    raw: Vec::new(),
                    eof: false,
                });
            }
            let chunk = chunks.last_mut().unwrap();
            // A blank line inside an Update is an empty context line.
            let line_bytes: Vec<u8> = if line.is_empty() {
                vec![b' ']
            } else {
                line.as_bytes().to_vec()
            };
            let first = line_bytes[0];
            let text = line_bytes[1..].to_vec();
            match first {
                b' ' => {
                    chunk.old.push(text.clone());
                    chunk.new.push(text);
                }
                b'-' => chunk.old.push(text),
                b'+' => chunk.new.push(text),
                _ => {
                    return Err(err(format!(
                        "{}: invalid patch line {}; lines must start with ' ', '-', or '+'",
                        path,
                        go_quote(line)
                    )));
                }
            }
            chunk.raw.push(line_bytes);
        }
        i += 1;
    }

    if chunks.is_empty() {
        return Err(err(format!("{}: update has no changes", path)));
    }
    for chunk in &chunks {
        if chunk.raw.is_empty() {
            return Err(err(format!(
                "{}: chunk {} has no change lines",
                path,
                go_quote(&bytes_to_string(&chunk.header))
            )));
        }
    }
    Ok((chunks, i))
}
