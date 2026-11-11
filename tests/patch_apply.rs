//! Patch engine integration tests.
//!
//! Port of Go `internal/patch/parse_test.go`, `compose_test.go`, and
//! `apply_test.go`. Task 16 owns the Codex patch parser, the in-memory plan,
//! and the snapshot/rollback commit. The public `FS` trait is exercised with
//! the real `OS` where the Go tests use the real filesystem, and with a small
//! fault-injecting FS where the Go tests reach into private commit internals.

use std::cell::Cell;
use std::fs;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

use subagent_mcp::patch::{
    Change, ChangeKind, Chunk, FS, Hunk, OS, Patch, PatchError, commit_fs, parse_patch, plan_fs,
    summary,
};

fn bytes(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn byte_lines(items: &[&str]) -> Vec<Vec<u8>> {
    items.iter().map(|s| bytes(s)).collect()
}

fn write_file(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn read_file(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap()
}

fn patch_text(lines: &[&str]) -> String {
    let mut all = vec!["*** Begin Patch"];
    all.extend_from_slice(lines);
    all.push("*** End Patch");
    all.join("\n")
}

fn parse_in(cwd: &Path, text: &str) -> Patch {
    let mut patch = parse_patch(text).expect("patch should parse");
    patch.cwd = cwd.to_path_buf();
    patch
}

fn plan_only(cwd: &Path, text: &str) -> Vec<Change> {
    let patch = parse_in(cwd, text);
    plan_fs(&OS, &patch).expect("plan should succeed")
}

fn apply_text(cwd: &Path, text: &str) -> Result<Vec<Change>, PatchError> {
    let patch = parse_in(cwd, text);
    let changes = plan_fs(&OS, &patch)?;
    commit_fs(&OS, &changes)?;
    Ok(changes)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

// tests/config.rs convention: Unix gets the real mode change, non-Unix a no-op
// so unguarded calls still compile. `get_mode` has no portable counterpart, so
// the assertions that read a mode are gated with `#[cfg(unix)]` at their call
// sites instead. Unix coverage is unchanged; all other tests run everywhere.
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(unix)]
fn get_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // Mirrors Go's `os.Geteuid() == 0` skip: root can read mode-000 files.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

// ---------------------------------------------------------------------------
// parse_test.go
// ---------------------------------------------------------------------------

#[test]
fn parse_all_operations() {
    let text = [
        "*** Begin Patch",
        "*** Add File: new.txt",
        "+hello",
        "+world",
        "*** Delete File: old.txt",
        "*** Update File: src/a.go",
        "*** Move to: src/b.go",
        "@@ func main() {",
        " \tkeep",
        "-\tgone",
        "+\tadded",
        "",
        "@@",
        "-tail",
        "+TAIL",
        "*** End of File",
        "*** End Patch",
    ]
    .join("\n");

    let patch = parse_patch(&text).unwrap();
    let want = vec![
        Hunk {
            kind: ChangeKind::Add,
            path: "new.txt".into(),
            move_to: String::new(),
            content: bytes("hello\nworld\n"),
            chunks: vec![],
        },
        Hunk {
            kind: ChangeKind::Delete,
            path: "old.txt".into(),
            move_to: String::new(),
            content: vec![],
            chunks: vec![],
        },
        Hunk {
            kind: ChangeKind::Update,
            path: "src/a.go".into(),
            move_to: "src/b.go".into(),
            content: vec![],
            chunks: vec![
                Chunk {
                    header: bytes("func main() {"),
                    old: byte_lines(&["\tkeep", "\tgone", ""]),
                    new: byte_lines(&["\tkeep", "\tadded", ""]),
                    raw: byte_lines(&[" \tkeep", "-\tgone", "+\tadded", " "]),
                    eof: false,
                },
                Chunk {
                    header: vec![],
                    old: byte_lines(&["tail"]),
                    new: byte_lines(&["TAIL"]),
                    raw: byte_lines(&["-tail", "+TAIL"]),
                    eof: true,
                },
            ],
        },
    ];
    assert_eq!(patch.hunks, want);
    assert_eq!(
        patch.paths(),
        vec!["new.txt", "old.txt", "src/a.go", "src/b.go"]
    );
    assert_eq!(ChangeKind::Add.as_str(), "add");
    assert_eq!(ChangeKind::Delete.as_str(), "delete");
    assert_eq!(ChangeKind::Update.as_str(), "update");
}

#[test]
fn parse_first_chunk_without_header() {
    let patch =
        parse_patch("*** Begin Patch\n*** Update File: a.txt\n-x\n+y\n*** End Patch").unwrap();
    assert_eq!(patch.hunks[0].chunks.len(), 1);
    assert_eq!(patch.hunks[0].chunks[0].old[0], bytes("x"));
}

#[test]
fn parse_tolerates_wrappers_and_crlf() {
    let cases: Vec<(&str, &str)> = vec![
        (
            "heredoc",
            "<<'EOF'\n*** Begin Patch\n*** Delete File: a.txt\n*** End Patch\nEOF\n",
        ),
        (
            "crlf",
            "*** Begin Patch\r\n*** Delete File: a.txt\r\n*** End Patch\r\n",
        ),
        (
            "whitespace",
            "\n\n  *** Begin Patch\n*** Delete File: a.txt\n*** End Patch  \n\n",
        ),
    ];
    for (name, text) in cases {
        let patch = parse_patch(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(patch.hunks.len(), 1, "{name}");
        assert_eq!(patch.hunks[0].path, "a.txt", "{name}");
    }
}

#[test]
fn parse_errors() {
    let cases: Vec<(&str, &str)> = vec![
        ("missing begin", "*** Delete File: a\n*** End Patch"),
        ("missing end", "*** Begin Patch\n*** Delete File: a"),
        ("empty", "*** Begin Patch\n*** End Patch"),
        (
            "unknown header",
            "*** Begin Patch\n*** Rename File: a\n*** End Patch",
        ),
        (
            "bad chunk line",
            "*** Begin Patch\n*** Update File: a\n@@\n?x\n*** End Patch",
        ),
        (
            "update no changes",
            "*** Begin Patch\n*** Update File: a\n*** End Patch",
        ),
        (
            "empty path",
            "*** Begin Patch\n*** Delete File: \n*** End Patch",
        ),
        (
            "eof first",
            "*** Begin Patch\n*** Update File: a\n*** End of File\n*** End Patch",
        ),
    ];
    for (name, text) in cases {
        assert!(parse_patch(text).is_err(), "{name} should fail");
    }
}

#[test]
fn parse_error_text_matches_go() {
    let cases: Vec<(&str, &str)> = vec![
        (
            "*** Delete File: a\n*** End Patch",
            "patch must start with \"*** Begin Patch\"",
        ),
        (
            "*** Begin Patch\n*** Delete File: a",
            "patch must end with \"*** End Patch\"",
        ),
        (
            "*** Begin Patch\n*** End Patch",
            "patch contains no file operations",
        ),
        (
            "*** Begin Patch\n*** Rename File: a\n*** End Patch",
            "unexpected line \"*** Rename File: a\"; expected a file operation header",
        ),
        (
            "*** Begin Patch\n*** Update File: a\n@@\n?x\n*** End Patch",
            "a: invalid patch line \"?x\"; lines must start with ' ', '-', or '+'",
        ),
        (
            "*** Begin Patch\n*** Update File: a\n*** End Patch",
            "a: update has no changes",
        ),
        (
            "*** Begin Patch\n*** Delete File: \n*** End Patch",
            "file operation has an empty path",
        ),
        (
            "*** Begin Patch\n*** Update File: a\n*** End of File\n*** End Patch",
            "a: \"*** End of File\" before any change line",
        ),
        (
            "*** Begin Patch\n*** Update File: a\n@@\n*** End Patch",
            "a: chunk \"\" has no change lines",
        ),
    ];
    for (text, want) in cases {
        assert_eq!(parse_patch(text).unwrap_err().to_string(), want);
    }
}

// ---------------------------------------------------------------------------
// compose_test.go
// ---------------------------------------------------------------------------

#[test]
fn same_file_updates_compose() {
    let dir = tempfile::tempdir().unwrap();
    write_file(&dir.path().join("a.txt"), b"a\nb\nc\n");
    apply_text(
        dir.path(),
        &patch_text(&[
            "*** Update File: a.txt",
            "@@",
            "-a",
            "+A",
            "*** Update File: a.txt",
            "@@",
            "-c",
            "+C",
        ]),
    )
    .unwrap();
    assert_eq!(read_file(&dir.path().join("a.txt")), bytes("A\nb\nC\n"));
}

#[test]
fn second_section_sees_first_sections_edit() {
    let dir = tempfile::tempdir().unwrap();
    write_file(&dir.path().join("a.txt"), b"a\nb\n");
    apply_text(
        dir.path(),
        &patch_text(&[
            "*** Update File: a.txt",
            "@@",
            "-a",
            "+A",
            "*** Update File: a.txt",
            "@@",
            "-A",
            "+AA",
        ]),
    )
    .unwrap();
    assert_eq!(read_file(&dir.path().join("a.txt")), bytes("AA\nb\n"));
}

#[test]
fn same_path_spellings_are_one_file() {
    let dir = tempfile::tempdir().unwrap();
    write_file(&dir.path().join("a.txt"), b"a\nb\nc\n");
    let abs = dir.path().join("a.txt");
    let abs_line = format!("*** Update File: {}", abs.display());
    apply_text(
        dir.path(),
        &patch_text(&[
            "*** Update File: a.txt",
            "@@",
            "-a",
            "+A",
            "*** Update File: ./a.txt",
            "@@",
            "-b",
            "+B",
            &abs_line,
            "@@",
            "-c",
            "+C",
        ]),
    )
    .unwrap();
    assert_eq!(read_file(&dir.path().join("a.txt")), bytes("A\nB\nC\n"));
}

#[test]
fn same_path_combinations() {
    struct Case {
        name: &'static str,
        files: Vec<(&'static str, &'static str)>,
        lines: Vec<&'static str>,
        want: Vec<(&'static str, &'static str)>,
        want_err: Option<&'static str>,
    }
    let cases = vec![
        Case {
            name: "add then update",
            files: vec![],
            lines: vec![
                "*** Add File: n.txt",
                "+one",
                "*** Update File: n.txt",
                "@@",
                "-one",
                "+ONE",
            ],
            want: vec![("n.txt", "ONE\n")],
            want_err: None,
        },
        Case {
            name: "update then update with move",
            files: vec![("a.txt", "a\nb\nc\n")],
            lines: vec![
                "*** Update File: a.txt",
                "@@",
                "-a",
                "+A",
                "*** Update File: a.txt",
                "*** Move to: b.txt",
                "@@",
                "-c",
                "+C",
            ],
            want: vec![("a.txt", ""), ("b.txt", "A\nb\nC\n")],
            want_err: None,
        },
        Case {
            name: "delete then add",
            files: vec![("x.txt", "old\n")],
            lines: vec!["*** Delete File: x.txt", "*** Add File: x.txt", "+new"],
            want: vec![("x.txt", "new\n")],
            want_err: None,
        },
        Case {
            name: "add twice",
            files: vec![],
            lines: vec!["*** Add File: n.txt", "+one", "*** Add File: n.txt", "+two"],
            want: vec![],
            want_err: Some("n.txt"),
        },
        Case {
            name: "update after delete",
            files: vec![("x.txt", "old\n")],
            lines: vec![
                "*** Delete File: x.txt",
                "*** Update File: x.txt",
                "@@",
                "-old",
                "+new",
            ],
            want: vec![],
            want_err: Some("x.txt"),
        },
        Case {
            name: "delete twice",
            files: vec![("x.txt", "old\n")],
            lines: vec!["*** Delete File: x.txt", "*** Delete File: x.txt"],
            want: vec![],
            want_err: Some("x.txt"),
        },
        Case {
            name: "update the source again after a move",
            files: vec![("a.txt", "a\nb\n")],
            lines: vec![
                "*** Update File: a.txt",
                "*** Move to: b.txt",
                "@@",
                "-a",
                "+A",
                "*** Update File: a.txt",
                "@@",
                "-b",
                "+B",
            ],
            want: vec![],
            want_err: Some("a.txt"),
        },
    ];

    for case in cases {
        let dir = tempfile::tempdir().unwrap();
        for (p, content) in &case.files {
            write_file(&dir.path().join(p), content.as_bytes());
        }
        let text = patch_text(&case.lines);
        let result = apply_text(dir.path(), &text);
        if let Some(want_err) = case.want_err {
            let err = result.expect_err("plan should fail");
            assert!(
                err.to_string().contains(want_err),
                "{}: error {:?} should name {}",
                case.name,
                err.to_string(),
                want_err
            );
            for (p, content) in &case.files {
                assert_eq!(
                    read_file(&dir.path().join(p)),
                    bytes(content),
                    "{}: {} should be untouched",
                    case.name,
                    p
                );
            }
        } else {
            result.unwrap_or_else(|e| panic!("{}: apply failed: {e}", case.name));
            for &(p, content) in &case.want {
                let path = dir.path().join(p);
                if content.is_empty() {
                    assert!(!path.exists(), "{}: {} should be gone", case.name, p);
                } else {
                    assert_eq!(
                        read_file(&path),
                        bytes(content),
                        "{}: {} content",
                        case.name,
                        p
                    );
                }
            }
        }
    }
}

#[test]
fn header_position_selects_the_second_block() {
    let dir = tempfile::tempdir().unwrap();
    write_file(&dir.path().join("f.txt"), b"dup\nbody\ndup\nbody\n");
    apply_text(
        dir.path(),
        &patch_text(&["*** Update File: f.txt", "@@ dup", " dup", "-body", "+BODY"]),
    )
    .unwrap();
    assert_eq!(
        read_file(&dir.path().join("f.txt")),
        bytes("dup\nbody\ndup\nBODY\n")
    );
}

#[test]
fn move_onto_a_planned_path_is_rejected() {
    let cases: Vec<(&str, String)> = vec![
        (
            "add then move onto it",
            patch_text(&[
                "*** Add File: b.txt",
                "+added",
                "*** Update File: a.txt",
                "*** Move to: b.txt",
                "@@",
                "-a",
                "+A",
            ]),
        ),
        (
            "two moves to one target",
            patch_text(&[
                "*** Update File: a.txt",
                "*** Move to: b.txt",
                "@@",
                "-a",
                "+A",
                "*** Update File: c.txt",
                "*** Move to: b.txt",
                "@@",
                "-c",
                "+C",
            ]),
        ),
    ];
    for (name, text) in cases {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("a.txt"), b"a\n");
        write_file(&dir.path().join("c.txt"), b"c\n");
        let err = apply_text(dir.path(), &text).expect_err("should be rejected");
        assert!(err.to_string().contains("b.txt"), "{name}: {err}");
        assert_eq!(read_file(&dir.path().join("a.txt")), bytes("a\n"), "{name}");
        assert!(!dir.path().join("b.txt").exists(), "{name}: b.txt exists");
    }
}

#[test]
fn commit_failure_leaves_every_file_untouched() {
    fn remove_b(cwd: &Path) {
        fs::remove_file(cwd.join("b.txt")).unwrap();
    }
    fn make_m_dir(cwd: &Path) {
        fs::create_dir(cwd.join("m.txt")).unwrap();
    }

    struct Case {
        name: &'static str,
        lines: Vec<&'static str>,
        disturb: fn(&Path),
        check_b: bool,
    }

    let cases = vec![
        Case {
            name: "file deleted after planning",
            lines: vec![
                "*** Update File: a.txt",
                "@@",
                "-one",
                "+ONE",
                "*** Add File: deep/er/n.txt",
                "+new",
                "*** Delete File: b.txt",
            ],
            disturb: remove_b,
            check_b: false,
        },
        Case {
            name: "target became a directory",
            lines: vec![
                "*** Update File: a.txt",
                "@@",
                "-one",
                "+ONE",
                "*** Add File: deep/er/n.txt",
                "+new",
                "*** Update File: b.txt",
                "*** Move to: m.txt",
                "@@",
                "-bee",
                "+BEE",
            ],
            disturb: make_m_dir,
            check_b: true,
        },
    ];

    for Case {
        name,
        lines,
        disturb,
        check_b,
    } in cases
    {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        write_file(&a, b"one\ntwo\n");
        write_file(&dir.path().join("b.txt"), b"bee\n");
        set_mode(&a, 0o600);
        let changes = plan_only(dir.path(), &patch_text(&lines));
        disturb(dir.path());
        assert!(
            commit_fs(&OS, &changes).is_err(),
            "{name}: disturbed step should fail"
        );
        assert_eq!(read_file(&a), bytes("one\ntwo\n"), "{name}: a.txt restored");
        #[cfg(unix)]
        assert_eq!(get_mode(&a), 0o600, "{name}: a.txt mode preserved");
        assert!(
            !dir.path().join("deep").exists(),
            "{name}: deep/ created by failed commit still exists"
        );
        if check_b {
            assert_eq!(
                read_file(&dir.path().join("b.txt")),
                bytes("bee\n"),
                "{name}: b.txt restored"
            );
        }
    }
}

#[test]
fn rollback_failure_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let orig = dir.path().join("orig.txt");
    write_file(&orig, b"orig\n");
    let trigger = dir.path().join("trigger.txt");

    let changes = vec![
        Change {
            kind: ChangeKind::Update,
            path: orig.clone(),
            move_to: None,
            new_content: Some(bytes("NEW\n")),
            diff: String::new(),
        },
        Change {
            kind: ChangeKind::Delete,
            path: trigger.clone(),
            move_to: None,
            new_content: None,
            diff: String::new(),
        },
    ];
    let fs = TestFs::fail_second_write(orig.clone()).with_lstat_fail(trigger, "write failed");
    let err = commit_fs(&fs, &changes).expect_err("commit should fail");
    let msg = err.to_string();
    assert!(msg.contains("rollback failed"), "{msg}");
    assert!(msg.contains("orig.txt"), "{msg}");
    assert!(msg.contains("write failed"), "{msg}");
}

#[test]
fn delete_unreadable_file_and_its_rollback() {
    if running_as_root() {
        return; // root reads mode-000 files
    }
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked.txt");
    write_file(&locked, b"secret\n");
    let keep = dir.path().join("keep.txt");
    write_file(&keep, b"keep\n");
    set_mode(&locked, 0o000);

    let changes = plan_only(
        dir.path(),
        &patch_text(&["*** Delete File: locked.txt", "*** Delete File: keep.txt"]),
    );
    fs::remove_file(&keep).unwrap(); // makes the second step fail
    assert!(
        commit_fs(&OS, &changes).is_err(),
        "second delete should fail"
    );
    #[cfg(unix)]
    assert_eq!(
        get_mode(&locked),
        0o000,
        "locked.txt restored with mode 000"
    );

    let changes = plan_only(dir.path(), &patch_text(&["*** Delete File: locked.txt"]));
    commit_fs(&OS, &changes).expect("unreadable file should be deleted");
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        0,
        "no set-aside copy left behind"
    );
    // Go's cleanup is `_ = os.Chmod(locked, 0o600)`: by now locked.txt is gone,
    // so the restore is a best-effort no-op and its error is intentionally
    // ignored. The mode-000 file no longer exists for tempdir cleanup.
}

// ---------------------------------------------------------------------------
// apply_test.go
// ---------------------------------------------------------------------------

#[test]
fn apply_update_add_delete_move() {
    let dir = tempfile::tempdir().unwrap();
    write_file(&dir.path().join("a.txt"), b"one\ntwo\nthree\n");
    write_file(&dir.path().join("gone.txt"), b"bye\n");
    write_file(&dir.path().join("m.txt"), b"move me\n");

    let text = [
        "*** Begin Patch",
        "*** Update File: a.txt",
        "@@",
        " one",
        "-two",
        "+TWO",
        " three",
        "*** Add File: dir/new.txt",
        "+fresh",
        "*** Delete File: gone.txt",
        "*** Update File: m.txt",
        "*** Move to: moved/m.txt",
        "-move me",
        "+moved",
        "*** End Patch",
    ]
    .join("\n");
    let changes = apply_text(dir.path(), &text).unwrap();

    assert_eq!(
        read_file(&dir.path().join("a.txt")),
        bytes("one\nTWO\nthree\n")
    );
    assert_eq!(
        read_file(&dir.path().join("dir").join("new.txt")),
        bytes("fresh\n")
    );
    assert!(!dir.path().join("gone.txt").exists());
    assert!(!dir.path().join("m.txt").exists());
    assert_eq!(
        read_file(&dir.path().join("moved").join("m.txt")),
        bytes("moved\n")
    );
    assert_eq!(
        changes[0].diff,
        "@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n"
    );

    let msg = summary(&changes);
    let want = vec![
        "Success. Updated the following files:".to_string(),
        format!("M {}", dir.path().join("a.txt").display()),
        format!("A {}", dir.path().join("dir").join("new.txt").display()),
        format!("D {}", dir.path().join("gone.txt").display()),
        format!("M {}", dir.path().join("moved").join("m.txt").display()),
    ];
    for w in want {
        assert!(msg.contains(&w), "summary {msg:?} lacks {w:?}");
    }
}

#[test]
fn apply_fuzzy_whitespace_and_header() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f.go");
    write_file(&f, b"func a() {\n\tx := 1   \n}\nfunc b() {\n\tx := 1\n}\n");
    apply_text(
        dir.path(),
        "*** Begin Patch\n*** Update File: f.go\n@@ func b() {\n-\tx := 1\n+\tx := 2\n*** End Patch",
    )
    .unwrap();
    assert_eq!(
        read_file(&f),
        bytes("func a() {\n\tx := 1   \n}\nfunc b() {\n\tx := 2\n}\n")
    );
    apply_text(
        dir.path(),
        "*** Begin Patch\n*** Update File: f.go\n-  x := 1\n+\tx := 9\n*** End Patch",
    )
    .unwrap();
    assert!(read_file(&f).starts_with(b"func a() {\n\tx := 9\n"));
}

#[test]
fn apply_end_of_file_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f.txt");
    write_file(&f, b"x\ny\nx\n");
    apply_text(
        dir.path(),
        "*** Begin Patch\n*** Update File: f.txt\n-x\n+z\n*** End of File\n*** End Patch",
    )
    .unwrap();
    assert_eq!(read_file(&f), bytes("x\ny\nz\n"));
}

#[test]
fn apply_preserves_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f.txt");
    write_file(&f, b"  a\r\nb\r\n  a\r\n");
    apply_text(
        dir.path(),
        "*** Begin Patch\n*** Update File: f.txt\n@@\n-  a\n+c\n*** End of File\n*** End Patch",
    )
    .unwrap();
    assert_eq!(read_file(&f), bytes("  a\r\nb\r\nc\r\n"));
}

#[test]
fn apply_preserves_missing_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f.txt");
    write_file(&f, b"a\nb");
    apply_text(
        dir.path(),
        "*** Begin Patch\n*** Update File: f.txt\n-b\n+c\n*** End Patch",
    )
    .unwrap();
    assert_eq!(read_file(&f), bytes("a\nc"));
}

#[test]
fn plan_failure_writes_nothing() {
    let cases: Vec<(&str, &str)> = vec![
        (
            "chunk not found",
            "*** Begin Patch\n*** Add File: new.txt\n+x\n*** Update File: a.txt\n-missing\n+y\n*** End Patch",
        ),
        (
            "add existing",
            "*** Begin Patch\n*** Add File: exists.txt\n+x\n*** End Patch",
        ),
        (
            "update missing",
            "*** Begin Patch\n*** Update File: nope.txt\n-x\n+y\n*** End Patch",
        ),
        (
            "delete missing",
            "*** Begin Patch\n*** Delete File: nope.txt\n*** End Patch",
        ),
        (
            "add under file",
            "*** Begin Patch\n*** Update File: a.txt\n-one\n+ONE\n*** Add File: plain/child.txt\n+x\n*** End Patch",
        ),
        (
            "move under file",
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: plain/a.txt\n-one\n+ONE\n*** End Patch",
        ),
        (
            "move onto dir",
            "*** Begin Patch\n*** Update File: a.txt\n*** Move to: adir\n-one\n+ONE\n*** End Patch",
        ),
    ];
    for (name, text) in cases {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("a.txt"), b"one\n");
        write_file(&dir.path().join("exists.txt"), b"here\n");
        write_file(&dir.path().join("plain"), b"not a dir\n");
        fs::create_dir(dir.path().join("adir")).unwrap();
        let patch = parse_in(dir.path(), text);
        assert!(plan_fs(&OS, &patch).is_err(), "{name}: plan should fail");
        assert!(
            !dir.path().join("new.txt").exists(),
            "{name}: new.txt written despite plan failure"
        );
        assert_eq!(
            read_file(&dir.path().join("a.txt")),
            bytes("one\n"),
            "{name}: a.txt modified"
        );
    }
}

// ---------------------------------------------------------------------------
// Differential: exact Diff/content for pure-insertion chunks and the offset
// header of a second replacement (goldens captured from Go applyChunks).
// ---------------------------------------------------------------------------

#[test]
fn apply_chunks_diff_matches_go() {
    let cases: Vec<(&str, &str, &str, &str)> = vec![
        (
            "insert",
            "*** Begin Patch\n*** Update File: f.txt\n@@\n+X\n+Y\n*** End Patch",
            "a\nb\nc\nd\nX\nY\n",
            "@@ -5,0 +5,2 @@\n+X\n+Y\n",
        ),
        (
            "multi",
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-a\n+A\n+EXTRA\n@@\n-c\n+C\n*** End Patch",
            "A\nEXTRA\nb\nC\nd\n",
            "@@ -1,1 +1,2 @@\n-a\n+A\n+EXTRA\n@@ -3,1 +4,1 @@\n-c\n+C\n",
        ),
        (
            "header_insert",
            "*** Begin Patch\n*** Update File: f.txt\n@@ b\n+b2\n*** End Patch",
            "a\nb\nb2\nc\nd\n",
            "@@ -3,0 +3,1 @@\n+b2\n",
        ),
    ];
    for (name, text, want_content, want_diff) in cases {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        write_file(&f, b"a\nb\nc\nd\n");
        let changes = apply_text(dir.path(), text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(read_file(&f), bytes(want_content), "{name}: content");
        assert_eq!(changes[0].diff, want_diff, "{name}: diff");
    }
}

// ---------------------------------------------------------------------------
// Extra: a failure in the middle of a multi-file commit restores every
// original file and removes created directories.
// ---------------------------------------------------------------------------

#[test]
fn mid_commit_failure_restores_everything() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    let c = dir.path().join("c.txt");
    let d = dir.path().join("newdir").join("d.txt");
    write_file(&a, b"A\n");
    write_file(&b, b"B\n");
    write_file(&c, b"C\n");

    let changes = vec![
        Change {
            kind: ChangeKind::Update,
            path: a.clone(),
            move_to: None,
            new_content: Some(bytes("A2\n")),
            diff: String::new(),
        },
        Change {
            kind: ChangeKind::Add,
            path: d.clone(),
            move_to: None,
            new_content: Some(bytes("D\n")),
            diff: String::new(),
        },
        Change {
            kind: ChangeKind::Update,
            path: b.clone(),
            move_to: None,
            new_content: Some(bytes("B2\n")),
            diff: String::new(),
        },
        Change {
            kind: ChangeKind::Update,
            path: c.clone(),
            move_to: None,
            new_content: Some(bytes("C2\n")),
            diff: String::new(),
        },
    ];
    let fs = TestFs::fail_nth_write(4);
    assert!(commit_fs(&fs, &changes).is_err(), "commit should fail");

    assert_eq!(read_file(&a), bytes("A\n"));
    assert_eq!(read_file(&b), bytes("B\n"));
    assert_eq!(read_file(&c), bytes("C\n"));
    assert!(!dir.path().join("newdir").exists(), "created dir removed");
    assert!(!d.exists(), "created file removed");
}

// ---------------------------------------------------------------------------
// Fault-injecting test FS. Wraps OS and injects failures so the rollback
// paths can be exercised through the public commit_fs API (the Go tests reach
// into private commit internals instead).
// ---------------------------------------------------------------------------

struct TestFs {
    inner: OS,
    fail_write_nth: usize,
    write_count: Cell<usize>,
    restore_fail_path: Option<PathBuf>,
    restore_write_count: Cell<usize>,
    lstat_fail: Option<(PathBuf, &'static str)>,
}

impl TestFs {
    fn new() -> Self {
        TestFs {
            inner: OS,
            fail_write_nth: 0,
            write_count: Cell::new(0),
            restore_fail_path: None,
            restore_write_count: Cell::new(0),
            lstat_fail: None,
        }
    }

    fn fail_nth_write(n: usize) -> Self {
        TestFs {
            fail_write_nth: n,
            ..Self::new()
        }
    }

    fn fail_second_write(path: PathBuf) -> Self {
        TestFs {
            restore_fail_path: Some(path),
            ..Self::new()
        }
    }

    fn with_lstat_fail(mut self, path: PathBuf, msg: &'static str) -> Self {
        self.lstat_fail = Some((path, msg));
        self
    }
}

impl FS for TestFs {
    fn stat(&self, path: &Path) -> io::Result<Metadata> {
        self.inner.stat(path)
    }

    fn lstat(&self, path: &Path) -> io::Result<Metadata> {
        if let Some((ref p, msg)) = self.lstat_fail
            && path == p.as_path()
        {
            return Err(io::Error::other(msg.to_string()));
        }
        self.inner.lstat(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }

    fn readlink(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.readlink(path)
    }

    fn mkdir_all(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.inner.mkdir_all(path, mode)
    }

    fn write(&self, path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
        let n = self.write_count.get() + 1;
        self.write_count.set(n);
        if n == self.fail_write_nth {
            return Err(io::Error::other(format!("injected write failure #{n}")));
        }
        if let Some(ref p) = self.restore_fail_path
            && path == p.as_path()
        {
            let c = self.restore_write_count.get() + 1;
            self.restore_write_count.set(c);
            if c == 2 {
                return Err(io::Error::other(format!(
                    "cannot restore {}",
                    path.display()
                )));
            }
        }
        self.inner.write(path, data, mode)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.inner.remove(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        self.inner.symlink(target, link)
    }

    fn chmod(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.inner.chmod(path, mode)
    }
}
