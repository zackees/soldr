//! Windows-side injection vehicle for the file-hook tier
//! (#551 slice 6d).
//!
//! Injects an arbitrary DLL (the
//! `running-process-probe-interposer-windows.dll` payload built
//! in slices 6a–6c) into a target process by:
//!
//! 1. `OpenProcess(target_pid)` with the access rights we need:
//!    - `PROCESS_VM_OPERATION` — required for `VirtualAllocEx`.
//!    - `PROCESS_VM_WRITE` — required for `WriteProcessMemory`.
//!    - `PROCESS_CREATE_THREAD` — required for `CreateRemoteThread`.
//!    - `PROCESS_QUERY_INFORMATION` — required to be allowed to
//!      query state for diagnostics.
//! 2. `VirtualAllocEx` in the target for the wide DLL-path string.
//! 3. `WriteProcessMemory` to put the path bytes in the allocation.
//! 4. `GetProcAddress(GetModuleHandle("kernel32.dll"), "LoadLibraryW")` —
//!    the address is the same in the remote process because
//!    `kernel32.dll` is at the same base for every process in a
//!    given boot session.
//! 5. `CreateRemoteThread(target, LoadLibraryW, dll_path_addr)` —
//!    forks a thread in the target that calls
//!    `LoadLibraryW(dll_path)`.
//! 6. `WaitForSingleObject` + `GetExitCodeThread` to confirm the
//!    DLL loaded (`LoadLibraryW`'s return value, the HMODULE, ends
//!    up as the thread exit code; nonzero means success).
//! 7. `VirtualFreeEx` + `CloseHandle` cleanup.
//!
//! No admin privileges needed when the target is a child of the
//! current process (or any process the current user owns). The
//! [`#551` design body](https://github.com/zackees/running-process/issues/551)
//! has the AV / EDR-exposure rationale for keeping this code in the
//! sidecar rather than in the main `running-process` crate.
//!
//! ## Slice 6d scope (this commit)
//!
//! Function-level injection only. No spawning of the target — the
//! caller is expected to have a PID already (typically a freshly
//! `CREATE_SUSPENDED`-spawned child that's waiting in
//! `ResumeThread`). Slice 7 wires this into `NativeProcess::spawn`
//! end-to-end and lands the integration tests.
//!
//! On error, every Win32 handle / allocation acquired so far is
//! cleaned up before returning. `inject_into_pid` is no-op-safe
//! against the same PID being called twice (a second injection
//! just loads the same DLL again, which `LoadLibraryW` reference-
//! counts — slice 6b's `mem::forget` of the `RawDetour` means the
//! detours are already installed).

#![allow(unsafe_code)] // explicit per-module allow; the crate-level
                       // attr is `#![deny(unsafe_code)]`.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, FALSE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, OpenProcess, WaitForSingleObject,
    LPTHREAD_START_ROUTINE, PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION,
    PROCESS_VM_WRITE,
};

/// `WaitForSingleObject` success indicator. Same numeric value as
/// `WAIT_OBJECT_0`; declared inline to avoid an extra feature flag
/// on `windows-sys`.
const WAIT_OBJECT_0_LOCAL: u32 = 0;

/// `WaitForSingleObject` timeout indicator (`WAIT_TIMEOUT`); declared
/// inline for the same reason as [`WAIT_OBJECT_0_LOCAL`].
const WAIT_TIMEOUT_LOCAL: u32 = 0x0000_0102;

/// Hard cap on how long the injector blocks on the remote `LoadLibraryW`
/// thread before giving up (issue #590, cluster F). Without it an
/// `INFINITE` wait wedges the injector forever whenever the remote
/// `LoadLibraryW` stalls on the loader lock (another target thread holds
/// it, or the process hasn't finished loader init). Override with
/// `RUNNING_PROCESS_INJECT_WAIT_TIMEOUT_MS` (milliseconds).
const DEFAULT_INJECT_WAIT_TIMEOUT_MS: u32 = 30_000;
const INJECT_WAIT_TIMEOUT_ENV: &str = "RUNNING_PROCESS_INJECT_WAIT_TIMEOUT_MS";

fn parse_inject_wait_timeout_ms(raw: Option<&str>) -> u32 {
    raw.and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(DEFAULT_INJECT_WAIT_TIMEOUT_MS)
}

fn inject_wait_timeout_ms() -> u32 {
    parse_inject_wait_timeout_ms(std::env::var(INJECT_WAIT_TIMEOUT_ENV).ok().as_deref())
}

/// Inject `dll_path` into the process identified by `pid`.
///
/// Blocks until the remote `LoadLibraryW` call returns or the bounded
/// wait ([`DEFAULT_INJECT_WAIT_TIMEOUT_MS`], overridable) elapses; a
/// timeout yields an [`io::ErrorKind::TimedOut`] error rather than
/// wedging forever. Returns the remote `HMODULE` (the `LoadLibraryW`
/// return value) cast to `usize` — nonzero on success.
///
/// # Errors
///
/// Returns an `io::Error` carrying the `GetLastError` value if any
/// of the seven steps fails. On error, all intermediate handles and
/// allocations are released before returning.
///
/// # Safety
///
/// This function itself is safe to call from safe Rust, but it
/// drives unsafe Win32 calls internally. The caller's responsibility:
///
/// - The target process (`pid`) is one the current security
///   principal has the right to open with the access rights listed
///   in the module docs. In practice: the target is a child of the
///   current process, or another process owned by the same user.
/// - `dll_path` points to a real, loadable DLL. Validity is
///   verified by `WaitForSingleObject` returning a nonzero exit
///   code from the remote `LoadLibraryW`.
pub fn inject_into_pid(pid: u32, dll_path: &Path) -> io::Result<usize> {
    let path_wide = encode_wide(dll_path);
    if path_wide.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dll_path encoded to empty UTF-16 — refusing to inject",
        ));
    }
    let path_bytes = (path_wide.len() * 2) as u32;

    // ── Step 1: OpenProcess ──
    let process = unsafe {
        OpenProcess(
            PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_WRITE,
            FALSE,
            pid,
        )
    };
    if process.is_null() || process == INVALID_HANDLE_VALUE {
        return Err(last_error("OpenProcess"));
    }

    // From here on, every early return goes through `cleanup` so we
    // don't leak handles / remote allocations. Track each resource
    // as we acquire it.
    struct Resources {
        process: HANDLE,
        remote_alloc: *mut core::ffi::c_void,
        thread: HANDLE,
    }
    impl Drop for Resources {
        fn drop(&mut self) {
            unsafe {
                if !self.thread.is_null() && self.thread != INVALID_HANDLE_VALUE {
                    CloseHandle(self.thread);
                }
                if !self.remote_alloc.is_null() && !self.process.is_null() {
                    VirtualFreeEx(self.process, self.remote_alloc, 0, MEM_RELEASE);
                }
                if !self.process.is_null() && self.process != INVALID_HANDLE_VALUE {
                    CloseHandle(self.process);
                }
            }
        }
    }
    let mut resources = Resources {
        process,
        remote_alloc: core::ptr::null_mut(),
        thread: core::ptr::null_mut(),
    };

    // ── Step 2: VirtualAllocEx for the path string ──
    let remote_alloc = unsafe {
        VirtualAllocEx(
            process,
            core::ptr::null(),
            path_bytes as usize,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if remote_alloc.is_null() {
        return Err(last_error("VirtualAllocEx"));
    }
    resources.remote_alloc = remote_alloc;

    // ── Step 3: WriteProcessMemory ──
    let mut bytes_written: usize = 0;
    let write_ok = unsafe {
        WriteProcessMemory(
            process,
            remote_alloc,
            path_wide.as_ptr() as *const _,
            path_bytes as usize,
            &mut bytes_written,
        )
    };
    if write_ok == FALSE || bytes_written != path_bytes as usize {
        return Err(last_error("WriteProcessMemory"));
    }

    // ── Step 4: Resolve LoadLibraryW in our address space ──
    // (Same address in the target — kernel32.dll is at the same base
    // for every process in a boot session due to KASLR happening once
    // at boot, not per-process. This is a longstanding documented
    // behavior that injection vehicles rely on.)
    let kernel32_w: [u16; 13] = [
        b'k' as u16,
        b'e' as u16,
        b'r' as u16,
        b'n' as u16,
        b'e' as u16,
        b'l' as u16,
        b'3' as u16,
        b'2' as u16,
        b'.' as u16,
        b'd' as u16,
        b'l' as u16,
        b'l' as u16,
        0,
    ];
    let kernel32 = unsafe { GetModuleHandleW(kernel32_w.as_ptr()) };
    if kernel32.is_null() {
        return Err(last_error("GetModuleHandleW(kernel32.dll)"));
    }
    let load_library_w = unsafe { GetProcAddress(kernel32, c"LoadLibraryW".as_ptr() as *const u8) };
    let Some(load_library_w) = load_library_w else {
        return Err(last_error("GetProcAddress(LoadLibraryW)"));
    };
    // Cast LoadLibraryW (which has signature `LPCWSTR -> HMODULE`)
    // to the generic `LPTHREAD_START_ROUTINE` shape
    // (`*mut c_void -> u32`). The runtime calling convention is
    // identical on Win64 (rcx for the first arg, return in rax) so
    // the transmute is sound.
    let start_routine: LPTHREAD_START_ROUTINE = Some(unsafe {
        core::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
        >(load_library_w)
    });

    // ── Step 5: CreateRemoteThread ──
    let mut thread_id: u32 = 0;
    let thread = unsafe {
        CreateRemoteThread(
            process,
            core::ptr::null(),
            0,
            start_routine,
            remote_alloc,
            0,
            &mut thread_id,
        )
    };
    if thread.is_null() || thread == INVALID_HANDLE_VALUE {
        return Err(last_error("CreateRemoteThread"));
    }
    resources.thread = thread;

    // ── Step 6: Wait (bounded) + capture exit code ──
    //
    // A finite timeout instead of INFINITE (issue #590, cluster F): if the
    // remote `LoadLibraryW` stalls on the loader lock, an INFINITE wait
    // would wedge the injector forever. On timeout we leak the remote
    // allocation on purpose — the remote thread may still be reading
    // `dll_path` from it, so `VirtualFreeEx`-ing it here would be a
    // use-after-free in the target — and return a TimedOut error.
    let timeout_ms = inject_wait_timeout_ms();
    let wait = unsafe { WaitForSingleObject(thread, timeout_ms) };
    if wait == WAIT_TIMEOUT_LOCAL {
        // Prevent Drop from freeing the allocation the still-running remote
        // thread may still be reading.
        resources.remote_alloc = core::ptr::null_mut();
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "remote LoadLibraryW({}) did not return within {timeout_ms} ms; \
                 the target may be stalled on the loader lock",
                dll_path.display()
            ),
        ));
    }
    if wait != WAIT_OBJECT_0_LOCAL {
        return Err(last_error("WaitForSingleObject"));
    }
    let mut exit_code: u32 = 0;
    let get_ok = unsafe { GetExitCodeThread(thread, &mut exit_code) };
    if get_ok == FALSE {
        return Err(last_error("GetExitCodeThread"));
    }
    if exit_code == 0 {
        // LoadLibraryW returned NULL — the target couldn't load the
        // DLL. Most common cause: dll_path doesn't exist at that
        // path in the target's filesystem view, or the DLL's
        // dependencies aren't resolvable.
        return Err(io::Error::other(format!(
            "remote LoadLibraryW({}) returned NULL (exit_code=0); \
             DLL not loadable in target",
            dll_path.display()
        )));
    }

    // ── Step 7: cleanup happens via Drop. Return success. ──
    Ok(exit_code as usize)
}

/// UTF-16 LE NUL-terminated encoding of `path` suitable for the
/// remote `LoadLibraryW` argument.
fn encode_wide(path: &Path) -> Vec<u16> {
    let mut v: Vec<u16> = OsStr::new(path).encode_wide().collect();
    v.push(0);
    v
}

/// Build an `io::Error` describing a Win32 failure of `op`, using
/// `GetLastError()` to fill in the underlying code. Calls
/// `io::Error::from_raw_os_error` so the resulting error's
/// `.kind()` matches the platform's mapping.
fn last_error(op: &'static str) -> io::Error {
    let code = unsafe { GetLastError() };
    let inner = io::Error::from_raw_os_error(code as i32);
    io::Error::new(inner.kind(), format!("{op} failed: {inner}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_wait_timeout_defaults_when_unset_or_invalid() {
        assert_eq!(
            parse_inject_wait_timeout_ms(None),
            DEFAULT_INJECT_WAIT_TIMEOUT_MS
        );
        assert_eq!(
            parse_inject_wait_timeout_ms(Some("not-a-number")),
            DEFAULT_INJECT_WAIT_TIMEOUT_MS
        );
        // Zero would degrade to an unbounded-ish immediate return; reject it
        // and fall back to the default.
        assert_eq!(
            parse_inject_wait_timeout_ms(Some("0")),
            DEFAULT_INJECT_WAIT_TIMEOUT_MS
        );
    }

    #[test]
    fn inject_wait_timeout_honors_valid_override() {
        assert_eq!(parse_inject_wait_timeout_ms(Some("500")), 500);
        assert_eq!(parse_inject_wait_timeout_ms(Some("  1500  ")), 1500);
    }

    #[test]
    fn wait_timeout_constant_matches_win32() {
        // WAIT_TIMEOUT is 0x102 (258).
        assert_eq!(WAIT_TIMEOUT_LOCAL, 258);
    }
}
