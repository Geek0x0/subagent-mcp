//! OS stdio plumbing for the MCP server.
//!
//! stdin is read by one detached OS thread performing blocking reads (a Tokio
//! stdin task is not cancellable and does not reproduce Go's backpressure). The
//! thread forwards complete lines over a capacity-1 channel, so at most one
//! frame is buffered ahead of the serve loop. stdout is drained by a task that
//! serializes frames and flushes every line.

use std::io::{BufRead, Write};

use tokio::sync::mpsc;

/// Capacity of the stdin hand-off channel: one frame of lookahead.
const STDIN_CHANNEL_CAPACITY: usize = 1;

/// Capacity of the stdout frame channel.
const STDOUT_CHANNEL_CAPACITY: usize = 100;

/// Spawns the detached blocking stdin reader and returns the frame receiver.
///
/// The thread is never joined: a blocking read on a pipe the client keeps open
/// is not cancellable, and shutdown must not wait for it.
pub fn spawn_stdin_reader() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(STDIN_CHANNEL_CAPACITY);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Ok(_) => {
                    if bytes.last() == Some(&b'\n') {
                        bytes.pop();
                        if bytes.last() == Some(&b'\r') {
                            bytes.pop();
                        }
                    }
                    // Go's encoding/json tolerates invalid UTF-8 by replacing
                    // it; lossy conversion keeps the reader alive instead of
                    // treating a malformed line as EOF.
                    let line = String::from_utf8_lossy(&bytes).into_owned();
                    if tx.blocking_send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// Spawns the stdout writer task and returns the frame sender.
///
/// The task owns the only writer, so frames from concurrent calls cannot
/// interleave. A write error ends the task; the server treats a closed channel
/// as a client that has gone away.
pub fn spawn_stdout_writer() -> mpsc::Sender<String> {
    let (tx, mut rx) = mpsc::channel(STDOUT_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let stdout = std::io::stdout();
        while let Some(frame) = rx.recv().await {
            let mut lock = stdout.lock();
            if writeln!(lock, "{frame}").is_err() {
                break;
            }
            if lock.flush().is_err() {
                break;
            }
        }
    });
    tx
}
