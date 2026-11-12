fn main() {
    // The Landlock helper must run before cli::run and before any Tokio runtime
    // is created: it restricts the process with Landlock and then execs.
    // cli::run owns everything after that point, including the Tokio runtime,
    // the Linux non-dumpable setting, the scrubbed environment, and the
    // protected config file.
    if subagent_mcp::sandbox::maybe_run_helper() {
        std::process::exit(126);
    }
    std::process::exit(subagent_mcp::cli::run(std::env::args_os().collect()));
}
