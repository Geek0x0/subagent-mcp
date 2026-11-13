//! Unix managed-command runner (port of `internal/tools/process_unix.go`).
//!
//! The child is started in its own process group. On cancellation the whole
//! group is signalled with `SIGKILL`, which also wakes the blocking
//! `Child::try_wait` loop. After the direct child exits the group is swept to
//! reap backgrounded descendants; a 1 s WaitDelay bounds any inherited-pipe
//! wait. A descendant that escapes the group with `setsid` can survive — the
//! same limitation Go documents.

use crate::tools::process::{CommandSpec, LimitedBuffer, build_command, cancelled_error};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{ChildStdout, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// How long a single `poll` waits before we re-check the child and the cancel
/// token. Small enough that cancellation/timeout is observed promptly.
const POLL_INTERVAL_MS: i32 = 20;

pub fn run_managed_command(
    spec: CommandSpec,
    cancel: &CancellationToken,
    wait_delay: Duration,
) -> io::Result<Output> {
    let mut command = build_command(spec);
    // New process group; pgid == the direct child's pid.
    // Go: SysProcAttr{Setpgid: true}.
    command.process_group(0);
    command.stderr(Stdio::null());
    // SAFETY: dup2 is async-signal-safe. Command has installed stdout before
    // pre_exec runs; both descriptors must write to that same pipe.
    unsafe {
        command.pre_exec(|| {
            if libc::dup2(1, 2) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pgid = child.id();

    let mut stdout = child.stdout.take();
    if let Some(s) = &stdout {
        set_nonblocking(s.as_raw_fd())?;
    }

    // Single Go-style limited buffer: one shared pipe feeds it, capping during
    // draining (never buffering unbounded output) and counting the true total.
    let mut limited = LimitedBuffer::new();
    let mut out_eof = stdout.is_none();
    let mut status: Option<ExitStatus> = None;
    let mut exited_at: Option<Instant> = None;
    let mut killed = false;

    loop {
        poll_once(&stdout, out_eof, POLL_INTERVAL_MS);
        drain(&mut stdout, &mut limited, &mut out_eof)?;

        if status.is_none()
            && let Some(s) = child.try_wait()?
        {
            status = Some(s);
            exited_at = Some(Instant::now());
        }

        // Cancellation only matters while the direct child is alive. Once it
        // has exited on its own, the remaining pipe wait is bounded solely by
        // wait_delay (Go: watchCtx returns once Process.Wait observes exit, so
        // a later ctx cancellation cannot convert the result into a timeout).
        if cancel.is_cancelled() && status.is_none() && !killed {
            kill_group(pgid);
            killed = true;
        }

        if let Some(t) = exited_at {
            if out_eof {
                break;
            }
            if t.elapsed() >= wait_delay {
                break; // ErrWaitDelay equivalent
            }
        }
    }

    // Go sweeps after Run's inherited-pipe drain, allowing short-lived
    // descendants to finish within WaitDelay.
    sweep_group(pgid);
    let status = match status {
        Some(s) => s,
        None => child.wait()?,
    };
    // Dropping the read ends unblocks any orphaned writers still holding the
    // pipes (Go: closeDescriptors(parentIOPipes)).
    drop(stdout);

    // The captured/truncated bytes travel with the cancellation error so the
    // shell tool can still return them as `output` (Go keeps `out` on error).
    let captured = limited.finish();
    if killed {
        return Err(cancelled_error(captured));
    }
    Ok(Output {
        status,
        stdout: captured,
        stderr: Vec::new(),
    })
}

fn set_nonblocking(fd: i32) -> io::Result<()> {
    // SAFETY: fd is a valid pipe read end owned by `child`.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn poll_once(stdout: &Option<ChildStdout>, out_eof: bool, timeout_ms: i32) {
    let mut fds: Vec<libc::pollfd> = Vec::with_capacity(1);
    if !out_eof && let Some(s) = stdout {
        fds.push(libc::pollfd {
            fd: s.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    if fds.is_empty() {
        // Output is at EOF; still poll the child/cancel at a fixed cadence.
        std::thread::sleep(Duration::from_millis(timeout_ms as u64));
        return;
    }
    // SAFETY: fds points to a valid, initialized array of `libc::pollfd`.
    unsafe {
        libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms);
    }
}

/// Drain all currently available bytes from a non-blocking pipe read end into
/// the capped buffer. Reading continues even after the cap is reached so the
/// child never blocks on a full pipe; surplus bytes are counted and discarded.
fn drain(
    reader: &mut Option<impl Read>,
    buf: &mut LimitedBuffer,
    eof: &mut bool,
) -> io::Result<()> {
    let Some(r) = reader.as_mut() else {
        *eof = true;
        return Ok(());
    };
    if *eof {
        return Ok(());
    }
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => {
                *eof = true;
                break;
            }
            Ok(n) => buf.push(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Go `cmd.Cancel`: `syscall.Kill(-pgid, SIGKILL)`. `libc::kill` with a
/// negative pid is exactly that call; errors (e.g. ESRCH when the group is
/// already gone) are ignored. `rustix::process::kill_process_group` wraps the
/// same syscall but its `process` feature is not enabled for this crate, and
/// Cargo.toml is outside this task's commit scope.
fn kill_group(pgid: u32) {
    let pgid = pgid as i32;
    // SAFETY: signalling the process group started for this command.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Go `sweepProcessGroup`: probe with signal 0; only signal a nonempty group so
/// a reused pgid is not signalled when the group has no members.
fn sweep_group(pgid: u32) {
    let pgid = pgid as i32;
    // SAFETY: kill(-pgid, 0) only probes for existence/permissions.
    let alive = unsafe { libc::kill(-pgid, 0) } == 0;
    if alive {
        // SAFETY: signalling the same process group; errors are ignored.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}
