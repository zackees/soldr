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
    ok != 0 && code == STILL_ACTIVE
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
mod tests {
    use super::*;

    #[test] // allow-bare-test: soldr-platform is a dependency leaf; timed_test! lives in soldr-core (#2493)
    fn windows_liveness_reports_current_process_alive() {
        assert!(is_alive(std::process::id()));
    }
}
