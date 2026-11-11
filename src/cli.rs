use std::ffi::OsString;
use std::io::{self, Write};

/// CLI entry point. `args` is the full `argv`, so `args[0]` is the program name.
///
/// Only `--version` and `-version` are recognized; when every argument after
/// the program name is one of those, the version is printed and 0 is returned.
/// Any unknown argument (or no argument) yields exit code 2. Additional
/// subcommands are added by later tasks.
pub fn run(args: Vec<OsString>) -> i32 {
    let mut saw_version = false;
    for arg in args.iter().skip(1) {
        match arg.to_str() {
            Some("--version" | "-version") => saw_version = true,
            _ => return 2,
        }
    }
    if !saw_version {
        return 2;
    }
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "subagent-mcp {}", crate::VERSION);
    let _ = stdout.flush();
    0
}
