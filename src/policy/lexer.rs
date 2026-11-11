//! Shell lexer and command allowlist.
//!
//! Ports Go `internal/policy/shell_lexer.go` and the allowlist half of
//! `internal/policy/policy.go` (subagent-mcp v0.13.1). The lexer performs the
//! part of shell parsing that matters to the allowlist: quote removal,
//! backslash handling and command separators. Redirections, heredocs, process
//! substitution, parameter/command expansion and parentheses are rejected
//! because they either run code or rewrite argv unpredictably.
//!
//! Pathname, brace and tilde expansion only choose file names, so they are
//! recorded per segment and the allowlist decides which commands may take
//! them. Only `ls`, `cat`, `head`, `tail`, `wc`, `stat`, `which`, `echo` and
//! `pwd` may expand; `find`, `rg`, `grep` and `git` have options that run a
//! program, so a filename must never turn into one of their options.

use crate::util::json::go_quote;

/// The result of lexing a command and applying the shell allowlist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellAnalysis {
    /// Simple-command segments in execution order, each a list of words.
    pub segments: Vec<Vec<String>>,
    /// Whether every segment is on the allowlist and free of dangerous flags.
    pub allowed: bool,
    /// The exact denial reason when `allowed` is false.
    pub reason: Option<String>,
}

/// A lexical rejection: the command is not safe to reason about at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexError {
    message: String,
}

impl LexError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The exact denial reason.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LexError {}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    words: Vec<String>,
    /// Set when a word contains an unquoted glob, brace or tilde expansion:
    /// the words bash runs then differ from the words written here.
    expands: bool,
}

// Commands that may take unquoted glob, brace and tilde words. They only read
// the files the expansion names and have no option that runs a program or
// writes.
const EXPANSION_ALLOWED: [&str; 9] = [
    "ls", "cat", "head", "tail", "wc", "stat", "which", "echo", "pwd",
];

const SIMPLE_ALLOWED: [&str; 12] = [
    "ls", "cat", "head", "tail", "rg", "grep", "find", "pwd", "wc", "stat", "which", "echo",
];

const GIT_ALLOWED: [&str; 8] = [
    "status",
    "diff",
    "log",
    "show",
    "branch",
    "blame",
    "rev-parse",
    "ls-files",
];

const FIND_DENIED: [&str; 9] = [
    "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprintf", "-fls", "-fprint", "-fprint0",
];

const GIT_BRANCH_ALLOWED: [&str; 17] = [
    "-a",
    "--all",
    "-r",
    "--remotes",
    "-v",
    "-vv",
    "--verbose",
    "-l",
    "--list",
    "--show-current",
    "--contains",
    "--merged",
    "--no-merged",
    "--sort",
    "--format",
    "--color",
    "--no-color",
];

/// Lex `command` and apply the shell allowlist.
///
/// Lexical rejection is an `Err`; a well-formed command that is simply not on
/// the allowlist is `Ok(ShellAnalysis { allowed: false, .. })` with a reason.
pub fn analyze_shell(command: &str) -> Result<ShellAnalysis, LexError> {
    let segments = lex_shell(command)?;
    let (allowed, reason) = allowlist(&segments);
    Ok(ShellAnalysis {
        segments: segments.into_iter().map(|segment| segment.words).collect(),
        allowed,
        reason,
    })
}

/// Go `isWhitelistedShell`: split into simple-command segments and accept only
/// allowlisted commands with safe flags and expansion use.
fn allowlist(segments: &[Segment]) -> (bool, Option<String>) {
    for segment in segments {
        let fields = &segment.words;
        let command = &fields[0];

        if segment.expands && !EXPANSION_ALLOWED.contains(&command.as_str()) {
            return (
                false,
                Some(format!(
                    "unquoted expansion is not allowed for command {}",
                    go_quote(command)
                )),
            );
        }
        if SIMPLE_ALLOWED.contains(&command.as_str()) {
            if let Some(reason) = dangerous_shell_flag(fields) {
                return (false, Some(reason));
            }
            continue;
        }
        if command == "git" && fields.len() > 1 && GIT_ALLOWED.contains(&fields[1].as_str()) {
            if let Some(reason) = dangerous_shell_flag(fields) {
                return (false, Some(reason));
            }
            continue;
        }
        return (
            false,
            Some(format!("command {} is not allowlisted", go_quote(command))),
        );
    }

    (true, None)
}

fn deny_flag(command: &str, flag: &str) -> String {
    format!(
        "flag {} is not allowed for command {}",
        go_quote(flag),
        go_quote(command)
    )
}

/// Go `hasDangerousShellFlag`. The command name has already passed the
/// allowlist, so only per-command flag restrictions remain.
fn dangerous_shell_flag(fields: &[String]) -> Option<String> {
    match fields[0].as_str() {
        "find" => {
            for argument in &fields[1..] {
                if FIND_DENIED.contains(&argument.as_str()) {
                    return Some(deny_flag("find", argument));
                }
            }
        }
        "rg" | "grep" => {
            for argument in &fields[1..] {
                if argument == "--pre"
                    || argument == "--pre-glob"
                    || argument == "--hostname-bin"
                    || argument.starts_with("--pre=")
                    || argument.starts_with("--pre-glob=")
                    || argument.starts_with("--hostname-bin=")
                {
                    return Some(deny_flag(&fields[0], argument));
                }
                // ripgrep's -z/--search-zip decompresses its inputs by
                // running gzip, xz or zstd resolved through PATH, which is
                // the program execution the allowlist exists to exclude.
                if fields[0] == "rg"
                    && (argument == "--search-zip" || short_option_cluster_has(argument, b'z'))
                {
                    return Some(deny_flag("rg", argument));
                }
            }
        }
        "git" => {
            if fields.len() < 2 {
                return None;
            }
            for argument in &fields[2..] {
                if argument == "--recurse-submodules"
                    || argument == "--show-signature"
                    || argument == "--verify-signatures"
                    || argument == "--submodule"
                    || argument.starts_with("--submodule=")
                {
                    return Some(deny_flag("git", argument));
                }
            }
            if fields[1] == "branch" {
                // Positional values for --contains, --merged, --no-merged,
                // --sort and --format are conservatively rejected to prevent
                // branch-name mutations; use an equals form where applicable.
                for argument in &fields[2..] {
                    if !GIT_BRANCH_ALLOWED.contains(&argument.as_str())
                        && !argument.starts_with("--sort=")
                        && !argument.starts_with("--format=")
                    {
                        return Some(format!(
                            "argument {} is not allowed for command {}",
                            go_quote(argument),
                            go_quote("git branch")
                        ));
                    }
                }
                return None;
            }
            for argument in &fields[2..] {
                // %G placeholders make log/show run the configured gpg program.
                if argument.contains("%G") {
                    return Some(deny_flag("git", argument));
                }
            }
            if fields[1] == "blame" {
                for argument in &fields[2..] {
                    if argument.starts_with("--ext-diff") || argument.starts_with("--textconv") {
                        return Some(deny_flag("git", argument));
                    }
                }
            }
            if fields[1] == "diff" || fields[1] == "log" || fields[1] == "show" {
                for argument in &fields[2..] {
                    if argument == "--output"
                        || argument.starts_with("--output=")
                        || argument == "-o"
                        || argument.starts_with("-o")
                        || argument.starts_with("--ext-diff")
                        || argument.starts_with("--textconv")
                    {
                        return Some(deny_flag("git", argument));
                    }
                }
            }
        }
        _ => {}
    }

    None
}

/// Go `shortOptionClusterHas`: one shell word is a cluster of short options
/// containing the given letter, so `-iz` is treated like `-z`. A long option, a
/// bare dash, or any word that is not a short-option cluster is not one.
fn short_option_cluster_has(word: &str, flag: u8) -> bool {
    let bytes = word.as_bytes();
    bytes.len() >= 2 && bytes[0] == b'-' && bytes[1] != b'-' && bytes[1..].contains(&flag)
}

/// Split a command into the simple-command segments the shell will execute.
/// Mirrors Go `lexShell`, including its byte-level handling.
fn lex_shell(command: &str) -> Result<Vec<Segment>, LexError> {
    Lexer::new(command.as_bytes()).run()
}

struct Lexer<'a> {
    command: &'a [u8],
    index: usize,
    segments: Vec<Segment>,
    segment: Vec<String>,
    segment_expands: bool,
    word: Vec<u8>,
    word_started: bool,
    tilde_eligible: bool,
    quote: u8,
    comment: bool,
    need_command: bool,
}

impl<'a> Lexer<'a> {
    fn new(command: &'a [u8]) -> Self {
        Self {
            command,
            index: 0,
            segments: Vec::new(),
            segment: Vec::new(),
            segment_expands: false,
            word: Vec::new(),
            word_started: false,
            tilde_eligible: true,
            quote: 0,
            comment: false,
            need_command: false,
        }
    }

    fn flush_word(&mut self) {
        if !self.word_started {
            return;
        }
        let word = std::mem::take(&mut self.word);
        self.segment.push(
            String::from_utf8(word).expect("shell word remains valid UTF-8 after quote removal"),
        );
        self.word_started = false;
        self.tilde_eligible = true;
    }

    fn flush_segment(&mut self) {
        self.flush_word();
        if self.segment.is_empty() {
            return;
        }
        self.segments.push(Segment {
            words: std::mem::take(&mut self.segment),
            expands: self.segment_expands,
        });
        self.segment_expands = false;
    }

    fn empty_before() -> LexError {
        LexError::new("empty command before separator")
    }

    fn empty_after() -> LexError {
        LexError::new("empty command after separator")
    }

    fn expansion() -> LexError {
        LexError::new("parameter expansion or command substitution is not allowed")
    }

    fn run(mut self) -> Result<Vec<Segment>, LexError> {
        while self.index < self.command.len() {
            let c = self.command[self.index];

            if self.comment {
                if c == b'\n' {
                    self.comment = false;
                    if self.need_command {
                        return Err(Self::empty_after());
                    }
                    self.flush_segment();
                }
                self.index += 1;
                continue;
            }

            if self.quote == b'\'' {
                if c == b'\'' {
                    self.quote = 0;
                } else {
                    self.word.push(c);
                }
                self.index += 1;
                continue;
            }

            if self.quote == b'"' {
                match c {
                    b'"' => {
                        self.quote = 0;
                        self.index += 1;
                    }
                    b'\\' => {
                        if self.index + 1 >= self.command.len() {
                            return Err(LexError::new("trailing backslash"));
                        }
                        let next = self.command[self.index + 1];
                        if next == b'\n' {
                            self.index += 2;
                            continue;
                        }
                        if next == b'$' || next == b'`' || next == b'"' || next == b'\\' {
                            self.word.push(next);
                            self.index += 2;
                            continue;
                        }
                        self.word.push(b'\\');
                        self.index += 1;
                    }
                    b'$' => return Err(Self::expansion()),
                    b'`' => {
                        return Err(LexError::new("command substitution is not allowed"));
                    }
                    _ => {
                        self.word.push(c);
                        self.index += 1;
                    }
                }
                continue;
            }

            match c {
                b'\\' => {
                    if self.index + 1 >= self.command.len() {
                        return Err(LexError::new("trailing backslash"));
                    }
                    self.need_command = false;
                    let next = self.command[self.index + 1];
                    if next == b'\n' {
                        self.index += 2;
                        continue;
                    }
                    self.word.push(next);
                    self.word_started = true;
                    self.tilde_eligible = false;
                    self.index += 2;
                }
                b'\'' => {
                    self.quote = b'\'';
                    self.word_started = true;
                    self.tilde_eligible = false;
                    self.need_command = false;
                    self.index += 1;
                }
                b'"' => {
                    self.quote = b'"';
                    self.word_started = true;
                    self.tilde_eligible = false;
                    self.need_command = false;
                    self.index += 1;
                }
                b' ' | b'\t' => {
                    self.flush_word();
                    self.index += 1;
                }
                b'\n' => {
                    self.flush_segment();
                    if self.need_command {
                        return Err(Self::empty_after());
                    }
                    self.need_command = false;
                    self.index += 1;
                }
                b';' => {
                    if self.need_command {
                        return Err(Self::empty_after());
                    }
                    self.flush_segment();
                    if self.segments.is_empty() {
                        return Err(Self::empty_before());
                    }
                    self.need_command = false;
                    self.index += 1;
                }
                b'&' => {
                    if self.need_command {
                        return Err(Self::empty_after());
                    }
                    self.flush_word();
                    let binary =
                        self.index + 1 < self.command.len() && self.command[self.index + 1] == b'&';
                    if binary && self.segment.is_empty() {
                        return Err(Self::empty_before());
                    }
                    self.flush_segment();
                    if self.segments.is_empty() {
                        return Err(Self::empty_before());
                    }
                    self.need_command = binary;
                    self.index += if binary { 2 } else { 1 };
                }
                b'|' => {
                    self.flush_word();
                    let binary =
                        self.index + 1 < self.command.len() && self.command[self.index + 1] == b'|';
                    if self.segment.is_empty() {
                        return Err(Self::empty_before());
                    }
                    self.flush_segment();
                    self.need_command = true;
                    self.index += if binary { 2 } else { 1 };
                }
                b'<' | b'>' => {
                    // This also rejects heredocs and process substitution.
                    // Quoted and escaped forms were handled above and are
                    // ordinary data.
                    return Err(LexError::new("redirection is not allowed"));
                }
                b'$' => return Err(Self::expansion()),
                b'`' => {
                    return Err(LexError::new("command substitution is not allowed"));
                }
                b'*' | b'?' | b'[' | b'{' | b'}' => {
                    // Pathname and brace expansion can turn a filename into an
                    // option.
                    self.word.push(c);
                    self.word_started = true;
                    self.tilde_eligible = false;
                    self.need_command = false;
                    self.segment_expands = true;
                    self.index += 1;
                }
                b'(' | b')' => {
                    return Err(LexError::new("parentheses are not allowed"));
                }
                b'~' => {
                    // Bash performs tilde expansion only at the beginning of a
                    // word and after an unquoted equals sign. A revision such
                    // as HEAD~1 is literal data.
                    if self.tilde_eligible {
                        self.segment_expands = true;
                    }
                    self.word.push(c);
                    self.word_started = true;
                    self.tilde_eligible = false;
                    self.index += 1;
                }
                b'#' => {
                    if !self.word_started {
                        self.comment = true;
                        self.index += 1;
                        continue;
                    }
                    self.word.push(c);
                    self.word_started = true;
                    self.tilde_eligible = false;
                    self.need_command = false;
                    self.index += 1;
                }
                _ => {
                    self.word.push(c);
                    self.word_started = true;
                    self.tilde_eligible = c == b'=';
                    self.need_command = false;
                    self.index += 1;
                }
            }
        }

        if self.quote == b'\'' {
            return Err(LexError::new("unterminated single quote"));
        }
        if self.quote == b'"' {
            return Err(LexError::new("unterminated double quote"));
        }
        if self.need_command {
            return Err(Self::empty_after());
        }
        self.flush_segment();
        if self.segments.is_empty() {
            return Err(LexError::new("empty command"));
        }
        Ok(self.segments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &str) -> Vec<Vec<String>> {
        lex_shell(command)
            .unwrap()
            .into_iter()
            .map(|segment| segment.words)
            .collect()
    }

    #[test]
    fn segments_follow_separators() {
        assert_eq!(
            words("ls -la; pwd && cat f | wc -l || echo no"),
            vec![
                vec!["ls".to_string(), "-la".to_string()],
                vec!["pwd".to_string()],
                vec!["cat".to_string(), "f".to_string()],
                vec!["wc".to_string(), "-l".to_string()],
                vec!["echo".to_string(), "no".to_string()],
            ]
        );
        assert_eq!(
            words("ls & pwd\ncat f"),
            vec![
                vec!["ls".to_string()],
                vec!["pwd".to_string()],
                vec!["cat".to_string(), "f".to_string()],
            ]
        );
    }

    #[test]
    fn quotes_and_backslashes_are_removed() {
        assert_eq!(
            words(r#"cat 'a b' "c d" a"b"'c' file\ with\ space"#),
            vec![vec![
                "cat".to_string(),
                "a b".to_string(),
                "c d".to_string(),
                "abc".to_string(),
                "file with space".to_string(),
            ]]
        );
        assert_eq!(
            words("echo \"a\\\"b\\\\c\\$d\\`e\""),
            vec![vec!["echo".to_string(), "a\"b\\c$d`e".to_string(),]]
        );
        assert_eq!(
            words("cat a\\\nb"),
            vec![vec!["cat".to_string(), "ab".to_string()]]
        );
        assert_eq!(
            words("cat café"),
            vec![vec!["cat".to_string(), "café".to_string()]]
        );
    }

    #[test]
    fn comments_and_newlines_end_segments() {
        assert_eq!(words("ls # trailing"), vec![vec!["ls".to_string()]]);
        assert_eq!(
            words("ls # trailing\npwd"),
            vec![vec!["ls".to_string()], vec!["pwd".to_string()]]
        );
        assert_eq!(words("# leading\nls"), vec![vec!["ls".to_string()]]);
        assert_eq!(
            words("echo a#b"),
            vec![vec!["echo".to_string(), "a#b".to_string()]]
        );
    }

    #[test]
    fn expansion_flags_follow_bash_rules() {
        let segments = lex_shell("cat *.go").unwrap();
        assert!(segments[0].expands);
        let segments = lex_shell("cat \\*.go").unwrap();
        assert!(!segments[0].expands);
        let segments = lex_shell("ls ~").unwrap();
        assert!(segments[0].expands);
        let segments = lex_shell("git log HEAD~1").unwrap();
        assert!(!segments[0].expands);
        let segments = lex_shell("echo x=~").unwrap();
        assert!(segments[0].expands);
        let segments = lex_shell("git log '~'").unwrap();
        assert!(!segments[0].expands);
        let segments = lex_shell("echo [ab]* {a,b} ?").unwrap();
        assert!(segments[0].expands);
    }

    #[test]
    fn expansion_flag_does_not_leak_across_segments() {
        let segments = lex_shell("ls *.go | find").unwrap();
        assert!(segments[0].expands);
        assert!(!segments[1].expands);

        let segments = lex_shell("find | ls *.go").unwrap();
        assert!(!segments[0].expands);
        assert!(segments[1].expands);
    }

    #[test]
    fn lexical_rejections_match_go() {
        for command in [
            "",
            "   ",
            "# comment",
            "cat 'x",
            "cat \"x",
            "cat \\",
            "echo $HOME",
            "echo \"$HOME\"",
            "echo `id`",
            "ls > out",
            "ls < in",
            "cat <<EOF",
            "cat <(ls)",
            "cat >(ls)",
            "ls (x)",
            "&& ls",
            "& ls",
            "| ls",
            "; ls",
            "ls &&",
            "ls ||",
            "ls |",
            "ls &&\npwd",
            "ls |\npwd",
        ] {
            assert!(
                lex_shell(command).is_err(),
                "lex_shell({command:?}) should reject"
            );
        }
    }

    #[test]
    fn empty_separators_with_a_completed_segment_are_accepted() {
        // Go only rejects an empty segment at the start or after a separator
        // that requires a command; `ls; ; pwd` is accepted.
        assert_eq!(
            words("ls; ; pwd"),
            vec![vec!["ls".to_string()], vec!["pwd".to_string()]]
        );
        assert_eq!(words("ls &"), vec![vec!["ls".to_string()]]);
        assert_eq!(
            words("ls & pwd"),
            vec![vec!["ls".to_string()], vec!["pwd".to_string()]]
        );
    }

    #[test]
    fn short_option_cluster_detection_matches_go() {
        assert!(short_option_cluster_has("-z", b'z'));
        assert!(short_option_cluster_has("-iz", b'z'));
        assert!(short_option_cluster_has("-zi", b'z'));
        assert!(!short_option_cluster_has("-", b'z'));
        assert!(!short_option_cluster_has("--z", b'z'));
        assert!(!short_option_cluster_has("z", b'z'));
        assert!(!short_option_cluster_has("", b'z'));
    }
}
