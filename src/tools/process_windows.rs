//! Temporary Windows managed-command runner (Task 17) — NOT Go-parity.
//!
//! This is a deliberately weak placeholder, present only so all targets compile
//! until Task 18 replaces it with the real Go-parity backend: `CreateProcessW`
//! suspended-start, `AssignProcessToJobObject` with `KILL_ON_JOB_CLOSE`, and
//! `ResumeThread`. This fallback starts the child with `std::process::Command`
//! and has NO Job Object, so descendants can outlive the call; on cancellation
//! it kills only the direct child. It is explicitly NOT Go-parity and is
//! replaced wholesale by Task 18.
//!
//! It still honours the shared contract: stdout/stderr are drained on helper
//! threads into a single capped [`LimitedBuffer`] (cap during draining), the
//! inherited-pipe wait is bounded by ONE combined `wait_delay` deadline,
//! captured bytes are always retained (Go's `out`), and pipe read ends are
//! dropped on expiry so their fds close.

use crate::tools::process::{CommandSpec, LimitedBuffer, build_command, cancelled_error};
use std::io::{self, Read};
use std::process::{ExitStatus, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub fn run_managed_command(
    spec: CommandSpec,
    cancel: &CancellationToken,
    wait_delay: Duration,
) -> io::Result<Output> {
    let mut command = build_command(spec);
    let mut child = command.spawn()?;

    let limited = Arc::new(Mutex::new(LimitedBuffer::new()));
    let stop = Arc::new(AtomicBool::new(false));
    // The read ends live in shared slots so the main thread can drop them on
    // WaitDelay expiry (closing the pipe fds) even while readers are running.
    let out_slot = Arc::new(Mutex::new(child.stdout.take()));
    let err_slot = Arc::new(Mutex::new(child.stderr.take()));

    // Each reader holds a `done_tx` clone; when both readers exit, all senders
    // are dropped and `done_rx` reports Disconnected. A SINGLE `recv_timeout`
    // bounds the inherited-pipe wait (never two sequential waits = 2x delay).
    let (done_tx, done_rx) = mpsc::channel::<()>();
    spawn_reader(
        Arc::clone(&out_slot),
        Arc::clone(&limited),
        Arc::clone(&stop),
        done_tx.clone(),
    );
    spawn_reader(
        Arc::clone(&err_slot),
        Arc::clone(&limited),
        Arc::clone(&stop),
        done_tx.clone(),
    );
    drop(done_tx);

    // Wait for the direct child. Only kill a still-live child on cancellation:
    // a cancel observed after the child exited on its own must NOT turn success
    // into `context canceled` (the `exited.is_none()` guard). `try_wait` is
    // checked first so a child that already exited on its own is never killed.
    let mut exited: Option<ExitStatus> = None;
    let mut killed = false;
    let status = loop {
        if exited.is_none()
            && let Some(s) = child.try_wait()?
        {
            exited = Some(s);
        }
        if cancel.is_cancelled() && exited.is_none() && !killed {
            // No Job Object yet: kill the direct child only.
            let _ = child.kill();
            killed = true;
        }
        if let Some(s) = exited {
            break s;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    // ONE combined deadline for both readers: returns Disconnected when both
    // have finished, or Timeout after wait_delay. No 2x wait_delay.
    let _ = done_rx.recv_timeout(wait_delay);

    // On WaitDelay expiry (or normal completion), stop the readers and drop the
    // pipe read ends so their fds close. `try_lock` avoids blocking on a reader
    // momentarily inside a blocking read; the `stop` flag makes that reader
    // release its slot as soon as the read returns. A descendant that inherited
    // the write end (the temporary non-Go-parity weakness) may keep one reader
    // blocked until it exits — Task 18's Job Object removes this gap.
    stop.store(true, Ordering::SeqCst);
    drop(done_rx);
    if let Ok(mut slot) = out_slot.try_lock() {
        *slot = None;
    }
    if let Ok(mut slot) = err_slot.try_lock() {
        *slot = None;
    }

    // Retain every byte captured so far (never discard on timeout). The bytes
    // travel with the cancellation error on the killed path (I-4).
    let captured = limited.lock().unwrap().finish();
    if killed {
        return Err(cancelled_error(captured));
    }
    Ok(Output {
        status,
        stdout: captured,
        stderr: Vec::new(),
    })
}

/// Drain one pipe read end into the shared capped buffer. The read end is held
/// only for a single chunk at a time and dropped when the loop ends (or when the
/// main thread takes it on WaitDelay expiry). `done_tx` is kept alive for the
/// thread's whole lifetime so dropping it signals `done_rx`.
fn spawn_reader<R: Read + Send + 'static>(
    slot: Arc<Mutex<Option<R>>>,
    limited: Arc<Mutex<LimitedBuffer>>,
    stop: Arc<AtomicBool>,
    done_tx: mpsc::Sender<()>,
) {
    std::thread::spawn(move || {
        let _done = done_tx;
        let mut chunk = [0u8; 8192];
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            // Scope the slot guard so it is released before locking `limited`
            // (no lock-order inversion), even while a read is in progress.
            let result = {
                let Ok(mut guard) = slot.lock() else {
                    break;
                };
                let Some(r) = guard.as_mut() else {
                    break;
                };
                r.read(&mut chunk)
            };
            match result {
                Ok(0) => break,
                Ok(n) => limited.lock().unwrap().push(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}
