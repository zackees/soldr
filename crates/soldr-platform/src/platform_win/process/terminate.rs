//! Windows process termination.

use std::io;
use std::process::{Child, Command, Stdio};

/// How a tree kill was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKill {
    /// The whole tree was killed (`taskkill /T /F` or the signal group).
    TreeKilled,
    /// The tree kill failed; the direct child was killed instead.
    ProcessKilled,
}

/// Kill `child` and its descendants, falling back to killing the child.
pub fn terminate_tree(child: &mut Child) -> io::Result<TreeKill> {
    let pid = child.id().to_string();
    let taskkill = Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match taskkill {
        Ok(status) if status.success() => Ok(TreeKill::TreeKilled),
        _ => {
            child.kill()?;
            Ok(TreeKill::ProcessKilled)
        }
    }
}

/// Terminate a single PID (`TerminateProcess` — the Windows equivalent of
/// SIGKILL; Windows has no graceful signal to offer).
pub fn terminate_pid(pid: u32) {
    let _ = signal_pid(pid, true);
}

/// Signal a single PID. Windows ignores `force`: `TerminateProcess` is the
/// only termination primitive available.
pub fn signal_pid(pid: u32, _force: bool) -> io::Result<()> {
    use std::os::windows::raw::HANDLE;
    // Win32 API spelling — clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    const PROCESS_TERMINATE: DWORD = 0x0001;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn TerminateProcess(h: HANDLE, exit_code: DWORD) -> BOOL;
        fn CloseHandle(h: HANDLE) -> BOOL;
    }
    // SAFETY: OpenProcess on a pid the caller has already verified; a null
    // return means the process is gone or inaccessible, which is success
    // for a terminate request. TerminateProcess is SIGKILL-equivalent.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Ok(());
    }
    unsafe {
        TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
    Ok(())
}
