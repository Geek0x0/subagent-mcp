//! Command-line entry point and flag parsing.
//!
//! Ports Go `cmd/subagent-mcp/main.go` (subagent-mcp v0.13.1) up to the serve
//! lifecycle. The parser reproduces Go `flag` semantics: `-flag` and `--flag`
//! are equivalent, parsing stops at the first positional argument, `--` ends
//! flags, an unknown flag prints `flag provided but not defined: -x` plus usage
//! to stderr and exits 2, and help exits 0.
//!
//! Task 25A owns the flags, `--check-config`, and the main-process setup
//! (non-dumpable on Linux, scrubbed environment, protected config file). The
//! stdio serve loop is Task 25B: [`serve`] returns a clear error until then.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::check;
use crate::config::{self, Config};
use crate::util::json::go_quote;

/// A CLI failure with the exact operator-facing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

fn err(message: impl Into<String>) -> CliError {
    CliError {
        message: message.into(),
    }
}

/// CLI entry point. `args` is the full `argv`, so `args[0]` is the program name.
pub fn run(args: Vec<OsString>) -> i32 {
    let program = args
        .first()
        .map(|arg| arg.to_string_lossy().into_owned())
        .unwrap_or_else(|| "subagent-mcp".to_string());

    let flags = match parse(&args) {
        ParseOutcome::Flags(flags) => flags,
        ParseOutcome::Help => {
            print_usage(&program);
            return 0;
        }
        ParseOutcome::Error(message) => {
            eprintln!("{message}");
            print_usage(&program);
            return 2;
        }
    };

    if flags.version {
        let mut stdout = io::stdout().lock();
        let _ = writeln!(stdout, "subagent-mcp {}", crate::VERSION);
        let _ = stdout.flush();
        return 0;
    }
    if flags.check_config {
        return run_check_config(&flags);
    }
    run_serve()
}

/// Validate `SUBAGENT_MCP_TOOL_NAME` exactly like Go `server.ValidateToolName`.
pub fn validate_tool_name(name: &str) -> Result<(), CliError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !valid {
        return Err(err(format!(
            "SUBAGENT_MCP_TOOL_NAME {} is invalid: the name must be 1 to 64 characters matching ^[A-Za-z0-9_-]{{1,64}}$",
            go_quote(name)
        )));
    }
    if name.ends_with("-reply") {
        return Err(err(format!(
            "SUBAGENT_MCP_TOOL_NAME {} is invalid: the name must not end with \"-reply\" because the reply tool is registered as {name}-reply",
            go_quote(name)
        )));
    }
    Ok(())
}

/// Serve the MCP stdio protocol. The lifecycle belongs to Task 25B.
pub fn serve(_config: Config, _tool_name: String) -> Result<(), CliError> {
    Err(err("serve lifecycle is implemented in Task 25B"))
}

// ---------------------------------------------------------------------------
// Flag parsing (Go `flag` semantics)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Flags {
    version: bool,
    check_config: bool,
    live: bool,
    positionals: Vec<OsString>,
}

enum ParseOutcome {
    Flags(Flags),
    Help,
    Error(String),
}

fn parse(args: &[OsString]) -> ParseOutcome {
    let mut flags = Flags::default();
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].to_string_lossy();
        if arg == "--" {
            flags.positionals.extend(args[index + 1..].iter().cloned());
            return ParseOutcome::Flags(flags);
        }
        if !arg.starts_with('-') || arg == "-" {
            flags.positionals.extend(args[index..].iter().cloned());
            return ParseOutcome::Flags(flags);
        }

        let minuses = if arg.starts_with("--") { 2 } else { 1 };
        let body = &arg[minuses..];
        if body.is_empty() || body.starts_with('-') || body.starts_with('=') {
            return ParseOutcome::Error(format!("bad flag syntax: {arg}"));
        }
        let (name, value) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (body, None),
        };
        let bool_value = |flag: &str| -> Result<bool, ParseOutcome> {
            parse_bool(value).map_err(|()| {
                let value = value.unwrap_or("");
                ParseOutcome::Error(format!(
                    "invalid boolean value {} for -{flag}: strconv.ParseBool: parsing {}: invalid syntax",
                    go_quote(value),
                    go_quote(value)
                ))
            })
        };

        match name {
            "version" => match bool_value("version") {
                Ok(value) => flags.version = value,
                Err(outcome) => return outcome,
            },
            "check-config" => match bool_value("check-config") {
                Ok(value) => flags.check_config = value,
                Err(outcome) => return outcome,
            },
            "live" => match bool_value("live") {
                Ok(value) => flags.live = value,
                Err(outcome) => return outcome,
            },
            "h" | "help" => return ParseOutcome::Help,
            _ => {
                return ParseOutcome::Error(format!("flag provided but not defined: -{name}"));
            }
        }
        index += 1;
    }
    ParseOutcome::Flags(flags)
}

/// Go `strconv.ParseBool`: an absent value means the flag was set.
fn parse_bool(value: Option<&str>) -> Result<bool, ()> {
    match value {
        None => Ok(true),
        Some("1" | "t" | "T" | "TRUE" | "true" | "True") => Ok(true),
        Some("0" | "f" | "F" | "FALSE" | "false" | "False") => Ok(false),
        Some(_) => Err(()),
    }
}

fn print_usage(program: &str) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "Usage of {program}:");
    let _ = writeln!(stderr, "  -check-config");
    let _ = writeln!(
        stderr,
        "    \tvalidate the config file and each provider's reachability, then exit"
    );
    let _ = writeln!(stderr, "  -live");
    let _ = writeln!(
        stderr,
        "    \twith --check-config, make a real minimal call to each reachable provider (billable)"
    );
    let _ = writeln!(stderr, "  -version");
    let _ = writeln!(stderr, "    \tprint version and exit");
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

fn run_check_config(flags: &Flags) -> i32 {
    let mut stdout = io::stdout().lock();
    let path = match flags.positionals.first().filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => match config::default_path() {
            Ok(path) => path,
            Err(error) => {
                let _ = writeln!(stdout, "config   FAIL: {error}");
                return 1;
            }
        },
    };

    let loaded = match config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(stdout, "config   {}   FAIL: {error}", path.display());
            return 1;
        }
    };
    configure_runtime(&loaded);
    let _ = writeln!(stdout, "config   {}   OK", path.display());
    check::report_sandbox(&mut stdout);
    let code = check::run_to(&mut stdout, &loaded, flags.live);
    let _ = stdout.flush();
    // Checking a codex-app-server provider starts its child, so release it
    // before exiting rather than leaving it to stdin EOF.
    crate::provider::shutdown();
    code
}

fn run_serve() -> i32 {
    let path = match config::default_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let config = match config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    configure_runtime(&config);

    let mut tool_name = "subagent".to_string();
    if let Some(value) = std::env::var_os("SUBAGENT_MCP_TOOL_NAME") {
        let value = value.to_string_lossy().into_owned();
        if !value.is_empty() {
            if let Err(error) = validate_tool_name(&value) {
                eprintln!("{error}");
                return 1;
            }
            tool_name = value;
        }
    }

    match serve(config, tool_name) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

/// Go `configureRuntime`: protect the process environment, scrub provider keys
/// from shell children, and refuse to read the config file through tools.
fn configure_runtime(config: &Config) {
    if let Err(error) = disable_process_dumping() {
        eprintln!(
            "warning: /proc environment protection could not be enabled: {error}; continuing"
        );
    }
    crate::tools::set_scrubbed_env(check::env_keys(config), vec!["SUBAGENT_MCP_".to_string()]);
    crate::tools::set_protected_files(vec![config.path.clone()]);
}

#[cfg(target_os = "linux")]
fn disable_process_dumping() -> io::Result<()> {
    // SAFETY: PR_SET_DUMPABLE takes a scalar mode and four unused arguments;
    // passing zeroes cannot violate memory safety.
    let result = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn disable_process_dumping() -> io::Result<()> {
    Ok(())
}
