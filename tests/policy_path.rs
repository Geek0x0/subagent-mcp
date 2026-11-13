//! Policy integration tests.
//!
//! Task 14 covers the shell lexer and allowlist. Task 15 extends this file with
//! the decision matrix and path-confinement cases.

use std::path::{Path, PathBuf};

use subagent_mcp::policy::lexer::analyze_shell;
use subagent_mcp::policy::path::{bound_path, evaluate, evaluate_with_reason, inside_relative};
use subagent_mcp::policy::{ApprovalPolicy, Decision, PolicyRequest, Sandbox, ToolKind};
#[cfg(unix)]
use subagent_mcp::util::fs::same_file;

fn allowed(command: &str) -> bool {
    analyze_shell(command).is_ok_and(|analysis| analysis.allowed)
}

fn denial(command: &str) -> String {
    let analysis = analyze_shell(command).expect("command should lex");
    assert!(
        !analysis.allowed,
        "analyze_shell({command:?}) unexpectedly allowed"
    );
    analysis.reason.expect("denied commands carry a reason")
}

fn lex_error(command: &str) -> String {
    analyze_shell(command)
        .expect_err("command should be rejected by the lexer")
        .to_string()
}

#[test]
fn allowlist_matches_go_table() {
    let cases = [
        ("ls -la", true),
        ("cat go.mod", true),
        ("rg -n foo internal", true),
        ("git status", true),
        ("git diff --stat", true),
        ("git log -5", true),
        ("git show HEAD", true),
        ("git branch", true),
        ("git branch -a", true),
        ("git branch -v", true),
        ("git branch --show-current", true),
        ("grep -r x .", true),
        ("find . -name '*.go'", true),
        ("pwd", true),
        ("head -20 f", true),
        ("tail f", true),
        ("wc -l f", true),
        ("echo hi", true),
        ("cat a | grep b", true),
        ("ls; pwd", true),
        ("rm -rf /", false),
        ("go test ./...", false),
        ("cat f > out", false),
        ("echo $(rm x)", false),
        ("echo `rm x`", false),
        ("ls && rm x", false),
        ("cat f | tee out", false),
        ("git push", false),
        ("FOO=1 ls", false),
        ("find . -delete", false),
        ("find . -exec rm -rf {} +", false),
        ("rg --pre ./evil.sh foo .", false),
        ("git diff --output=/tmp/x HEAD", false),
        ("git branch -D main", false),
        ("git branch -m old new", false),
        ("git branch new-branch", false),
        ("git branch -f main HEAD~5", false),
        ("", false),
    ];

    for (command, want) in cases {
        assert_eq!(allowed(command), want, "analyze_shell({command:?}).allowed");
    }
}

#[test]
fn legitimate_commands_are_allowed() {
    let commands = [
        "ls -la",
        "ls internal/",
        r#"rg "pattern" internal/"#,
        r#"rg -n 'func Test' --glob '*.go'"#,
        "rg -i -n pattern internal/",
        "rg pattern archive.gz",
        r#"grep -rn "foo bar" ."#,
        "cat 'file with space.txt'",
        "head -n 20 a.go",
        "find . -name '*.go'",
        r#"find . -type f -name "*_test.go""#,
        "wc -l a b",
        "ls *.go",
        "wc -l *.go",
        "cat docs/*.md",
        "ls ~",
        "ls {a,b}",
        "echo [ab]*",
        "ls *.go | rg -n foo",
        "ls *.go | find",
        "find | ls *.go",
        "git status",
        "git log --oneline -5",
        "git diff --stat",
        "git rev-parse HEAD",
        "git branch -a",
        "rg foo | head",
        r#"rg "a;b" ."#,
        r#"echo 'a;b'; pwd"#,
    ];

    for command in commands {
        assert!(allowed(command), "analyze_shell({command:?}) should allow");
    }
}

#[test]
fn adversarial_commands_are_denied() {
    let commands = [
        "find . '-delete'",
        r#"find . "-delete""#,
        "find . -de''lete",
        r#"rg '--pre=sh' x ."#,
        r#"grep '--pre=sh' x ."#,
        r#"echo ${x:=$'\x24(touch pwned)'}; echo ${x@P}"#,
        "cat /proc/$PPID/environ",
        "echo $HOME",
        "ls $(pwd)",
        "ls `pwd`",
        "cat <(ls)",
        "cat >(ls)",
        "ls > out",
        "ls < in",
        "cd internal",
        "git diff --stat *",
        "find *",
        "find . -name x ~",
        "rg foo *.go",
        "grep -r foo {a,b}",
        "git log ~",
        "ls *.go | rg -n foo *",
        "ls (x)",
        "ls && rm x",
        "ls\nrm -rf x",
        "git -c core.pager=x log",
        "git --exec-path",
        "env FOO=1 ls",
        "rg --pre=sh x",
        "rg --pre x",
        "rg --pre-glob x",
        "rg --hostname-bin x",
        "rg -z x archive.gz",
        "rg --search-zip x archive.gz",
        "rg -iz x archive.gz",
        "grep --pre=sh x",
        "grep --pre x",
        "grep --pre-glob x",
        "grep --hostname-bin x",
        "git diff --output=x",
        "git diff --output x",
        "git diff -o x",
        "git show --output=x",
        "git show --output x",
        "git log --output=x",
        "git log --output x",
        "git log --show-signature",
        "git diff --ext-diff",
        "git show --textconv",
        "find . -fprint0 output",
        "find . -okdir echo file \\;",
        "find . -ok echo file \\;",
        "find . -fls output",
        "find . -fprintf output %p",
    ];

    for command in commands {
        assert!(!allowed(command), "analyze_shell({command:?}) should deny");
    }
}

#[test]
fn lexer_splits_segments_and_words() {
    let analysis = analyze_shell("cat 'file with space.txt' | wc -l").unwrap();
    assert!(analysis.allowed);
    assert_eq!(
        analysis.segments,
        vec![
            vec!["cat".to_string(), "file with space.txt".to_string()],
            vec!["wc".to_string(), "-l".to_string()],
        ]
    );

    let analysis = analyze_shell("ls; pwd").unwrap();
    assert_eq!(
        analysis.segments,
        vec![vec!["ls".to_string()], vec!["pwd".to_string()]]
    );

    let analysis = analyze_shell("cat café").unwrap();
    assert_eq!(
        analysis.segments,
        vec![vec!["cat".to_string(), "café".to_string()]]
    );
}

#[test]
fn quoting_and_backslash_remove_shell_syntax() {
    assert_eq!(
        analyze_shell(r#"cat 'a b' "c d""#).unwrap().segments,
        vec![vec![
            "cat".to_string(),
            "a b".to_string(),
            "c d".to_string()
        ]]
    );
    assert_eq!(
        analyze_shell(r#"cat a"b"'c'"#).unwrap().segments,
        vec![vec!["cat".to_string(), "abc".to_string()]]
    );
    assert_eq!(
        analyze_shell(r"cat file\ with\ space").unwrap().segments,
        vec![vec!["cat".to_string(), "file with space".to_string()]]
    );
    assert_eq!(
        analyze_shell("cat a\\\nb").unwrap().segments,
        vec![vec!["cat".to_string(), "ab".to_string()]]
    );
    assert_eq!(
        analyze_shell("echo \"a\\\"b\"").unwrap().segments,
        vec![vec!["echo".to_string(), "a\"b".to_string()]]
    );

    // Quoted and escaped metacharacters are ordinary data.
    assert!(allowed(r#"echo '$HOME'"#));
    assert!(allowed(r#"echo \$HOME"#));
    assert!(allowed(r#"echo '<'"#));
    assert!(allowed(r#"echo '>'"#));
    assert!(allowed(r#"echo '('"#));
    assert!(allowed(r#"echo '`'"#));
    assert!(allowed(r#"cat a\;b"#));
    assert!(allowed(r#"echo 'a;b'; pwd"#));
    assert!(allowed(r#"rg "a;b" ."#));
}

#[test]
fn separators_newlines_and_comments_match_go() {
    for command in [
        "ls; pwd",
        "ls;pwd",
        "ls & pwd",
        "ls &",
        "ls && pwd",
        "ls | pwd",
        "ls || pwd",
        "ls\npwd",
        "ls\n\npwd",
        "ls # trailing comment",
        "ls # comment\npwd",
        "# leading comment\nls",
        "ls; ; pwd",
        "ls\n# comment\npwd",
    ] {
        assert!(allowed(command), "analyze_shell({command:?}) should allow");
    }

    for command in [
        "&& ls",
        "& ls",
        "| ls",
        "ls &&",
        "ls ||",
        "ls |",
        "; ls",
        "ls &&\npwd",
        "ls |\npwd",
        "ls | # comment\npwd",
        "ls # comment\nrm x",
    ] {
        assert!(!allowed(command), "analyze_shell({command:?}) should deny");
    }
}

#[test]
fn expansion_policy_is_per_command() {
    for command in [
        "ls *.go",
        "cat docs/*.md",
        "head *.go",
        "tail *.go",
        "wc -l *.go",
        "stat *.go",
        "which *.go",
        "echo [ab]*",
        "pwd *.go",
        "ls ~",
        "ls {a,b}",
        "echo x=~",
        "cat HEAD~1",
        "find \\*.go",
        "git log \\~",
        "git log '~'",
        "git log HEAD~1",
    ] {
        assert!(allowed(command), "analyze_shell({command:?}) should allow");
    }

    for command in [
        "find *",
        "find . -name x ~",
        "rg foo *.go",
        "grep -r foo {a,b}",
        "git log ~",
        "git diff --stat *",
        "git log x=~",
        "ls *.go | rg -n foo *",
    ] {
        assert!(!allowed(command), "analyze_shell({command:?}) should deny");
    }
}

#[test]
fn git_subcommands_and_flags_match_go() {
    for command in [
        "git status",
        "git diff",
        "git log",
        "git show",
        "git branch",
        "git blame file",
        "git rev-parse HEAD",
        "git ls-files",
        "git branch -a",
        "git branch -r",
        "git branch -v",
        "git branch -vv",
        "git branch --all",
        "git branch --remotes",
        "git branch --verbose",
        "git branch -l",
        "git branch --list",
        "git branch --show-current",
        "git branch --sort=-committerdate",
        "git branch --format='%(refname)'",
        "git branch --color",
        "git branch --no-color",
        "git log --oneline -5",
        "git show --stat HEAD",
    ] {
        assert!(allowed(command), "analyze_shell({command:?}) should allow");
    }

    for command in [
        "git push",
        "git commit",
        "git -c core.pager=x log",
        "git --exec-path",
        "git log --recurse-submodules",
        "git log --show-signature",
        "git log --verify-signatures",
        "git log --submodule",
        "git log --submodule=short",
        "git log --format=%G?",
        "git show --pretty=%G",
        "git blame --ext-diff file",
        "git blame --textconv file",
        "git diff --output=x",
        "git diff --output x",
        "git diff -o x",
        "git log -output=x",
        "git log --ext-diff",
        "git show --textconv",
        "git branch -D main",
        "git branch -m old new",
        "git branch new-branch",
        "git branch -f main HEAD~5",
        "git branch --contains main",
        "git branch --merged main",
        "git branch --no-merged main",
        "git branch --contains=main",
        "git branch --merged=main",
        "git branch --no-merged=main",
        "git branch --sort -committerdate",
        "git branch --format '%(refname)'",
        "git branch --color=always",
    ] {
        assert!(!allowed(command), "analyze_shell({command:?}) should deny");
    }
}

#[test]
fn rg_and_grep_dangerous_flags_match_go() {
    for command in [
        "rg --pre=sh x",
        "rg --pre x",
        "rg --pre-glob x",
        "rg --pre-glob=*.go x",
        "rg --hostname-bin x",
        "rg --hostname-bin=host x",
        "rg -z x",
        "rg -iz x archive.gz",
        "rg --search-zip x archive.gz",
        "grep --pre=sh x",
        "grep --pre x",
        "grep --pre-glob x",
        "grep --hostname-bin x",
        "grep --pre-glob=*.go x",
        "grep --hostname-bin=host x",
    ] {
        assert!(!allowed(command), "analyze_shell({command:?}) should deny");
    }

    // `-z` is only dangerous for ripgrep; grep has no such flag.
    assert!(allowed("grep -z x"));
}

#[test]
fn find_dangerous_flags_match_go() {
    for flag in [
        "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprintf", "-fls", "-fprint", "-fprint0",
    ] {
        let command = format!("find . {flag} output");
        assert!(!allowed(&command), "analyze_shell({command:?}) should deny");
    }

    for command in [
        "find . '-delete'",
        r#"find . "-delete""#,
        "find . -de''lete",
    ] {
        assert!(!allowed(command), "analyze_shell({command:?}) should deny");
    }
}

#[test]
fn allowlist_denial_reasons_are_exact() {
    assert_eq!(denial("rm -rf /"), "command \"rm\" is not allowlisted");
    assert_eq!(
        denial("find *"),
        "unquoted expansion is not allowed for command \"find\""
    );
    assert_eq!(
        denial("find . -delete"),
        "flag \"-delete\" is not allowed for command \"find\""
    );
    assert_eq!(
        denial("rg --pre ./evil.sh foo ."),
        "flag \"--pre\" is not allowed for command \"rg\""
    );
    assert_eq!(
        denial("rg -iz x archive.gz"),
        "flag \"-iz\" is not allowed for command \"rg\""
    );
    assert_eq!(
        denial("git diff --output=/tmp/x HEAD"),
        "flag \"--output=/tmp/x\" is not allowed for command \"git\""
    );
    assert_eq!(
        denial("git log --format=%G"),
        "flag \"--format=%G\" is not allowed for command \"git\""
    );
    assert_eq!(
        denial("git branch new-branch"),
        "argument \"new-branch\" is not allowed for command \"git branch\""
    );
}

#[test]
fn lexer_error_reasons_are_exact() {
    assert_eq!(lex_error(""), "empty command");
    assert_eq!(lex_error("   "), "empty command");
    assert_eq!(lex_error("# only a comment"), "empty command");
    assert_eq!(lex_error("cat 'unterminated"), "unterminated single quote");
    assert_eq!(lex_error("cat \"unterminated"), "unterminated double quote");
    assert_eq!(lex_error("cat \\"), "trailing backslash");
    assert_eq!(lex_error("echo \"abc\\"), "trailing backslash");
    assert_eq!(
        lex_error("echo $HOME"),
        "parameter expansion or command substitution is not allowed"
    );
    assert_eq!(
        lex_error("echo \"$HOME\""),
        "parameter expansion or command substitution is not allowed"
    );
    assert_eq!(
        lex_error("echo `id`"),
        "command substitution is not allowed"
    );
    assert_eq!(lex_error("ls > out"), "redirection is not allowed");
    assert_eq!(lex_error("cat <<EOF"), "redirection is not allowed");
    assert_eq!(lex_error("cat <(ls)"), "redirection is not allowed");
    assert_eq!(lex_error("ls (x)"), "parentheses are not allowed");
    assert_eq!(lex_error("&& ls"), "empty command before separator");
    assert_eq!(lex_error("| ls"), "empty command before separator");
    assert_eq!(lex_error("; ls"), "empty command before separator");
    assert_eq!(lex_error("ls &&"), "empty command after separator");
    assert_eq!(lex_error("ls ||"), "empty command after separator");
    assert_eq!(lex_error("ls |"), "empty command after separator");
}

fn request(
    tool: ToolKind,
    command: Option<&str>,
    path: Option<&Path>,
    cwd: &Path,
) -> PolicyRequest {
    PolicyRequest {
        tool,
        command: command.map(str::to_string),
        path: path.map(|path| path.to_string_lossy().into_owned()),
        cwd: cwd.to_path_buf(),
        bound_cwd: None,
    }
}

struct MatrixCase {
    name: &'static str,
    sandbox: Sandbox,
    approval: ApprovalPolicy,
    tool: ToolKind,
    command: Option<&'static str>,
    path: Option<PathBuf>,
    want: Decision,
}

#[test]
fn decision_matrix_matches_go() {
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path();
    let inside = cwd.join("inside.txt");
    std::fs::write(&inside, b"inside").unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path().join("outside.txt");
    std::fs::write(&outside, b"outside").unwrap();

    // Every case from Go TestEvaluate. The shared Rust enums have no invalid
    // variant, so Go's "unknown approval policy" row cannot be represented;
    // the exhaustive match in `evaluate` fails closed by construction.
    let cases = [
        MatrixCase {
            name: "read-only on-request read file",
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::OnRequest,
            tool: ToolKind::ReadFile,
            command: None,
            path: Some(inside.clone()),
            want: Decision::Allow,
        },
        MatrixCase {
            name: "read-only on-request whitelisted shell",
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::OnRequest,
            tool: ToolKind::Shell,
            command: Some("ls"),
            path: None,
            want: Decision::Allow,
        },
        MatrixCase {
            name: "read-only on-request non-whitelisted shell",
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::OnRequest,
            tool: ToolKind::Shell,
            command: Some("rm x"),
            path: None,
            want: Decision::AskApproval,
        },
        MatrixCase {
            name: "read-only on-request write inside cwd",
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::OnRequest,
            tool: ToolKind::WriteFile,
            command: None,
            path: Some(inside.clone()),
            want: Decision::AskApproval,
        },
        MatrixCase {
            name: "read-only never non-whitelisted shell",
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::Never,
            tool: ToolKind::Shell,
            command: Some("rm x"),
            path: None,
            want: Decision::Deny,
        },
        MatrixCase {
            name: "read-only never write file",
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::Never,
            tool: ToolKind::WriteFile,
            command: None,
            path: Some(inside.clone()),
            want: Decision::Deny,
        },
        MatrixCase {
            name: "read-only untrusted whitelisted shell",
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::Untrusted,
            tool: ToolKind::Shell,
            command: Some("ls"),
            path: None,
            want: Decision::Allow,
        },
        MatrixCase {
            name: "read-only untrusted non-whitelisted shell",
            sandbox: Sandbox::ReadOnly,
            approval: ApprovalPolicy::Untrusted,
            tool: ToolKind::Shell,
            command: Some("rm x"),
            path: None,
            want: Decision::AskApproval,
        },
        MatrixCase {
            name: "workspace-write on-request arbitrary shell",
            sandbox: Sandbox::WorkspaceWrite,
            approval: ApprovalPolicy::OnRequest,
            tool: ToolKind::Shell,
            command: Some("rm x"),
            path: None,
            want: Decision::Allow,
        },
        MatrixCase {
            name: "workspace-write on-request write inside cwd",
            sandbox: Sandbox::WorkspaceWrite,
            approval: ApprovalPolicy::OnRequest,
            tool: ToolKind::WriteFile,
            command: None,
            path: Some(inside.clone()),
            want: Decision::Allow,
        },
        MatrixCase {
            name: "workspace-write on-request write outside cwd",
            sandbox: Sandbox::WorkspaceWrite,
            approval: ApprovalPolicy::OnRequest,
            tool: ToolKind::WriteFile,
            command: None,
            path: Some(outside.clone()),
            want: Decision::AskApproval,
        },
        MatrixCase {
            name: "workspace-write never write outside cwd",
            sandbox: Sandbox::WorkspaceWrite,
            approval: ApprovalPolicy::Never,
            tool: ToolKind::WriteFile,
            command: None,
            path: Some(outside.clone()),
            want: Decision::Deny,
        },
        MatrixCase {
            name: "workspace-write untrusted write inside cwd",
            sandbox: Sandbox::WorkspaceWrite,
            approval: ApprovalPolicy::Untrusted,
            tool: ToolKind::WriteFile,
            command: None,
            path: Some(inside.clone()),
            want: Decision::AskApproval,
        },
        MatrixCase {
            name: "workspace-write untrusted read file",
            sandbox: Sandbox::WorkspaceWrite,
            approval: ApprovalPolicy::Untrusted,
            tool: ToolKind::ReadFile,
            command: None,
            path: Some(outside.clone()),
            want: Decision::Allow,
        },
        MatrixCase {
            name: "workspace-write on-failure write outside cwd",
            sandbox: Sandbox::WorkspaceWrite,
            approval: ApprovalPolicy::OnFailure,
            tool: ToolKind::WriteFile,
            command: None,
            path: Some(outside.clone()),
            want: Decision::AskApproval,
        },
        MatrixCase {
            name: "danger-full-access never arbitrary shell",
            sandbox: Sandbox::DangerFullAccess,
            approval: ApprovalPolicy::Never,
            tool: ToolKind::Shell,
            command: Some("rm -rf /tmp/x"),
            path: None,
            want: Decision::Allow,
        },
        MatrixCase {
            name: "danger-full-access untrusted non-whitelisted shell",
            sandbox: Sandbox::DangerFullAccess,
            approval: ApprovalPolicy::Untrusted,
            tool: ToolKind::Shell,
            command: Some("rm x"),
            path: None,
            want: Decision::AskApproval,
        },
    ];

    for case in cases {
        let req = request(case.tool, case.command, case.path.as_deref(), cwd);
        let decision = evaluate(&req, case.sandbox, case.approval, cwd);
        assert_eq!(decision, case.want, "{}", case.name);

        let (detailed, reason) = evaluate_with_reason(&req, case.sandbox, case.approval, cwd);
        assert_eq!(detailed, case.want, "{} (detailed)", case.name);
        if case.want == Decision::Allow {
            assert_eq!(reason, "", "{} returned a reason for Allow", case.name);
        } else {
            assert!(
                !reason.is_empty(),
                "{} returned no reason for {decision:?}",
                case.name
            );
        }
    }
}

#[test]
fn sandbox_and_approval_reasons_are_exact() {
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path();
    let inside = cwd.join("inside.txt");
    std::fs::write(&inside, b"inside").unwrap();

    let req = request(ToolKind::Shell, Some("rm x"), None, cwd);
    let (decision, reason) =
        evaluate_with_reason(&req, Sandbox::ReadOnly, ApprovalPolicy::OnRequest, cwd);
    assert_eq!(decision, Decision::AskApproval);
    assert_eq!(
        reason,
        "operation is not permitted by the read-only sandbox"
    );

    let req = request(ToolKind::Other("custom".into()), None, None, cwd);
    let (decision, reason) = evaluate_with_reason(
        &req,
        Sandbox::WorkspaceWrite,
        ApprovalPolicy::OnRequest,
        cwd,
    );
    assert_eq!(decision, Decision::AskApproval);
    assert_eq!(
        reason,
        "tool \"custom\" is not permitted by the workspace-write sandbox"
    );

    let req = request(ToolKind::WriteFile, None, Some(&inside), cwd);
    let (decision, reason) = evaluate_with_reason(
        &req,
        Sandbox::WorkspaceWrite,
        ApprovalPolicy::Untrusted,
        cwd,
    );
    assert_eq!(decision, Decision::AskApproval);
    assert_eq!(
        reason,
        "untrusted policy requires approval for non-whitelisted operations"
    );

    // A non-whitelisted shell keeps the sandbox reason under `untrusted`.
    let req = request(ToolKind::Shell, Some("rm x"), None, cwd);
    let (decision, reason) =
        evaluate_with_reason(&req, Sandbox::ReadOnly, ApprovalPolicy::Untrusted, cwd);
    assert_eq!(decision, Decision::AskApproval);
    assert_eq!(
        reason,
        "operation is not permitted by the read-only sandbox"
    );
}

#[test]
fn parent_directory_components_are_rejected_before_resolution() {
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path();
    let targets = [
        PathBuf::from(".."),
        PathBuf::from("../outside.txt"),
        PathBuf::from("a/../b"),
        PathBuf::from("inside/../../outside.txt"),
        cwd.join("..").join("outside.txt"),
    ];

    for target in targets {
        let err = inside_relative(cwd, &target).unwrap_err();
        assert!(
            err.to_string()
                .contains("contains a parent-directory component"),
            "inside_relative({target:?}) = {err}"
        );
        let err = bound_path(cwd, &target).unwrap_err();
        assert!(
            err.to_string()
                .contains("contains a parent-directory component"),
            "bound_path({target:?}) = {err}"
        );
    }
}

#[test]
fn missing_tails_are_reattached() {
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path();

    assert_eq!(
        inside_relative(cwd, Path::new("a/b/c/new.txt")).unwrap(),
        PathBuf::from("a/b/c/new.txt")
    );
    assert_eq!(
        inside_relative(cwd, &cwd.join("a/b/c/new.txt")).unwrap(),
        PathBuf::from("a/b/c/new.txt")
    );
    assert_eq!(inside_relative(cwd, cwd).unwrap(), PathBuf::from("."));
}

#[test]
fn absolute_paths_outside_cwd_are_rejected() {
    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path();
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path().join("outside.txt");
    std::fs::write(&outside, b"outside").unwrap();

    let err = inside_relative(cwd, &outside).unwrap_err();
    assert!(
        err.to_string()
            .contains("resolves outside working directory"),
        "{err}"
    );
}

#[cfg(unix)]
#[test]
fn symlink_resolution_matches_go_path_table() {
    use std::os::unix::fs::symlink;

    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path();
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path();

    let inside_file = cwd.join("inside.txt");
    std::fs::write(&inside_file, b"inside").unwrap();

    symlink(outside, cwd.join("outside-directory")).unwrap();
    std::fs::write(
        cwd.join("outside-directory").join("escaped.txt"),
        b"escaped",
    )
    .unwrap();

    let outside_file = outside.join("outside.txt");
    std::fs::write(&outside_file, b"outside").unwrap();
    symlink(&outside_file, cwd.join("outside-file")).unwrap();

    let inside_directory = cwd.join("inside-directory");
    std::fs::create_dir(&inside_directory).unwrap();
    symlink(&inside_directory, cwd.join("inside-directory-link")).unwrap();

    // Relative and absolute spellings of a real file inside cwd.
    assert_eq!(
        inside_relative(cwd, Path::new("inside.txt")).unwrap(),
        PathBuf::from("inside.txt")
    );
    assert_eq!(
        inside_relative(cwd, &inside_file).unwrap(),
        PathBuf::from("inside.txt")
    );

    // A file below a directory symlink that leaves cwd.
    let escaped = cwd.join("outside-directory").join("escaped.txt");
    let err = inside_relative(cwd, &escaped).unwrap_err();
    assert!(
        err.to_string()
            .contains("resolves outside working directory"),
        "{err}"
    );

    // A symlink to a file outside cwd.
    let err = inside_relative(cwd, &cwd.join("outside-file")).unwrap_err();
    assert!(
        err.to_string()
            .contains("resolves outside working directory"),
        "{err}"
    );

    // A symlink followed by a parent-directory component is rejected before
    // resolution, even though the lexical result could land back in cwd.
    let dot_dot_escape = cwd.join("outside-directory").join("..").join("evil");
    let err = inside_relative(cwd, &dot_dot_escape).unwrap_err();
    assert!(
        err.to_string()
            .contains("contains a parent-directory component"),
        "{err}"
    );

    // A missing tail below an inside symlink is resolved through the link.
    let new_file = cwd.join("inside-directory-link").join("newfile.txt");
    assert_eq!(
        inside_relative(cwd, &new_file).unwrap(),
        PathBuf::from("inside-directory/newfile.txt")
    );

    // A missing tail below an outside symlink is re-checked after resolution.
    let escape_tail = cwd
        .join("outside-directory")
        .join("missing")
        .join("new.txt");
    let err = inside_relative(cwd, &escape_tail).unwrap_err();
    assert!(
        err.to_string()
            .contains("resolves outside working directory"),
        "{err}"
    );
}

#[test]
fn trusted_shell_uses_the_lexer_allowed_flag() {
    use subagent_mcp::policy::path::is_whitelisted_shell;

    assert!(is_whitelisted_shell("ls -la"));
    assert!(!is_whitelisted_shell("rm x"));
    // A lexical hazard is not trusted even though it is not an allowlist
    // denial with a reason.
    assert!(!is_whitelisted_shell("echo $HOME"));
    assert!(!is_whitelisted_shell(""));
}

#[test]
fn unbound_bound_path_reports_the_go_error() {
    let err = bound_path(Path::new(""), Path::new("inside.txt")).unwrap_err();
    assert_eq!(err.to_string(), "working directory is not bound");
}

#[test]
fn bound_paths_join_under_the_root() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    std::fs::write(root.join("inside.txt"), b"inside").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();

    assert_eq!(
        bound_path(root, Path::new("inside.txt")).unwrap().path,
        root.join("inside.txt")
    );
    assert_eq!(
        bound_path(root, Path::new("sub/new.txt")).unwrap().path,
        root.join("sub/new.txt")
    );
    assert_eq!(bound_path(root, Path::new(".")).unwrap().path, root);

    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path().join("outside.txt");
    std::fs::write(&outside, b"outside").unwrap();
    assert!(bound_path(root, &outside).is_err());
    assert!(bound_path(root, Path::new("../outside.txt")).is_err());
}

#[cfg(unix)]
#[test]
fn same_file_dedupes_bound_and_linked_paths() {
    use std::os::unix::fs::symlink;

    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path();
    let file = root.join("a.txt");
    std::fs::write(&file, b"a").unwrap();
    symlink(&file, root.join("link.txt")).unwrap();
    std::fs::hard_link(&file, root.join("hard.txt")).unwrap();
    std::fs::write(root.join("b.txt"), b"b").unwrap();

    let bound_file = bound_path(root, Path::new("a.txt")).unwrap();
    let bound_link = bound_path(root, Path::new("link.txt")).unwrap();
    let bound_hard = bound_path(root, Path::new("hard.txt")).unwrap();
    let bound_other = bound_path(root, Path::new("b.txt")).unwrap();

    assert!(same_file(&bound_file.path, &bound_link.path).unwrap());
    assert!(same_file(&bound_file.path, &bound_hard.path).unwrap());
    assert!(!same_file(&bound_file.path, &bound_other.path).unwrap());
}

#[cfg(unix)]
#[test]
fn workspace_write_rejects_symlink_escape_with_inside_lexical_path() {
    use std::os::unix::fs::symlink;

    let cwd_dir = tempfile::tempdir().unwrap();
    let cwd = cwd_dir.path();
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path();
    std::fs::write(outside.join("evil.txt"), b"evil").unwrap();
    symlink(outside, cwd.join("escape")).unwrap();

    // The lexical path is inside cwd, but its parent symlink escapes.
    let lexical_escape = cwd.join("escape").join("evil.txt");
    let req = request(ToolKind::WriteFile, None, Some(&lexical_escape), cwd);
    let (decision, reason) = evaluate_with_reason(
        &req,
        Sandbox::WorkspaceWrite,
        ApprovalPolicy::OnRequest,
        cwd,
    );
    assert_eq!(decision, Decision::AskApproval);
    assert!(
        reason.contains("resolves outside working directory"),
        "{reason}"
    );
    assert_eq!(
        evaluate(&req, Sandbox::WorkspaceWrite, ApprovalPolicy::Never, cwd),
        Decision::Deny
    );

    // A missing tail below the escaping symlink is also rejected.
    let lexical_new = cwd.join("escape").join("new.txt");
    let req_new = request(ToolKind::WriteFile, None, Some(&lexical_new), cwd);
    assert_eq!(
        evaluate(
            &req_new,
            Sandbox::WorkspaceWrite,
            ApprovalPolicy::OnRequest,
            cwd
        ),
        Decision::AskApproval
    );

    // A real file inside cwd is allowed.
    let inside = cwd.join("inside.txt");
    std::fs::write(&inside, b"inside").unwrap();
    let req_inside = request(ToolKind::WriteFile, None, Some(&inside), cwd);
    assert_eq!(
        evaluate(
            &req_inside,
            Sandbox::WorkspaceWrite,
            ApprovalPolicy::OnRequest,
            cwd
        ),
        Decision::Allow
    );

    // Workspace-write containment is measured against the bound root, not the
    // lexical cwd string.
    let mut bound_outside = request(ToolKind::WriteFile, None, Some(&inside), cwd);
    bound_outside.bound_cwd = Some(outside.to_path_buf());
    assert_eq!(
        evaluate(
            &bound_outside,
            Sandbox::WorkspaceWrite,
            ApprovalPolicy::OnRequest,
            cwd
        ),
        Decision::AskApproval
    );

    let mut bound_inside = request(ToolKind::WriteFile, None, Some(&inside), cwd);
    bound_inside.bound_cwd = Some(cwd.to_path_buf());
    assert_eq!(
        evaluate(
            &bound_inside,
            Sandbox::WorkspaceWrite,
            ApprovalPolicy::OnRequest,
            cwd
        ),
        Decision::Allow
    );
}
