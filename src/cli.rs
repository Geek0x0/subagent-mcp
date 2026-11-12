//! Command-line entry point, flag parsing, and the serve lifecycle.
//!
//! Ports Go `cmd/subagent-mcp/main.go` (subagent-mcp v0.13.1). The parser
//! reproduces Go `flag` semantics: `-flag` and `--flag` are equivalent, parsing
//! stops at the first positional argument, `--` ends flags, an unknown flag
//! prints `flag provided but not defined: -x` plus usage to stderr and exits 2,
//! and help exits 0.
//!
//! Task 25A owns the flags, `--check-config`, and the main-process setup
//! (non-dumpable on Linux, scrubbed environment, protected config file). Task
//! 25B owns the stdio serve lifecycle: the detached stdin reader and stdout
//! writer, first-signal graceful shutdown, second-signal exit 130, stdin EOF
//! cleanup, and the shared ten-second shutdown budget.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::agent::session::SessionManager;
use crate::check;
use crate::config::{self, Config};
use crate::mcp::server::{McpError, McpServer, ToolHandler};
use crate::mcp::stdio::{self, StdoutWriter};
use crate::mcp::tools::SubagentTools;
use crate::util::json::go_quote;

/// Go `shutdownGrace`: how long the process waits for in-flight runs to unwind
/// and for sessions and provider resources to be released.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

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

/// Serve the MCP stdio protocol until stdin reaches EOF or a signal asks the
/// server to stop, then shut down within [`SHUTDOWN_GRACE`].
///
/// The process exits through [`std::process::exit`] after the writer drains, so
/// the detached stdin thread is never joined and a dependency that ignores
/// cancellation cannot keep the process alive. Setup failures return an error
/// before the loop starts.
pub async fn serve(config: Config, tool_name: String) -> Result<(), CliError> {
    let mut signals =
        Signals::new().map_err(|error| err(format!("install the signal handler: {error}")))?;

    let manager = Arc::new(SessionManager::new());
    let tools: Arc<dyn ToolHandler> = Arc::new(SubagentTools::new(
        tool_name,
        Arc::new(config),
        Arc::clone(&manager),
    ));
    let server = McpServer::new(
        "subagent-mcp".to_string(),
        crate::VERSION.to_string(),
        tools,
    );

    let frames = stdio::spawn_stdin_reader();
    let writer = StdoutWriter::spawn();
    let out = writer.sender();

    // The serve loop runs as a task so a signal can stop it while it is parked
    // on stdin; the loop itself returns cleanly on EOF and on `close`.
    let serve_server = server.clone();
    let mut serve_task = tokio::spawn(async move { serve_server.serve(frames, out).await });

    let stop = tokio::select! {
        joined = &mut serve_task => Stop::Served(joined),
        name = signals.next() => Stop::Signal(name),
    };
    let signal_path = matches!(&stop, Stop::Signal(_));

    let mut code = 0;
    let mut pending: Option<tokio::task::JoinHandle<Result<(), McpError>>> = None;
    match stop {
        Stop::Served(joined) => match joined {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log_line(&error);
                code = 1;
            }
            Err(error) => {
                log_line(format!("serve task failed: {error}"));
                code = 1;
            }
        },
        Stop::Signal(name) => {
            log_line(format!("received {name}; shutting down"));
            pending = Some(serve_task);
        }
    }

    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    let cleanup = async {
        // Stop reading, cancel every active call, and wait for the handlers.
        // Queued calls are cancelled as they begin because `close` sets the
        // closing flag first.
        server.close(remaining(deadline)).await;
        if server.active_calls() > 0 {
            log_line("shutdown: in-flight calls did not finish in time; releasing sessions anyway");
        }

        // Sessions go next: releasing their bound directories and closing their
        // provider threads (which interrupts a pending Codex turn) is what makes
        // the provider pool safe to release.
        let session_cancel = CancellationToken::new();
        match tokio::time::timeout(remaining(deadline), manager.close(&session_cancel)).await {
            Ok(()) => {}
            Err(_) => {
                log_line("shutdown: a session is still busy; leaving it to process exit");
            }
        }

        // On the signal path, ending the serve loop is how `close` stops it;
        // the loop reports success rather than the cancellation Go's mcp-go
        // loop returns, so only a genuine error changes the exit code.
        if let Some(task) = pending.as_mut() {
            match tokio::time::timeout(remaining(deadline), task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => {
                    log_line(&error);
                    code = 1;
                }
                Ok(Err(error)) => {
                    log_line(format!("serve task failed: {error}"));
                    code = 1;
                }
                Err(_) => log_line(format!(
                    "shutdown: the stdio loop did not stop within {}s",
                    SHUTDOWN_GRACE.as_secs()
                )),
            }
        }

        let dropped = server.dropped_notifications();
        if dropped > 0 {
            log_line(format!(
                "shutdown: the client's notification channel refused {dropped} event(s); the client was not keeping up"
            ));
        }

        // Provider resources go last, and within the same budget: a pool whose
        // child is being dialed holds its gate until that dial returns, and
        // shutdown must not outlast the grace period it promises.
        let shutdown = tokio::task::spawn_blocking(crate::provider::shutdown);
        match tokio::time::timeout(remaining(deadline), shutdown).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log_line(format!("shutdown: provider cleanup task failed: {error}")),
            Err(_) => log_line(format!(
                "shutdown: a provider did not release its resources within {}s",
                SHUTDOWN_GRACE.as_secs()
            )),
        }

        // Drain every frame already handed to the writer before exiting; the
        // detached stdin thread is deliberately not joined.
        match tokio::time::timeout(remaining(deadline), writer.flush()).await {
            Ok(()) => {}
            Err(_) => log_line("shutdown: the client's output did not drain in time"),
        }
    };

    if signal_path {
        // A second signal abandons the wait: an operator must always be able to
        // stop a server parked on a dependency that ignores cancellation.
        tokio::select! {
            _ = cleanup => {}
            name = signals.next() => {
                log_line(format!("received a second signal ({name}); exiting immediately"));
                std::process::exit(130);
            }
        }
    } else {
        cleanup.await;
    }

    std::process::exit(code);
}

/// Why the serve loop stopped. `Served` carries the task result so EOF reports
/// a channel failure exactly once.
enum Stop {
    Served(Result<Result<(), McpError>, tokio::task::JoinError>),
    Signal(&'static str),
}

/// The remaining slice of the shared shutdown budget.
fn remaining(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}

/// The one stderr logger for the serve path, shaped like Go's `log` package.
/// API keys, config values, and request bodies are never passed here.
fn log_line(message: impl std::fmt::Display) {
    eprintln!(
        "{} {message}",
        chrono::Local::now().format("%Y/%m/%d %H:%M:%S")
    );
}

/// SIGINT/SIGTERM on Unix; Ctrl-C on Windows, which has no SIGTERM.
#[cfg(unix)]
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn next(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}

#[cfg(not(unix))]
struct Signals;

#[cfg(not(unix))]
impl Signals {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    async fn next(&mut self) -> &'static str {
        let _ = tokio::signal::ctrl_c().await;
        "SIGINT"
    }
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
            log_line(&error);
            return 1;
        }
    };
    let config = match config::load(&path) {
        Ok(config) => config,
        Err(error) => {
            log_line(&error);
            return 1;
        }
    };
    configure_runtime(&config);

    let mut tool_name = "subagent".to_string();
    if let Some(value) = std::env::var_os("SUBAGENT_MCP_TOOL_NAME") {
        let value = value.to_string_lossy().into_owned();
        if !value.is_empty() {
            if let Err(error) = validate_tool_name(&value) {
                log_line(&error);
                return 1;
            }
            tool_name = value;
        }
    }

    // The helper has already run, so this is the first Tokio runtime in the
    // process. `serve` exits through `std::process::exit` once the writer has
    // drained; only a setup failure returns here.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log_line(format!("start the async runtime: {error}"));
            return 1;
        }
    };
    match runtime.block_on(serve(config, tool_name)) {
        Ok(()) => 0,
        Err(error) => {
            log_line(&error);
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
