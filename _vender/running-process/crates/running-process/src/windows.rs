#![cfg(windows)]

use std::process::Child;
use std::sync::mpsc::Sender;

use crate::observer::ObserverEvent;

pub(crate) struct WindowsJobHandle {
    pub(crate) job: usize,
    /// IOCP associated with the Job Object for #539 slice 2 — the
    /// descendant-lifecycle pump thread reads JOB_OBJECT_MSG_* messages
    /// off this port. `None` means observation wasn't requested at spawn
    /// time and the pump thread is not running.
    iocp: Option<usize>,
}

impl Drop for WindowsJobHandle {
    fn drop(&mut self) {
        unsafe {
            // Close the Job first: KILL_ON_JOB_CLOSE then drives every
            // process in the job to exit, which fires
            // ACTIVE_PROCESS_ZERO on the IOCP and lets the pump thread
            // wind down cleanly.
            winapi::um::handleapi::CloseHandle(self.job as winapi::shared::ntdef::HANDLE);
            if let Some(port) = self.iocp.take() {
                // Closing the port unblocks GetQueuedCompletionStatus
                // with an error, which is the pump thread's secondary
                // exit signal (primary is ACTIVE_PROCESS_ZERO).
                winapi::um::handleapi::CloseHandle(port as winapi::shared::ntdef::HANDLE);
            }
        }
    }
}

/// Parent-side handles for the captured stdout/stderr pipes, kept so
/// that `kill_impl` can call `CancelIoEx` to interrupt a reader thread
/// blocked in `read()`. Stored as `usize` because `RawHandle` (a raw
/// pointer) is not `Send` and we share this via `Arc<Mutex<...>>`.
///
/// The reader thread clears its slot (under the mutex) immediately
/// before dropping its `ChildStd*`, so `kill_impl` never calls
/// `CancelIoEx` on a closed (and potentially reused) handle.
#[derive(Default)]
pub(crate) struct CapturePipeHandles {
    pub(crate) stdout: Option<usize>,
    pub(crate) stderr: Option<usize>,
}

pub(crate) fn assign_child_to_windows_kill_on_close_job_impl(
    child: &Child,
) -> Result<WindowsJobHandle, std::io::Error> {
    assign_child_to_windows_kill_on_close_job_with_observer_impl(child, None, 0)
}

/// Like [`assign_child_to_windows_kill_on_close_job_impl`] but also wires
/// the Job Object to an IOCP that fires
/// [`ObserverEventKind::DescendantStarted`](crate::observer::ObserverEventKind::DescendantStarted)
/// / [`DescendantExited`](crate::observer::ObserverEventKind::DescendantExited)
/// events for every process the child spawns into the Job. #539 slice 2.
///
/// When `descendant_sink` is `Some(tx)`, an IOCP is created and associated
/// with the Job via `JobObjectAssociateCompletionPortInformation`, and a
/// background pump thread is spawned that reads
/// `JOB_OBJECT_MSG_NEW_PROCESS` / `EXIT_PROCESS` /
/// `ABNORMAL_EXIT_PROCESS` / `ACTIVE_PROCESS_ZERO` and forwards them as
/// observer events on the provided sender. The directly-spawned child's
/// `NEW_PROCESS`/`EXIT_PROCESS` notifications (PID == `direct_pid`) are
/// suppressed because the `Lifecycle` category already covers them via
/// `ObserverEmitter::emit_started` / `emit_exited`.
///
/// When `descendant_sink` is `None`, no IOCP is created and behavior is
/// identical to the bare variant.
pub(crate) fn assign_child_to_windows_kill_on_close_job_with_observer_impl(
    child: &Child,
    descendant_sink: Option<Sender<ObserverEvent>>,
    direct_pid: u32,
) -> Result<WindowsJobHandle, std::io::Error> {
    crate::rp_rust_debug_scope!("running_process::assign_child_to_windows_kill_on_close_job");
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;

    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::jobapi2::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    };
    use winapi::um::winnt::{
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    let handle = child.as_raw_handle();
    let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
    if job.is_null() || job == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    // BREAKAWAY_OK does not weaken containment on its own: a child escapes
    // only if it explicitly passes CREATE_BREAKAWAY_FROM_JOB. Without it,
    // that request fails with ERROR_ACCESS_DENIED and long-lived daemons
    // spawned beneath this job are killed when the job handle drops.
    info.BasicLimitInformation.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        return Err(err);
    }

    // Wire up the IOCP pump BEFORE assigning the child to the Job, so the
    // child's own NEW_PROCESS notification is reliably queued onto the
    // port and observed by the pump thread (otherwise the assign could
    // race ahead of the SetInformationJobObject call below).
    let iocp = match descendant_sink {
        Some(sink) => match attach_iocp_pump(job, sink, direct_pid) {
            Ok(port) => Some(port),
            Err(err) => {
                unsafe { CloseHandle(job) };
                return Err(err);
            }
        },
        None => None,
    };

    let ok = unsafe { AssignProcessToJobObject(job, handle.cast()) };
    if ok == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe { CloseHandle(job) };
        if let Some(port) = iocp {
            unsafe { CloseHandle(port as winapi::shared::ntdef::HANDLE) };
        }
        return Err(err);
    }

    Ok(WindowsJobHandle {
        job: job as usize,
        iocp,
    })
}

/// Create an IOCP, associate it with `job`, and spawn the pump thread that
/// forwards descendant-lifecycle events on `sink`.
///
/// Returns the IOCP HANDLE as `usize` so it can be stored alongside the
/// Job HANDLE for symmetric drop / cleanup.
fn attach_iocp_pump(
    job: winapi::shared::ntdef::HANDLE,
    sink: Sender<ObserverEvent>,
    direct_pid: u32,
) -> Result<usize, std::io::Error> {
    use std::mem::zeroed;
    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::ioapiset::CreateIoCompletionPort;
    use winapi::um::jobapi2::SetInformationJobObject;
    use winapi::um::winnt::{
        JobObjectAssociateCompletionPortInformation, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
    };

    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    if port.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    let mut assoc: JOBOBJECT_ASSOCIATE_COMPLETION_PORT = unsafe { zeroed() };
    assoc.CompletionKey = job as winapi::shared::ntdef::PVOID;
    assoc.CompletionPort = port;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectAssociateCompletionPortInformation,
            (&mut assoc as *mut JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(),
            std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
        )
    };
    if ok == FALSE {
        let err = std::io::Error::last_os_error();
        unsafe { winapi::um::handleapi::CloseHandle(port) };
        return Err(err);
    }

    let port_usize = port as usize;
    std::thread::Builder::new()
        .name("rp-job-iocp-pump".to_string())
        .spawn(move || iocp_pump_loop(port_usize, sink, direct_pid))
        .map_err(|e| {
            unsafe { winapi::um::handleapi::CloseHandle(port) };
            std::io::Error::other(format!("spawn IOCP pump thread: {e}"))
        })?;

    Ok(port_usize)
}

/// IOCP pump loop. Runs on a dedicated thread until either
/// `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` fires or the port is closed by
/// [`WindowsJobHandle::drop`].
fn iocp_pump_loop(port_usize: usize, sink: Sender<ObserverEvent>, direct_pid: u32) {
    use winapi::shared::minwindef::{DWORD, FALSE, LPDWORD};
    use winapi::um::ioapiset::GetQueuedCompletionStatus;
    use winapi::um::minwinbase::LPOVERLAPPED;

    // JOB_OBJECT_MSG_* constants (winnt.h). winapi exposes them in
    // winapi::um::winnt but they are gated behind features that aren't on
    // by default in our dependency footprint, so define the ones we need
    // locally — these values are part of the stable Win32 ABI.
    const JOB_OBJECT_MSG_END_OF_JOB_TIME: u32 = 1;
    const JOB_OBJECT_MSG_END_OF_PROCESS_TIME: u32 = 2;
    const JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT: u32 = 3;
    const JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO: u32 = 4;
    const JOB_OBJECT_MSG_NEW_PROCESS: u32 = 6;
    const JOB_OBJECT_MSG_EXIT_PROCESS: u32 = 7;
    const JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;
    let _ = (
        JOB_OBJECT_MSG_END_OF_JOB_TIME,
        JOB_OBJECT_MSG_END_OF_PROCESS_TIME,
        JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT,
    );

    let port = port_usize as winapi::shared::ntdef::HANDLE;
    loop {
        let mut bytes_transferred: DWORD = 0;
        let mut completion_key: usize = 0;
        let mut overlapped: LPOVERLAPPED = std::ptr::null_mut();
        let ok = unsafe {
            GetQueuedCompletionStatus(
                port,
                &mut bytes_transferred as LPDWORD,
                &mut completion_key as *mut usize as *mut _,
                &mut overlapped as *mut LPOVERLAPPED,
                winapi::um::winbase::INFINITE,
            )
        };
        if ok == FALSE {
            // Port closed or transient error → bail out. KILL_ON_JOB_CLOSE
            // is the safety net for any unobserved descendant still alive.
            break;
        }
        // For Job Object completion notifications: `dwNumberOfBytesTransferred`
        // is the message code (e.g. JOB_OBJECT_MSG_NEW_PROCESS); the PID is
        // carried in `lpOverlapped` cast to integer. See Microsoft docs:
        // SetInformationJobObject (JobObjectAssociateCompletionPortInformation).
        let msg = bytes_transferred as u32;
        let pid = overlapped as usize as u32;

        match msg {
            JOB_OBJECT_MSG_NEW_PROCESS => {
                if pid == direct_pid {
                    // Direct child's NEW_PROCESS is covered by the
                    // Lifecycle category; suppress to avoid double-firing.
                    continue;
                }
                let _ = sink.send(ObserverEvent::new_now(
                    crate::observer::EventCategory::Process,
                    crate::observer::ObserverEventKind::DescendantStarted,
                    pid,
                ));
            }
            JOB_OBJECT_MSG_EXIT_PROCESS | JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS => {
                if pid == direct_pid {
                    continue;
                }
                let _ = sink.send(ObserverEvent::new_now(
                    crate::observer::EventCategory::Process,
                    crate::observer::ObserverEventKind::DescendantExited,
                    pid,
                ));
            }
            JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO => {
                // Last process in the Job exited → no more notifications
                // can arrive. Drop the sink and exit the pump.
                break;
            }
            _ => {
                // Unhandled message kinds (END_OF_JOB_TIME, etc.) — ignore.
            }
        }
    }
}

pub(crate) fn windows_priority_flags(nice: Option<i32>) -> u32 {
    const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;

    match nice {
        Some(value) if value >= 15 => IDLE_PRIORITY_CLASS,
        Some(value) if value >= 1 => BELOW_NORMAL_PRIORITY_CLASS,
        Some(value) if value <= -15 => HIGH_PRIORITY_CLASS,
        Some(value) if value <= -1 => ABOVE_NORMAL_PRIORITY_CLASS,
        _ => 0,
    }
}

/// True when the running process is attached to a console.
///
/// Console-less parents (the detached daemon) are the #584 flash scenario;
/// console-attached parents pass their console to children by inheritance,
/// so no window is created for the child in the first place.
///
/// Probes ATTACHMENT (`GetConsoleCP() != 0`), not `GetConsoleWindow`: a
/// process can be attached to a hidden, windowless console (CI runners,
/// agent harnesses), where the window handle is null but CTRL_C delivery
/// across the shared console works — exactly the case the #622 gate must
/// treat as console-attached.
pub(crate) fn parent_has_console() -> bool {
    unsafe { windows_sys::Win32::System::Console::GetConsoleCP() != 0 }
}

/// Compute the full Windows `creation_flags` for a spawned child.
///
/// # `CREATE_NO_WINDOW` default (#584), gated on a console-less parent (#622)
///
/// A `running-process` daemon runs detached (no console). When such a
/// window-less parent launches a console-subsystem child without
/// `CREATE_NO_WINDOW`, Windows allocates a fresh console window for the
/// child — a visible flash for the child's lifetime. Redirecting stdio to
/// pipes does not suppress it; only the creation flag does. So we default
/// children of console-LESS parents to `CREATE_NO_WINDOW`.
///
/// The default MUST NOT apply when the parent has a console
/// (`parent_has_console = true`): the child then inherits the existing
/// console — no new window can flash — and forcing `CREATE_NO_WINDOW`
/// would detach the child onto an invisible console of its own, which
/// breaks `GenerateConsoleCtrlEvent` CTRL_C/CTRL_BREAK delivery between
/// parent and child (issue #622 — six KeyboardInterrupt integration tests
/// went red on every Windows CI run after #585 applied the default
/// unconditionally).
///
/// The default is also suppressed when the caller has already expressed a
/// console opinion through `creationflags` — passing any of
/// `CREATE_NO_WINDOW`, `CREATE_NEW_CONSOLE`, or `DETACHED_PROCESS` opts out
/// of the injected default (`CREATE_NEW_CONSOLE` is the way to *keep* a
/// visible console). Priority (`nice`) and `CREATE_NEW_PROCESS_GROUP` bits
/// are OR-ed in and never clobbered.
pub(crate) fn windows_creation_flags(
    creationflags: Option<u32>,
    create_process_group: bool,
    nice: Option<i32>,
    parent_has_console: bool,
) -> u32 {
    // CREATE_NEW_PROCESS_GROUP makes GenerateConsoleCtrlEvent with
    // CTRL_BREAK_EVENT route to this child's group (rather than the
    // daemon's group) — required for the pipe-session soft-signal path
    // on Windows (#130 M4).
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let caller = creationflags.unwrap_or(0);
    let group = if create_process_group {
        CREATE_NEW_PROCESS_GROUP
    } else {
        0
    };
    // #584: hide the console by default for console-less parents; respect
    // a caller that already dictates console behaviour via any
    // console-related creation flag, and never detach a console-attached
    // parent's child from the shared console (#622).
    let caller_has_console_opinion =
        caller & (CREATE_NO_WINDOW | CREATE_NEW_CONSOLE | DETACHED_PROCESS) != 0;
    let no_window = if caller_has_console_opinion || parent_has_console {
        0
    } else {
        CREATE_NO_WINDOW
    };

    caller | group | no_window | windows_priority_flags(nice)
}
