//! Shell execution and managed-process cleanup.

pub mod process;
pub mod shell;

#[cfg(unix)]
pub mod process_unix;
/// Windows backend: `CreateProcessW` suspended-start -> Job Object assign ->
/// resume (Task 18). This is the one intentional security-strengthening
/// deviation from Go; the differential suite allowlists it (design §5.6/§8.2).
/// See [`process_windows`]'s module doc for the differential allowlist entry.
#[cfg(windows)]
pub mod process_windows;
