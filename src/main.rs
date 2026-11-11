fn main() {
    std::process::exit(subagent_mcp::cli::run(std::env::args_os().collect()));
}
