//! Shell execution and managed-process cleanup.

pub mod process;
pub mod shell;

#[cfg(unix)]
pub mod process_unix;
#[cfg(windows)]
pub mod process_windows;
