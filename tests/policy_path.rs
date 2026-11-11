//! Policy integration tests.
//!
//! Task 14 covers the shell lexer and allowlist. Task 15 extends this file with
//! the decision matrix and path-confinement cases.

use subagent_mcp::policy::lexer::analyze_shell;

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
