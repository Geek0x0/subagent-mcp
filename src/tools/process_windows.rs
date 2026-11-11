//! Temporary Windows managed-command runner (Task 17).
//!
//! This is the Go-parity "start-then-assign" placeholder: it starts the child
//! with `std::process::Command`, then manages it — reads both pipes on helper
//! threads, kills the direct child on cancellation, and bounds the inherited
//! pipe wait with `wait_delay`. It deliberately does **not** yet confine the
//! child (and its descendants) to a Job Object, so a descendant can outlive the
//! call. Task 18 replaces this file with the `CreateProcessW` suspended-start /
//! `AssignProcessToJobObject` / `ResumeThread` path and `KILL_ON_JOB_CLOSE`,
//! which closes that gap. Keeping the whole backend in one file makes that
//! replacement a drop-in.

use crate::tools::process::{CommandSpec, build_command, cancelled_error};
use std::io::{self, Read};
use std::process::Output;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub fn run_managed_command(
    spec: CommandSpec,
    cancel: &CancellationToken,
    wait_delay: Duration,
) -> io::Result<Output> {
    let mut command = build_command(spec);
    let mut child = command.spawn()?;

    // Drain stdout/stderr on helper threads so a full pipe cannot deadlock the
    // child; collect the bytes through bounded channels (Go `cmd.WaitDelay`).
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut v);
        }
        let _ = out_tx.send(v);
    });
    std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_end(&mut v);
        }
        let _ = err_tx.send(v);
    });

    let mut killed = false;
    let status = loop {
        if cancel.is_cancelled() && !killed {
            // No Job Object yet: kill the direct child only.
            let _ = child.kill();
            killed = true;
        }
        match child.try_wait()? {
            Some(s) => break s,
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };

    // Bounded inherited-pipe wait. Detached reader threads finish when the
    // (temporary, unconfined) descendants eventually exit.
    let stdout = out_rx.recv_timeout(wait_delay).unwrap_or_default();
    let stderr = err_rx.recv_timeout(wait_delay).unwrap_or_default();

    if killed {
        return Err(cancelled_error());
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
