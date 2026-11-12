//! OS stdio plumbing for the MCP server.
//!
//! stdin is read by one detached OS thread performing blocking reads (a Tokio
//! stdin task is not cancellable and does not reproduce Go's backpressure). The
//! thread forwards complete lines over a capacity-1 channel, so at most one
//! frame is buffered ahead of the serve loop. stdout is drained by a task that
//! serializes frames and flushes every line; [`StdoutWriter::flush`] lets the
//! shutdown path wait until the queue is empty before the process exits.

use std::io::{BufRead, Write};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

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

/// The stdout writer task and its flush handle.
///
/// The task owns the only writer, so frames from concurrent calls cannot
/// interleave. A write error ends the task; the server treats a closed channel
/// as a client that has gone away. The flush channel is separate from the frame
/// channel because the server state keeps a frame sender alive for elicitation
/// approvals, so dropping the frame sender cannot signal "drained".
pub struct StdoutWriter {
    frames: mpsc::Sender<String>,
    flush: mpsc::Sender<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl StdoutWriter {
    /// Spawns the writer task.
    pub fn spawn() -> Self {
        let (frames, mut frame_rx) = mpsc::channel::<String>(STDOUT_CHANNEL_CAPACITY);
        let (flush, mut flush_rx) = mpsc::channel::<oneshot::Sender<()>>(1);
        let task = tokio::spawn(async move {
            let stdout = std::io::stdout();
            let mut flush_open = true;
            loop {
                tokio::select! {
                    biased;
                    request = flush_rx.recv(), if flush_open => match request {
                        Some(ack) => {
                            // Drain every frame queued before the flush request,
                            // then flush the OS buffer and acknowledge.
                            while let Ok(frame) = frame_rx.try_recv() {
                                if !write_frame(&stdout, &frame) {
                                    let _ = ack.send(());
                                    return;
                                }
                            }
                            let _ = stdout.lock().flush();
                            let _ = ack.send(());
                        }
                        None => flush_open = false,
                    },
                    frame = frame_rx.recv() => {
                        let Some(frame) = frame else { break };
                        if !write_frame(&stdout, &frame) {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            frames,
            flush,
            task,
        }
    }

    /// A clone of the frame sender.
    pub fn sender(&self) -> mpsc::Sender<String> {
        self.frames.clone()
    }

    /// Waits until every queued frame has been written and stdout has been
    /// flushed. Returns immediately if the writer task has already stopped.
    pub async fn flush(&self) {
        let (ack, done) = oneshot::channel();
        if self.flush.send(ack).await.is_ok() {
            let _ = done.await;
        }
    }

    /// The writer task's join handle, for a caller that wants to wait for the
    /// task itself. Shutdown exits the process instead of joining.
    pub fn task(&self) -> &JoinHandle<()> {
        &self.task
    }
}

/// Spawns the stdout writer task and returns the frame sender.
///
/// The task owns the only writer, so frames from concurrent calls cannot
/// interleave. A write error ends the task; the server treats a closed channel
/// as a client that has gone away.
pub fn spawn_stdout_writer() -> mpsc::Sender<String> {
    StdoutWriter::spawn().sender()
}

fn write_frame(stdout: &std::io::Stdout, frame: &str) -> bool {
    let mut lock = stdout.lock();
    writeln!(lock, "{frame}").is_ok() && lock.flush().is_ok()
}
