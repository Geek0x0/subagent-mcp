fn main() {
    // The Landlock helper must run before cli::run and before any Tokio runtime
    // is created: it restricts the process with Landlock and then execs.
    if subagent_mcp::sandbox::maybe_run_helper() {
        std::process::exit(126);
    }
    std::process::exit(subagent_mcp::cli::run(std::env::args_os().collect()));
}
