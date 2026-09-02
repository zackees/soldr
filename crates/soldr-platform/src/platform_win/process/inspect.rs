//! Windows PID inspection: liveness and running-image lookup.

use std::path::{Path, PathBuf};
use windows_sys::Win32::System::Console::{AttachConsole, FreeConsole};

/// Probe whether `pid` owns a Windows console. The caller should isolate this
/// probe because it detaches the probing process from its inherited console.
pub fn console_attached(pid: u32) -> Option<bool> {
    unsafe {
        let _ = FreeConsole();
        let attached = AttachConsole(pid) != 0;
        if attached {
            let _ = FreeConsole();
        }
        Some(attached)
    }
}

/// A process observed running from inside a directory tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessHolder {
    /// The process id.
    pub pid: u32,
    /// The fully-resolved executable image path.
    pub exe: PathBuf,
}

/// True while `pid` names a live Windows process.
///
/// # The 259 ambiguity
///
/// `GetExitCodeProcess` reports `STILL_ACTIVE` (259) for a running process --
/// and also for one that has exited with code **259**, because the sentinel is
/// drawn from the same value space as real exit codes. On that reading alone a
/// process that returned 259 stays "alive" forever.
///
/// soldr#2806 is why this is not left as a footnote: a tree-kill test reported
/// a grandchild surviving a 10s poll, and the exit-code check is the only thing
/// that observation rests on. The reading is almost certainly correct there --
/// `ping.exe` documents exits of 0 and 1 -- but "almost certainly" is the wrong
/// standard for the predicate 41 call sites and the reentrancy guard depend on.
///
/// So a `STILL_ACTIVE` answer is corroborated: a process handle becomes
/// **signalled** when the process terminates, and that has no sentinel
/// collision. If the wait says signalled, the process is gone and 259 was a
/// real exit code.
pub fn is_alive(pid: u32) -> bool {
    use std::os::windows::raw::HANDLE;
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    // PROCESS_QUERY_LIMITED_INFORMATION (not SYNCHRONIZE): restricted
    // tokens (job-object sandboxes) strip SYNCHRONIZE from process DACLs,
    // which would make every visible process read as dead.
    const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x0000_1000;
    const STILL_ACTIVE: DWORD = 259;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn GetExitCodeProcess(h: HANDLE, exit_code: *mut DWORD) -> BOOL;
        fn CloseHandle(h: HANDLE) -> BOOL;
    }
    // SAFETY: OpenProcess on a pid the caller captured; GetExitCodeProcess
    // reads one DWORD into `code`. A null handle means the process is gone.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut code: DWORD = 0;
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    unsafe { CloseHandle(handle) };
    if ok == 0 || code != STILL_ACTIVE {
        return false;
    }
    !exited_with_the_sentinel_code(pid)
}

/// Did `pid` actually terminate, despite reporting the `STILL_ACTIVE` sentinel?
///
/// Answered by whether the process handle is signalled, which happens only on
/// termination and cannot collide with an exit code.
///
/// **Returns `false` whenever it cannot tell.** This runs only to *downgrade* an
/// already-`STILL_ACTIVE` answer, so an inconclusive result must leave that
/// answer standing. In particular the `SYNCHRONIZE` open is expected to fail
/// under the restricted tokens the caller's comment describes -- and reporting
/// "exited" there would resurrect exactly the every-process-reads-as-dead bug
/// that made the caller avoid `SYNCHRONIZE` in the first place.
fn exited_with_the_sentinel_code(pid: u32) -> bool {
    use std::os::windows::raw::HANDLE;
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    const SYNCHRONIZE: DWORD = 0x0010_0000;
    const WAIT_OBJECT_0: DWORD = 0;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn WaitForSingleObject(h: HANDLE, millis: DWORD) -> DWORD;
        fn CloseHandle(h: HANDLE) -> BOOL;
    }
    // SAFETY: same pid the caller just opened; the wait is non-blocking and
    // touches no caller memory.
    let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let waited = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
    waited == WAIT_OBJECT_0
}

/// Windows reaps exited processes immediately; there is no zombie state to
/// observe.
pub fn is_zombie(_pid: u32) -> bool {
    false
}

/// Full image path for a pid, or `None` when it cannot be read.
pub fn executable_path(pid: u32) -> Option<PathBuf> {
    let path = image_path(pid)?;
    // Canonicalize so prefix comparisons compare like with like; if the
    // image is already gone, fall back to the raw path.
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

/// True when the running image's file stem matches `expected_stem`
/// (case-insensitive, as Windows paths are).
pub fn executable_stem_matches(pid: u32, expected_stem: &str) -> bool {
    executable_path(pid)
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.eq_ignore_ascii_case(expected_stem))
        .unwrap_or(false)
}

/// True when the running image resolves to `expected_path`
/// (case-insensitive, as Windows paths are).
pub fn executable_path_matches(pid: u32, expected_path: &Path) -> bool {
    let Some(actual) = executable_path(pid) else {
        return false;
    };
    let expected = std::fs::canonicalize(expected_path)
        .unwrap_or_else(|_| expected_path.to_path_buf());
    actual
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.to_string_lossy())
}

/// Every process whose image lives under `dir` (recursively).
///
/// Most failures here are ordinary and expected: system processes refuse
/// `OpenProcess` for an unelevated caller, and a process can exit between
/// the snapshot and the query. Both mean "not a holder we can name".
pub fn holders_under(dir: &Path) -> Vec<ProcessHolder> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };

    // Compare against the canonical form: the process list reports
    // fully-resolved paths, so an un-normalized `dir` (a relative path, a
    // path through a junction) would never match and the diagnosis would
    // silently report nothing.
    let Ok(root) = std::fs::canonicalize(dir) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    // SAFETY: TH32CS_SNAPPROCESS with pid 0 snapshots all processes; the
    // handle is closed on every path out.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return found;
    }

    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    // SAFETY: `entry` is zeroed with dwSize set as the API requires.
    // Process32FirstW is skipped deliberately: entry 0 is the idle
    // process, which has no image path worth querying, and Process32NextW
    // walks the rest from a fresh snapshot regardless.
    while unsafe { Process32NextW(snapshot, &mut entry) } != 0 {
        if let Some(exe) = executable_path(entry.th32ProcessID) {
            if exe.starts_with(&root) {
                found.push(ProcessHolder {
                    pid: entry.th32ProcessID,
                    exe,
                });
            }
        }
    }

    // SAFETY: `snapshot` is a valid handle from CreateToolhelp32Snapshot.
    unsafe { CloseHandle(snapshot) };
    found
}

/// A PID-reuse-safe identity token for `pid`: its creation time.
///
/// Windows reuses pids freely, so a bare pid is not an identity on its own --
/// `(pid, creation time)` is, the same pairing `terminate::is_same_process`
/// already relies on to avoid signalling a stranger once a tree-kill's
/// remembered descendant pid gets recycled. This delegates to
/// `terminate::process_creation_token` rather than opening a second
/// `GetProcessTimes` call site that could silently disagree with it.
///
/// `None` on any failure. `None` must never be treated as a match by a
/// caller comparing tokens.
pub fn process_start_token(pid: u32) -> Option<u64> {
    // Same out-of-range guard in spirit as the Unix `pid_t::try_from`
    // checks: 0 (the System Idle Process) is never a real, ownable process
    // and `OpenProcess` on it should not be trusted to answer honestly.
    if pid == 0 {
        return None;
    }
    super::terminate::process_creation_token(pid)
}

/// Raw (uncanonicalized) image path for a pid.
fn image_path(pid: u32) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, MAX_PATH};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: QUERY_LIMITED_INFORMATION is the least-privilege right that
    // still permits QueryFullProcessImageNameW; a null return is checked.
    let handle: HANDLE = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    let mut buf = [0u16; MAX_PATH as usize];
    let mut len = buf.len() as u32;
    // SAFETY: `handle` is open, `buf`/`len` describe the same buffer, and
    // `len` is updated to the written length on success.
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
    // SAFETY: `handle` came from OpenProcess above and is not used after.
    unsafe { CloseHandle(handle) };

    if ok == 0 {
        return None;
    }
    let wide = &buf[..len as usize];
    Some(PathBuf::from(std::ffi::OsString::from_wide(wide)))
}

#[cfg(test)]
#[path = "inspect_tests.rs"]
mod tests;
