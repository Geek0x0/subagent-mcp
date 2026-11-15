//! Windows managed-command runner (Task 18): `CreateProcessW` suspended-start
//! -> Job Object assign -> resume.
//!
//! This is the **one intentional security-strengthening deviation from Go**.
//! Go (`internal/tools/process_windows.go`) starts the child and *then* assigns
//! it to a Job Object, so a descendant spawned in that window can escape the
//! job. Here the job is created first, the child is created with
//! `CREATE_SUSPENDED`, assigned to the job while it cannot run, and only then
//! resumed; an assignment failure kills the suspended child and fails the call.
//! `std::process::Command` cannot expose the primary-thread handle needed to
//! resume, so this backend calls `windows-sys` directly. This replaces Task 17's
//! temporary start-then-assign fallback.
//!
//! **Differential allowlist entry (keep).** Design §5.6 / §8.2 and the plan's
//! Task 18 record this as the single approved reference deviation and state
//! that the differential suite (`tests/differential/`, Tasks 26/27) allowlists
//! this difference. Do not remove this note when the harness lands: the
//! difference must never be reported as an unexplained Go-vs-Rust diff.
//!
//! Shared contract: stdout/stderr share one pipe, drained on one
//! helper thread into a capped [`LimitedBuffer`]; the inherited-pipe
//! wait is bounded by ONE combined `wait_delay` deadline (never two sequential
//! WaitDelay periods); captured bytes are always retained; cancellation kills
//! the whole job and the bytes travel with [`cancelled_error`].

use crate::tools::process::{CommandSpec, LimitedBuffer, cancelled_error};
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io::{self, Read};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::process::{ExitStatus, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::Foundation::{
    GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

/// Fault-injection seam for `tests/windows_process.rs` only: arm the next job
/// assignment to fail so the "kill the suspended child and return an error" path
/// can be exercised end-to-end. Inert by default; no production code sets it.
static FORCE_ASSIGN_FAILURE: AtomicBool = AtomicBool::new(false);

#[doc(hidden)]
pub fn force_assignment_failure_for_test(force: bool) {
    FORCE_ASSIGN_FAILURE.store(force, Ordering::SeqCst);
}

/// Run a managed command to completion.
///
/// `cancel` is NOT inspected until after the child has been assigned to the job
/// and resumed (see [`WindowsChild::wait`]): cancellation is thereby serialized
/// with assignment and can never leave a resumed-but-unassigned child. Any
/// setup/assignment failure terminates the still-suspended child, closes every
/// handle, and returns the error.
pub fn run_managed_command(
    spec: CommandSpec,
    cancel: &CancellationToken,
    wait_delay: Duration,
) -> io::Result<Output> {
    let mut child = spawn_child(&spec)?;
    child.resume()?;
    let (status, captured, killed) = child.wait(cancel, wait_delay)?;
    // Dropping `child` terminates the whole job (KILL_ON_JOB_CLOSE), so no
    // descendant can still be alive when this returns.
    drop(child);
    if killed {
        return Err(cancelled_error(captured));
    }
    Ok(Output {
        status,
        stdout: captured,
        stderr: Vec::new(),
    })
}

/// The suspended child plus every handle it needs. Closing the job handle is
/// what triggers `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` for any descendant still
/// in the job, so `Drop` also terminates the job explicitly for belt-and-
/// braces cleanup on every path (success, cancellation, error).
pub struct WindowsChild {
    process: OwnedHandle,
    thread: Option<OwnedHandle>,
    job: OwnedHandle,
    stdout_read: Option<OwnedHandle>,
}

impl WindowsChild {
    /// Resume the suspended primary thread, then close its handle. The child
    /// MUST already be assigned to the job (see `spawn_child`).
    pub fn resume(&mut self) -> io::Result<()> {
        let thread = self
            .thread
            .take()
            .ok_or_else(|| io::Error::other("child primary thread already resumed"))?;
        // SAFETY: `thread` is the valid suspended primary thread returned by
        // CreateProcessW. ResumeThread returns -1 (u32::MAX) on failure.
        let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
        let error = (previous == u32::MAX).then(io::Error::last_os_error);
        if let Some(e) = error {
            // The child may still be suspended. Terminate it FIRST (so it can
            // never run), resume once more so the termination APC is delivered
            // to the suspended thread, and reap it before closing handles.
            // SAFETY: all handles are valid and owned by this struct.
            unsafe {
                TerminateProcess(self.process.as_raw_handle(), 1);
                TerminateJobObject(self.job.as_raw_handle(), 1);
                ResumeThread(thread.as_raw_handle());
                WaitForSingleObject(self.process.as_raw_handle(), 1_000);
            }
            drop(thread);
            return Err(e);
        }
        drop(thread); // CloseHandle, per the brief ("close the thread handle after resume").
        Ok(())
    }

    /// Terminate every process in the job (direct child + descendants). Used on
    /// cancellation; `Drop` also relies on the job close for the same effect.
    pub fn kill_job(&self) {
        // SAFETY: `job` is a valid Job Object handle owned by this child.
        unsafe {
            TerminateJobObject(self.job.as_raw_handle(), 1);
        }
    }

    /// Wait for the process to exit (honouring cancellation) and drain the shared
    /// output pipe within one `wait_delay` deadline. When that deadline expires
    /// the reader is told to stop and the call waits up to `READER_STOP_TIMEOUT`
    /// for it to exit, so the thread and its read handle are released rather than
    /// abandoned. Returns the exit status, the capped/merged/marked bytes, and
    /// whether cancellation killed a still-live child.
    pub fn wait(
        &mut self,
        cancel: &CancellationToken,
        wait_delay: Duration,
    ) -> io::Result<(ExitStatus, Vec<u8>, bool)> {
        let limited = Arc::new(Mutex::new(LimitedBuffer::new()));
        // Set once the pipe drain deadline has expired: the reader polls it
        // between peeks, so it can be released without any I/O cancellation.
        let stop = Arc::new(AtomicBool::new(false));

        // A single recv_timeout bounds inherited-pipe drain. The reader drops
        // its done_tx on exit so done_rx reports Disconnected.
        let (done_tx, done_rx) = mpsc::channel::<()>();
        // The reader owns the read end for its whole life and never blocks in a
        // `ReadFile` (it peeks first), so `stop` can always release it -- even
        // when a descendant holds the write end (see `spawn_reader`).
        if let Some(reader) = self.stdout_read.take().map(into_file) {
            spawn_reader(
                reader,
                Arc::clone(&limited),
                Arc::clone(&stop),
                done_tx.clone(),
            );
        }
        drop(done_tx);

        // Mirror Task 17: only kill a STILL-LIVE child on cancellation. A cancel
        // observed after the child exited on its own must NOT turn success into
        // `context canceled` (the `exited.is_none()` guard).
        let mut exited: Option<u32> = None;
        let mut killed = false;
        let code = loop {
            if exited.is_none() {
                exited = self.try_wait()?;
            }
            if cancel.is_cancelled() && exited.is_none() && !killed {
                self.kill_job();
                killed = true;
            }
            if let Some(c) = exited {
                break c;
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        // One deadline for process-exit pipe drain.
        let _ = done_rx.recv_timeout(wait_delay);
        // WaitDelay expiry (a surviving descendant still holds the write end, so
        // EOF never arrives) or any other path where the reader is still
        // draining: set `stop`, which the reader observes within one poll
        // interval. It then closes the read handle and exits, so waiting for that
        // (bounded) releases the thread and its handle instead of abandoning
        // them. When the reader already exited on EOF this returns immediately
        // (Disconnected).
        stop.store(true, Ordering::SeqCst);
        let _ = done_rx.recv_timeout(READER_STOP_TIMEOUT);
        drop(done_rx);

        // Never discard captured bytes on timeout; they travel with the
        // cancellation error (I-4) or the success Output.
        let captured = limited.lock().unwrap().finish();
        // SAFETY: `code` is the raw DWORD from GetExitCodeProcess.
        let status = ExitStatus::from_raw(code);
        Ok((status, captured, killed))
    }

    /// Non-blocking poll of the process handle, mirroring `Child::try_wait`.
    fn try_wait(&self) -> io::Result<Option<u32>> {
        // SAFETY: `self.process` is a live process handle owned by this child.
        let rc = unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) };
        match rc {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: valid process handle and a valid u32 out-param.
                let ok = unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Some(code))
            }
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            _ => Ok(None),
        }
    }
}

impl Drop for WindowsChild {
    fn drop(&mut self) {
        // Security guarantee on EVERY path: terminate the whole job and the
        // direct child, then closing the job handle triggers KILL_ON_JOB_CLOSE
        // for any descendant still associated with it. Terminating first also
        // reaps a suspended child whose resume() failed.
        // SAFETY: process and job are valid handles owned by this struct; the
        // calls are harmless if the process has already exited.
        unsafe {
            TerminateProcess(self.process.as_raw_handle(), 1);
            TerminateJobObject(self.job.as_raw_handle(), 1);
        }
        // self.thread / self.stdout_read drop here as well,
        // closing any handles the wait path did not already take.
    }
}

/// Serializes the process-global inheritable-handle window opened by
/// [`InheritWindow`] against every other Windows spawn in this process that
/// inherits handles without an explicit handle list.
static SPAWN_INHERIT_LOCK: Mutex<()> = Mutex::new(());

/// Proof that [`SPAWN_INHERIT_LOCK`] is held; released on drop.
///
/// [`InheritWindow`] holds one for the whole mark-inheritable -> spawn -> reset
/// window. Every other spawn that does not pass a handle list must hold one
/// around its spawn call, or it can create its child while the shell's pipe
/// write end is inheritable and inherit that write end. The codex app-server dial
/// (`crate::provider::codex::pool`) is such a spawn; it is the only other one in
/// this crate. Hold this guard across the spawn call only -- never across a wait
/// for a child, or a shell call would be stuck behind a live child.
pub(crate) struct SpawnGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

/// Take [`SPAWN_INHERIT_LOCK`] for one spawn. A poisoned lock is reused: the
/// guard protects no data, only the timing window.
pub(crate) fn spawn_guard() -> SpawnGuard {
    SpawnGuard {
        _lock: SPAWN_INHERIT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    }
}

/// Marks the child's inherited handles as inheritable for exactly the duration
/// of one `CreateProcessW` call, and restores them on every exit path.
///
/// Handle inheritance is process-global: while `HANDLE_FLAG_INHERIT` is set, any
/// concurrent spawn without an explicit handle list would inherit our pipe write
/// end. That keeps the write end open after the child exits, so the reader never
/// sees EOF and the runtime burns a full `wait_delay` per call. The window is
/// therefore taken under [`SPAWN_INHERIT_LOCK`], and every such spawn in this
/// crate takes the same lock via [`spawn_guard`], so none can overlap it. `Drop`
/// resets the flags and releases the lock even when setup fails midway.
struct InheritWindow {
    guard: Option<SpawnGuard>,
    handles: [HANDLE; 2],
    marked: usize,
}

impl InheritWindow {
    fn new(handles: [HANDLE; 2]) -> io::Result<Self> {
        let mut window = Self {
            guard: Some(spawn_guard()),
            handles,
            marked: 0,
        };
        for (index, &handle) in handles.iter().enumerate() {
            // SAFETY: handle is a valid handle we own; marking it inheritable is
            // required for the PROC_THREAD_ATTRIBUTE_HANDLE_LIST attribute.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                window.marked = index;
                return Err(io::Error::last_os_error());
            }
            window.marked = index + 1;
        }
        Ok(window)
    }

    /// Restore the inherit flags and release the spawn lock.
    fn close(&mut self) {
        for &handle in &self.handles[..self.marked] {
            // SAFETY: handle is a valid handle we own.
            let _ = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        }
        self.marked = 0;
        self.guard = None;
    }
}

impl Drop for InheritWindow {
    fn drop(&mut self) {
        self.close();
    }
}

/// All raw handles created during setup. `OwnedHandle` closes each on drop, so
/// any early `?` / error return leaks nothing.
struct Handles {
    process: OwnedHandle,
    thread: OwnedHandle,
    job: OwnedHandle,
    stdout_read: OwnedHandle,
    stdout_write: OwnedHandle,
    stdin: OwnedHandle,
    attr_list: ProcThreadAttributeList,
}

/// Create the job (with KILL_ON_JOB_CLOSE), the pipes and stdin NUL device, the
/// STARTUPINFOEXW handle list, the CREATE_SUSPENDED child, and assign it to the
/// job. The child is returned still suspended; assignment failure kills it.
fn spawn_child(spec: &CommandSpec) -> io::Result<WindowsChild> {
    // `CreateProcessW` may modify the command line, so it must be writable.
    let program = resolve_program(spec)?;
    let application = wide(program.as_os_str());
    let mut cmdline = build_command_line(program.as_os_str(), &spec.args);
    // `env_clear` semantics: the child sees EXACTLY `spec.env`, never inherits.
    let env_block = build_env_block(&spec.env);
    let cwd_wide = spec.cwd.as_ref().map(|p| wide(p.as_os_str()));

    let (stdout_read, stdout_write) = make_pipe()?;
    let stdin = open_nul()?;

    // Only these two handles are inherited: stdin (NUL) and the shared output
    // write end. The PROC_THREAD_ATTRIBUTE_HANDLE_LIST below
    // restricts inheritance to exactly them even though bInheritHandles=TRUE.
    let child_handles: [HANDLE; 2] = [stdin.as_raw_handle(), stdout_write.as_raw_handle()];

    // The inheritable window is process-global: a concurrent spawn that does not
    // pass an explicit handle list would otherwise inherit our pipe write end,
    // keep it open past its child's exit, and stall our reader. `InheritWindow`
    // holds the process-wide spawn lock across mark-inheritable -> CreateProcessW
    // -> reset, and the only other Windows spawn in this crate (the codex
    // app-server dial) takes the same lock, so the two cannot overlap. The flags
    // are restored on every exit path, including errors.
    let mut inherit = InheritWindow::new(child_handles)?;

    let job = create_job()?;
    let mut attr_list = ProcThreadAttributeList::new(&child_handles)?;

    let mut siex = STARTUPINFOEXW::default();
    siex.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    siex.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    siex.StartupInfo.hStdInput = child_handles[0];
    siex.StartupInfo.hStdOutput = child_handles[1];
    siex.StartupInfo.hStdError = child_handles[1];
    siex.lpAttributeList = attr_list.as_ptr();

    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: all pointers are valid for the duration of the call; cmdline is
    // writable and null-terminated; env_block/cwd_wide are kept alive here;
    // The explicit application prevents CreateProcessW's implicit cwd search.
    let ok = unsafe {
        CreateProcessW(
            application.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // bInheritHandles, scoped by the handle list
            CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env_block.as_ptr() as *const c_void,
            cwd_wide.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
            &siex as *const STARTUPINFOEXW as *const STARTUPINFOW,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if pi.hProcess.is_null()
        || pi.hProcess == INVALID_HANDLE_VALUE
        || pi.hThread.is_null()
        || pi.hThread == INVALID_HANDLE_VALUE
    {
        return Err(io::Error::other("CreateProcessW returned invalid handles"));
    }

    // Close the inheritable window as soon as the child exists: reset the
    // inherit flags and release the process-wide spawn lock so a waiting spawn
    // (the codex dial) can proceed. Doing it before the job assignment also keeps
    // the critical section short.
    inherit.close();

    // SAFETY: CreateProcessW succeeded and the handles are ours to close.
    let h = Handles {
        process: unsafe { OwnedHandle::from_raw_handle(pi.hProcess) },
        thread: unsafe { OwnedHandle::from_raw_handle(pi.hThread) },
        job,
        stdout_read,
        stdout_write,
        stdin,
        attr_list,
    };

    // Cancellation is serialized with assignment: the token is not read until
    // `wait` (after resume), so a cancellation can never resume an unassigned
    // child. Assignment itself must succeed before resume.
    let assigned = if FORCE_ASSIGN_FAILURE.load(Ordering::SeqCst) {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "forced job assignment failure (test seam)",
        ))
    } else {
        // SAFETY: job and process are valid handles we own.
        if unsafe { AssignProcessToJobObject(h.job.as_raw_handle(), h.process.as_raw_handle()) }
            != 0
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    };
    if let Err(e) = assigned {
        // Kill the STILL-SUSPENDED child: terminate first so it cannot run or
        // spawn any descendant outside the job, then resume its primary thread
        // so the terminated suspended process is reaped, and bound the wait.
        // SAFETY: process/thread/job are valid handles we own.
        unsafe {
            TerminateProcess(h.process.as_raw_handle(), 1);
            TerminateJobObject(h.job.as_raw_handle(), 1);
            ResumeThread(h.thread.as_raw_handle());
            WaitForSingleObject(h.process.as_raw_handle(), 1_000);
        }
        // `h` drops here: every handle closes and the job close triggers
        // KILL_ON_JOB_CLOSE.
        return Err(io::Error::new(
            e.kind(),
            format!("failed to assign child to Job Object: {e}"),
        ));
    }

    // Hand ownership to WindowsChild; close the parent's copies of the child's
    // write ends and NUL stdin so the child's inherited copies are the only
    // ones (readers see EOF as soon as the job terminates).
    let Handles {
        process,
        thread,
        job,
        stdout_read,
        stdout_write,
        stdin,
        attr_list,
    } = h;
    drop(stdout_write);
    drop(stdin);
    drop(attr_list);
    Ok(WindowsChild {
        process,
        thread: Some(thread),
        job,
        stdout_read: Some(stdout_read),
    })
}

/// RAII owner of the PROC_THREAD_ATTRIBUTE_LIST buffer. `Vec<usize>` guarantees
/// pointer alignment for the attribute list; `Drop` deletes the list.
struct ProcThreadAttributeList {
    buf: Vec<usize>,
}

impl ProcThreadAttributeList {
    fn new(handles: &[HANDLE]) -> io::Result<Self> {
        let mut size: usize = 0;
        // SAFETY: a null list with attribute count 1 is the documented way to
        // query the required buffer size; the call is expected to "fail".
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        }
        if size == 0 {
            return Err(io::Error::other(
                "InitializeProcThreadAttributeList reported a zero size",
            ));
        }
        let mut buf = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list = buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        // SAFETY: buf is aligned (Vec<usize>) and large enough for `size`.
        let ok = unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the list is initialized; handles describes a valid HANDLE
        // array that outlives this call.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr() as *const c_void,
                std::mem::size_of_val(handles),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            let e = io::Error::last_os_error();
            // SAFETY: the list was initialized above.
            unsafe { DeleteProcThreadAttributeList(list) };
            return Err(e);
        }
        Ok(Self { buf })
    }

    fn as_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was successfully initialized in `new`.
        unsafe {
            DeleteProcThreadAttributeList(self.buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST);
        }
    }
}

/// Create an anonymous pipe with non-inheritable handles.
fn make_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: the out-params are valid HANDLE slots; a null SECURITY_ATTRIBUTES
    // creates non-inheritable handles until SetHandleInformation opts in.
    let ok = unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe succeeded; both handles are now owned by us.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

/// Open the NUL device for the child's stdin (Go's `os.DevNull`).
fn open_nul() -> io::Result<OwnedHandle> {
    let name = wide(OsStr::new("NUL"));
    // SAFETY: `name` is a valid null-terminated PCWSTR. On success the handle
    // is ours to close.
    let h = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

/// Create the Job Object and set JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE so closing
/// the last job handle (on any path) terminates every process in the job.
fn create_job() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes/name create an unnamed, private Job Object.
    let h = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: h is a valid job handle we own.
    let job = unsafe { OwnedHandle::from_raw_handle(h) };

    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `info` is a fully-initialized zeroed structure whose length
    // matches the JobObjectExtendedLimitInformation information class.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

/// Convert an owned pipe read handle into a `File` so the reader thread can
/// use the standard `Read` implementation (and own/close the handle).
fn into_file(h: OwnedHandle) -> File {
    // SAFETY: h is a valid pipe read handle we own; File takes ownership and
    // closes it on drop.
    unsafe { File::from_raw_handle(h.into_raw_handle()) }
}

/// How often the reader peeks at the pipe while it is idle. Data that is
/// already buffered is read immediately (no sleep), so this only bounds how long
/// new output waits and how long `stop` takes to be observed.
const PIPE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long `wait` waits for the reader to exit after setting `stop`: one poll
/// interval plus the time to drain what was already buffered, i.e. still inside
/// the `wait_delay + 300 ms` bound the Windows WaitDelay test asserts. Bounded,
/// so a reader that somehow never comes back cannot hold the call.
const READER_STOP_TIMEOUT: Duration = Duration::from_millis(250);

/// Drain the pipe read end into the shared capped buffer until EOF, or until
/// `stop` is set.
///
/// The handle is owned here for the thread's whole lifetime and closed when the
/// loop ends. Nothing here may block in `ReadFile` -- that is what makes the
/// thread releasable on WaitDelay expiry, when a surviving descendant still holds
/// the write end and EOF never arrives: this crate's enabled `windows-sys`
/// features have no I/O cancellation (`CancelSynchronousIo`/`CancelIoEx` live
/// behind `Win32_System_IO`), and closing the handle from the waiting thread
/// would not unblock a `ReadFile` already in progress. So the loop peeks:
/// `PeekNamedPipe` does not block, reports how many bytes are already buffered
/// (0 when none have arrived yet), and fails once every write end is closed.
/// A read is issued for at most the peeked byte count, which is exactly what is
/// available, so it returns without waiting for more output.
///
/// Draining is not cut short: a pipe whose writers are gone but which still holds
/// buffered data reports that data first, so the loop reads it before the peek
/// starts failing, and only then is the failure treated as EOF. `done_tx` stays
/// alive for the thread's whole lifetime so dropping it signals `done_rx`.
fn spawn_reader(
    mut reader: File,
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
            let mut available: u32 = 0;
            // SAFETY: `reader` owns a live pipe read handle for the whole loop;
            // the null buffer/out-params are the documented "count only" form of
            // the call, and `available` is a valid u32 out-param.
            let ok = unsafe {
                PeekNamedPipe(
                    reader.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                // ERROR_BROKEN_PIPE (every write end closed, buffer drained) or
                // an unusable handle: there is nothing left to read.
                break;
            }
            if available == 0 {
                std::thread::sleep(PIPE_POLL_INTERVAL);
                continue;
            }
            // Never ask for more than the peek reported, so this cannot block.
            let want = (available as usize).min(chunk.len());
            match reader.read(&mut chunk[..want]) {
                Ok(0) => break,
                Ok(n) => limited.lock().unwrap().push(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
}

/// Convert an `OsStr` to a null-terminated UTF-16 buffer.
fn wide(s: &OsStr) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_wide().collect();
    v.push(0);
    v
}

/// Append one argument using the Windows command-line quoting rules (the same
/// algorithm Rust's `std::process::Command` uses): quote when whitespace or a
/// quote is present, double backslashes preceding a quote, and double trailing
/// backslashes before a closing quote.
fn append_arg(cmd: &mut Vec<u16>, arg: &OsStr) {
    let chars: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = chars.is_empty()
        || chars
            .iter()
            .any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'"' as u16);
    if needs_quotes {
        cmd.push(b'"' as u16);
    }
    let mut backslashes = 0usize;
    for &c in &chars {
        if c == b'\\' as u16 {
            backslashes += 1;
        } else if c == b'"' as u16 {
            for _ in 0..backslashes * 2 + 1 {
                cmd.push(b'\\' as u16);
            }
            cmd.push(b'"' as u16);
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                cmd.push(b'\\' as u16);
            }
            cmd.push(c);
            backslashes = 0;
        }
    }
    if needs_quotes {
        for _ in 0..backslashes * 2 {
            cmd.push(b'\\' as u16);
        }
        cmd.push(b'"' as u16);
    } else {
        for _ in 0..backslashes {
            cmd.push(b'\\' as u16);
        }
    }
}

/// Resolve bare executable names via PATH/PATHEXT without implicit cwd search.
fn resolve_program(spec: &CommandSpec) -> io::Result<std::path::PathBuf> {
    use std::path::{Path, PathBuf};
    let program = Path::new(&spec.program);
    let cwd = match &spec.cwd {
        Some(cwd) => std::path::absolute(cwd)?,
        None => std::env::current_dir()?,
    };
    if program.components().count() > 1
        || program.has_root()
        || spec.program.to_string_lossy().contains(['/', '\\', ':'])
    {
        return Ok(cwd.join(program));
    }
    let env = |key: &str| {
        spec.env
            .iter()
            .find(|(name, _)| name.to_string_lossy().eq_ignore_ascii_case(key))
            .map(|(_, value)| value.clone())
    };
    let path = env("PATH").unwrap_or_default();
    let extensions = env("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
    let mut names = Vec::new();
    if program.extension().is_some() {
        names.push(PathBuf::from(program));
    }
    for extension in extensions
        .to_string_lossy()
        .split(';')
        .filter(|e| !e.is_empty())
    {
        let mut name = spec.program.clone();
        name.push(extension);
        names.push(PathBuf::from(name));
    }
    for dir in std::env::split_paths(&path).filter(|p| !p.as_os_str().is_empty()) {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                // Go's LookPath refuses an executable found via a relative
                // PATH entry (ErrDot); never silently trust the current repo.
                if !candidate.is_absolute() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "refusing executable resolved relative to current directory: {}",
                            candidate.display()
                        ),
                    ));
                }
                return Ok(candidate);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("executable {:?} not found in PATH", spec.program),
    ))
}

/// Build the command line with the same resolved program as lpApplicationName.
fn build_command_line(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut cmd = Vec::new();
    append_arg(&mut cmd, program);
    for arg in args {
        cmd.push(b' ' as u16);
        append_arg(&mut cmd, arg);
    }
    cmd.push(0);
    cmd
}

/// Build the double-null-terminated Unicode environment block. Windows wants
/// the block sorted alphabetically (case-insensitively) by variable name.
fn build_env_block(env: &[(OsString, OsString)]) -> Vec<u16> {
    if env.is_empty() {
        return vec![0, 0];
    }
    let mut pairs: Vec<(Vec<u16>, Vec<u16>)> = env
        .iter()
        .map(|(k, v)| (k.encode_wide().collect(), v.encode_wide().collect()))
        .collect();
    pairs.sort_by(|a, b| {
        let lower = |s: &Vec<u16>| -> Vec<u16> {
            s.iter()
                .map(|&c| {
                    if c < 128 {
                        (c as u8).to_ascii_lowercase() as u16
                    } else {
                        c
                    }
                })
                .collect()
        };
        lower(&a.0).cmp(&lower(&b.0))
    });
    let mut block = Vec::new();
    for (k, v) in pairs {
        block.extend(k);
        block.push(b'=' as u16);
        block.extend(v);
        block.push(0);
    }
    block.push(0);
    block
}
