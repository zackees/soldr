//! Windows DLL-injection interposer for the running-process
//! file-hook tier (#551 slice 6).
//!
//! Built as a cdylib `running_process_probe_interposer_windows.dll`.
//! Unlike the Linux LD_PRELOAD and macOS DYLD_INSERT_LIBRARIES
//! interposers — which the dynamic linker loads automatically when
//! the appropriate env var is set — Windows has no equivalent loader
//! env var. Injection happens via:
//!
//! 1. The parent (running-process daemon) allocates memory in the
//!    target process via `VirtualAllocEx` and writes the path to
//!    this DLL into it.
//! 2. The parent calls
//!    `CreateRemoteThread(target, LoadLibraryW, dll_path_ptr)`,
//!    which forks a thread in the target that calls
//!    `LoadLibraryW(dll_path)`.
//! 3. `LoadLibraryW` brings this DLL into the target's address space
//!    and calls our `DllMain` with `DLL_PROCESS_ATTACH`.
//! 4. `DllMain` installs `retour`-backed inline detours on the Win32
//!    file APIs. Each detour calls the original via a trampoline,
//!    emitting an `RPP_HOOK …` line on stderr matching the
//!    Linux + macOS interposer format.
//!
//! ## AV / EDR exposure
//!
//! The injection vehicle (`CreateRemoteThread` + `LoadLibraryW`) is
//! the prototypical "process injection" pattern AV/EDR products flag
//! aggressively. The `#551` design body documents the mitigation:
//! injection lives in the **sidecar helper binary**
//! (`running-process-probe-agent`, slices 1–2) which is embedded
//! in the `running-process-probe` crate via `include_bytes!` and
//! extracted to a per-user cache at first use. The main
//! `running-process` crate stays free of injection symbols entirely.
//! This DLL (the **payload**) doesn't itself call the injection
//! primitives; only the sidecar does.
//!
//! ## Slice 6c scope (this commit)
//!
//! Bundle the remaining four file-mutation detours
//! (`WriteFile`, `CloseHandle`, `DeleteFileW`, `MoveFileExW`) on top
//! of the `CreateFileW` detour landed in slice 6b. Adds a
//! process-global HANDLE→path table so close/write events can
//! resolve back to the original path the way the macOS interposer
//! does via `fcntl(F_GETPATH)` and the Linux one via
//! `/proc/self/fd/<n>`. Matches the surface area the macOS slice 5b
//! bundle shipped.
//!
//! `CloseHandle` is polymorphic on Windows — it closes any HANDLE
//! type (file, socket, mutex, thread, ...). We only emit
//! `file-close` for handles that appear in our table, so non-file
//! closes pass through silently.
//!
//! `MoveFileExW` is the rename/move primitive (`MoveFileW` is a
//! thin wrapper that delegates to it on modern Windows). Detouring
//! `MoveFileExW` catches both.
//!
//! Slice 6d adds the sidecar-side injection vehicle that drives
//! `CreateRemoteThread(LoadLibraryW, dll_path)` into freshly
//! spawned children of the running-process daemon.

// Gated on Windows x86_64 only: `retour` 0.4.0-alpha.4 uses
// `iced-x86` for prologue disassembly, which doesn't support
// ARM64. On Windows ARM64 the crate falls through to an empty
// stub so the workspace still builds end-to-end — matching the
// per-target dep gate in Cargo.toml. Slice 6b's CreateFileW
// detour and friends literally cannot be implemented on ARM64
// with the current retour, so reporting "feature unavailable"
// honestly is the right answer.
#![cfg(all(target_os = "windows", target_arch = "x86_64"))]

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use retour::RawDetour;
use windows_sys::Win32::Foundation::{
    CloseHandle, BOOL, FALSE, HANDLE, INVALID_HANDLE_VALUE, TRUE,
};
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows_sys::Win32::System::Threading::CreateThread;

// ── Function-pointer types ──

type CreateFileWFn = unsafe extern "system" fn(
    *const u16,               // lpFileName (LPCWSTR)
    u32,                      // dwDesiredAccess
    u32,                      // dwShareMode
    *const core::ffi::c_void, // lpSecurityAttributes
    u32,                      // dwCreationDisposition
    u32,                      // dwFlagsAndAttributes
    HANDLE,                   // hTemplateFile
) -> HANDLE;

type WriteFileFn = unsafe extern "system" fn(
    HANDLE,                 // hFile
    *const u8,              // lpBuffer
    u32,                    // nNumberOfBytesToWrite
    *mut u32,               // lpNumberOfBytesWritten
    *mut core::ffi::c_void, // lpOverlapped (OVERLAPPED*)
) -> BOOL;

type CloseHandleFn = unsafe extern "system" fn(HANDLE) -> BOOL;

type DeleteFileWFn = unsafe extern "system" fn(*const u16) -> BOOL;

type MoveFileExWFn = unsafe extern "system" fn(*const u16, *const u16, u32) -> BOOL;

// ── Trampolines ──

/// Pointer to the trampoline that calls the original `CreateFileW`.
/// Populated when the detour is enabled in `install_detours()`.
///
/// We can't store a `CreateFileWFn` directly in an `AtomicPtr<F>`
/// because fn-pointers and data-pointers have different niches in
/// Rust's typesystem. Instead store the raw bytes and transmute
/// at the call site.
static REAL_CREATE_FILE_W: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static REAL_WRITE_FILE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static REAL_CLOSE_HANDLE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static REAL_DELETE_FILE_W: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static REAL_MOVE_FILE_EX_W: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

thread_local! {
    /// Reentrancy guard. We never want our hook body to re-enter
    /// itself (e.g. if `format!` inside emit allocates and the
    /// allocator opens a heap-backed file). Same pattern as the
    /// Linux + macOS interposers.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

// ── HANDLE → path table ──

/// Process-global HANDLE→path map. Populated on successful
/// `CreateFileW`, queried on `WriteFile` / `CloseHandle`, removed
/// on `CloseHandle`. Same purpose as the Linux/macOS interposers'
/// fd_table; the key here is the raw handle value cast to `isize`
/// (HANDLE is `*mut c_void`, but we never dereference it — only
/// use it as a token).
static HANDLE_TABLE: OnceLock<Mutex<HashMap<isize, String>>> = OnceLock::new();

fn handle_table() -> &'static Mutex<HashMap<isize, String>> {
    HANDLE_TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn handle_table_insert(handle: HANDLE, path: String) {
    if let Ok(mut tbl) = handle_table().lock() {
        tbl.insert(handle as isize, path);
    }
}

fn handle_table_get(handle: HANDLE) -> Option<String> {
    handle_table().lock().ok()?.get(&(handle as isize)).cloned()
}

fn handle_table_remove(handle: HANDLE) -> Option<String> {
    handle_table().lock().ok()?.remove(&(handle as isize))
}

// ── Event emission ──

/// Write `bytes` to stderr.
///
/// Goes through the `WriteFile` trampoline once that detour is
/// installed (so we don't observe our own emissions); falls back
/// to the un-detoured `WriteFile` import while the trampoline
/// isn't ready (i.e. before `install_detours()` finishes, or if
/// installation failed). The `IN_HOOK` reentrancy guard handles
/// the case where another hook called this and the `WriteFile`
/// detour fires — it short-circuits the emit-recursion path.
/// Bounded queue of pending `RPP_HOOK …` lines plus a wakeup condvar.
/// `emit_bytes` enqueues here (never blocks); a dedicated drain thread
/// writes them to stderr with a blocking `WriteFile`. This decouples the
/// host process's own file operations from stderr backpressure (issue
/// #590, cluster E): previously a full/undrained stderr pipe blocked the
/// blocking `WriteFile` *inside every hooked file op*, wedging the host
/// thread. Now backpressure only slows the drain thread, and the queue
/// drops its oldest lines once it fills.
struct EmitQueue {
    lines: Mutex<VecDeque<Vec<u8>>>,
    wakeup: Condvar,
}

static EMIT_QUEUE: OnceLock<EmitQueue> = OnceLock::new();

/// Cap on buffered lines. Oldest lines are dropped on overflow so a
/// stalled stderr can never make `emit_bytes` block or grow memory without
/// bound.
const EMIT_QUEUE_MAX_LINES: usize = 4096;

fn emit_queue() -> &'static EmitQueue {
    EMIT_QUEUE.get_or_init(|| EmitQueue {
        lines: Mutex::new(VecDeque::new()),
        wakeup: Condvar::new(),
    })
}

/// Enqueue an `RPP_HOOK` line for the drain thread. Non-blocking: takes a
/// brief mutex, drops the oldest line on overflow, and returns — a hooked
/// file op never blocks on stderr.
fn emit_bytes(bytes: &[u8]) {
    let queue = emit_queue();
    if let Ok(mut lines) = queue.lines.lock() {
        if lines.len() >= EMIT_QUEUE_MAX_LINES {
            lines.pop_front();
        }
        lines.push_back(bytes.to_vec());
        queue.wakeup.notify_one();
    }
}

/// Blocking write of one line to stderr. Runs only on the drain thread, so
/// blocking here never affects a host file op. Prefers the `WriteFile`
/// trampoline (the un-detoured original) so the drain's own write does not
/// re-enter the `WriteFile` detour.
fn write_line_to_stderr(bytes: &[u8]) {
    unsafe {
        let h = GetStdHandle(STD_ERROR_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return;
        }
        let mut written: u32 = 0;
        let trampoline = REAL_WRITE_FILE.load(Ordering::Acquire);
        if !trampoline.is_null() {
            let original: WriteFileFn = std::mem::transmute(trampoline);
            let _ = original(
                h,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                core::ptr::null_mut(),
            );
        } else {
            let _ = WriteFile(
                h,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                core::ptr::null_mut(),
            );
        }
    }
}

/// Drain loop: block on the condvar until lines are queued, then write them
/// to stderr. A slow/full stderr blocks only this thread.
fn emit_drain_loop() {
    let queue = emit_queue();
    loop {
        let line = {
            let mut lines = match queue.lines.lock() {
                Ok(lines) => lines,
                Err(_) => return,
            };
            loop {
                if let Some(line) = lines.pop_front() {
                    break line;
                }
                lines = match queue.wakeup.wait(lines) {
                    Ok(lines) => lines,
                    Err(_) => return,
                };
            }
        };
        write_line_to_stderr(&line);
    }
}

/// Start the stderr drain thread exactly once. Called from the install
/// worker (off the loader lock). Free-function `thread::spawn` (a thread,
/// not a process) so the spawn-path guard leaves it alone.
fn start_emit_drain_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::spawn(emit_drain_loop);
    });
}

fn emit_line(line: &str) {
    emit_bytes(line.as_bytes());
}

fn emit_open(path: &str, access: u32, disposition: u32, handle: HANDLE) {
    // `flags` slot reuses the Linux/macOS field name; for Windows we
    // pack `dwDesiredAccess` and `dwCreationDisposition` into a
    // structured value the downstream parser can split on.
    emit_line(&format!(
        "RPP_HOOK file-open path={path:?} access=0x{access:08x} disposition={disposition} handle={handle:p}\n",
    ));
}

fn emit_close(path: &str, handle: HANDLE) {
    emit_line(&format!(
        "RPP_HOOK file-close path={path:?} handle={handle:p}\n"
    ));
}

fn emit_write(path: &str, handle: HANDLE, byte_count: u32) {
    emit_line(&format!(
        "RPP_HOOK file-write path={path:?} handle={handle:p} byte_count={byte_count}\n"
    ));
}

fn emit_unlink(path: &str) {
    emit_line(&format!("RPP_HOOK file-unlink path={path:?}\n"));
}

fn emit_rename(from: &str, to: &str) {
    emit_line(&format!("RPP_HOOK file-rename from={from:?} to={to:?}\n"));
}

// ── Path helpers ──

/// Read a NUL-terminated UTF-16 string into a Rust `String`. Returns
/// `None` for a null pointer or invalid UTF-16. Bounded at 32k
/// `wchar_t`s to avoid runaway scans of unterminated buffers (the
/// Win32 `MAX_PATH`-extended form caps at ~32760 wide chars).
fn wide_cstr_to_string(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while len < 32_768 {
        let c = unsafe { *ptr.add(len) };
        if c == 0 {
            break;
        }
        len += 1;
    }
    if len == 32_768 {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    OsString::from_wide(slice).into_string().ok()
}

// ── Detour bodies ──

/// Detour body for `CreateFileW`. Calls the original via the
/// stashed trampoline, emits an `RPP_HOOK file-open` line on
/// success, registers the handle in the HANDLE→path table, and
/// returns the original handle. Failures (`INVALID_HANDLE_VALUE`)
/// pass through without emitting or registering.
unsafe extern "system" fn create_file_w_detour(
    lp_file_name: *const u16,
    dw_desired_access: u32,
    dw_share_mode: u32,
    lp_security_attributes: *const core::ffi::c_void,
    dw_creation_disposition: u32,
    dw_flags_and_attributes: u32,
    h_template_file: HANDLE,
) -> HANDLE {
    let trampoline_ptr = REAL_CREATE_FILE_W.load(Ordering::Acquire);
    if trampoline_ptr.is_null() {
        return INVALID_HANDLE_VALUE;
    }
    let original: CreateFileWFn = std::mem::transmute(trampoline_ptr);

    let handle = original(
        lp_file_name,
        dw_desired_access,
        dw_share_mode,
        lp_security_attributes,
        dw_creation_disposition,
        dw_flags_and_attributes,
        h_template_file,
    );

    if IN_HOOK.with(|c| c.get()) {
        return handle;
    }
    IN_HOOK.with(|c| c.set(true));
    if handle != INVALID_HANDLE_VALUE {
        if let Some(path) = wide_cstr_to_string(lp_file_name) {
            emit_open(&path, dw_desired_access, dw_creation_disposition, handle);
            handle_table_insert(handle, path);
        }
    }
    IN_HOOK.with(|c| c.set(false));

    handle
}

/// Detour body for `WriteFile`. Calls the original, emits a
/// `file-write` line if the handle is in our table (so we only
/// emit for file writes, not socket / pipe / etc.). Always passes
/// through to the original first so I/O semantics are preserved.
unsafe extern "system" fn write_file_detour(
    h_file: HANDLE,
    lp_buffer: *const u8,
    n_number_of_bytes_to_write: u32,
    lp_number_of_bytes_written: *mut u32,
    lp_overlapped: *mut core::ffi::c_void,
) -> BOOL {
    let trampoline_ptr = REAL_WRITE_FILE.load(Ordering::Acquire);
    if trampoline_ptr.is_null() {
        return FALSE;
    }
    let original: WriteFileFn = std::mem::transmute(trampoline_ptr);

    let ok = original(
        h_file,
        lp_buffer,
        n_number_of_bytes_to_write,
        lp_number_of_bytes_written,
        lp_overlapped,
    );

    if IN_HOOK.with(|c| c.get()) {
        return ok;
    }
    IN_HOOK.with(|c| c.set(true));
    if ok != FALSE {
        if let Some(path) = handle_table_get(h_file) {
            // `lpNumberOfBytesWritten` may be null for OVERLAPPED I/O;
            // fall back to the requested count in that case.
            let written = if lp_number_of_bytes_written.is_null() {
                n_number_of_bytes_to_write
            } else {
                *lp_number_of_bytes_written
            };
            emit_write(&path, h_file, written);
        }
    }
    IN_HOOK.with(|c| c.set(false));

    ok
}

/// Detour body for `CloseHandle`. Polymorphic — closes any HANDLE
/// type. We only emit `file-close` for handles that appear in our
/// table; non-file closes pass through silently.
unsafe extern "system" fn close_handle_detour(h_object: HANDLE) -> BOOL {
    let trampoline_ptr = REAL_CLOSE_HANDLE.load(Ordering::Acquire);
    if trampoline_ptr.is_null() {
        return FALSE;
    }
    let original: CloseHandleFn = std::mem::transmute(trampoline_ptr);

    if IN_HOOK.with(|c| c.get()) {
        return original(h_object);
    }
    IN_HOOK.with(|c| c.set(true));
    // Look up the path before the close so we can emit a final
    // event even if the table cleanup happens after the close.
    let path_before = handle_table_get(h_object);
    let ok = original(h_object);
    if ok != FALSE {
        if let Some(path) = path_before {
            emit_close(&path, h_object);
            let _ = handle_table_remove(h_object);
        }
    }
    IN_HOOK.with(|c| c.set(false));

    ok
}

/// Detour body for `DeleteFileW`. Emits `file-unlink` on success.
unsafe extern "system" fn delete_file_w_detour(lp_file_name: *const u16) -> BOOL {
    let trampoline_ptr = REAL_DELETE_FILE_W.load(Ordering::Acquire);
    if trampoline_ptr.is_null() {
        return FALSE;
    }
    let original: DeleteFileWFn = std::mem::transmute(trampoline_ptr);

    let ok = original(lp_file_name);

    if IN_HOOK.with(|c| c.get()) {
        return ok;
    }
    IN_HOOK.with(|c| c.set(true));
    if ok != FALSE {
        if let Some(path) = wide_cstr_to_string(lp_file_name) {
            emit_unlink(&path);
        }
    }
    IN_HOOK.with(|c| c.set(false));

    ok
}

/// Detour body for `MoveFileExW` — the underlying rename / move
/// primitive (`MoveFileW` delegates to it on modern Windows). Emits
/// `file-rename` on success.
unsafe extern "system" fn move_file_ex_w_detour(
    lp_existing_file_name: *const u16,
    lp_new_file_name: *const u16,
    dw_flags: u32,
) -> BOOL {
    let trampoline_ptr = REAL_MOVE_FILE_EX_W.load(Ordering::Acquire);
    if trampoline_ptr.is_null() {
        return FALSE;
    }
    let original: MoveFileExWFn = std::mem::transmute(trampoline_ptr);

    let ok = original(lp_existing_file_name, lp_new_file_name, dw_flags);

    if IN_HOOK.with(|c| c.get()) {
        return ok;
    }
    IN_HOOK.with(|c| c.set(true));
    if ok != FALSE {
        if let (Some(from), Some(to)) = (
            wide_cstr_to_string(lp_existing_file_name),
            wide_cstr_to_string(lp_new_file_name),
        ) {
            emit_rename(&from, &to);
        }
    }
    IN_HOOK.with(|c| c.set(false));

    ok
}

// ── Install machinery ──

/// Look up `name` in `module` and install a detour pointing at
/// `hook`. On success, stashes the trampoline pointer in `slot`.
/// Errors are swallowed: a hook-install failure should not prevent
/// the host process from starting.
unsafe fn install_one(
    module: *mut core::ffi::c_void,
    name: &core::ffi::CStr,
    hook: *const (),
    slot: &AtomicPtr<()>,
) {
    let Some(target) = GetProcAddress(module, name.as_ptr() as *const u8) else {
        return;
    };
    let Ok(detour) = RawDetour::new(target as *const (), hook) else {
        return;
    };
    // Publish the trampoline BEFORE enabling the detour (issue #590,
    // cluster L). `enable()` patches the target to jump to `hook`; if it
    // ran before the `slot.store` below, a concurrent call in the
    // enable→store window would enter the hook, read a null trampoline,
    // and drop the caller's real I/O. The trampoline is valid as soon as
    // `RawDetour::new` returns (it is the saved original prologue + jump),
    // and it is only ever dereferenced from inside a live hook, so
    // publishing it while the detour is not yet enabled is safe.
    // `&() -> *const () -> *mut ()` — see slice 6b commit message for the
    // rationale on stripping the borrow.
    slot.store(
        detour.trampoline() as *const () as *mut (),
        Ordering::Release,
    );
    if detour.enable().is_err() {
        // Enable failed: the RawDetour is about to drop (freeing the
        // trampoline), so unpublish first — otherwise another already-live
        // hook could dereference a dangling trampoline pointer.
        slot.store(core::ptr::null_mut(), Ordering::Release);
        return;
    }
    // Leak the RawDetour so its Drop doesn't disable the hook when
    // this scope exits. The detour lives for the rest of the
    // process.
    core::mem::forget(detour);
}

/// Install all wired-up detours. Called from `DllMain` under
/// `DLL_PROCESS_ATTACH`.
unsafe fn install_detours() {
    // Wide-encode "kernel32.dll\0" for GetModuleHandleW. Encoded
    // inline rather than via a UTF-16 literal to keep
    // `windows-sys` the only Win32 dep.
    let kernel32: [u16; 13] = [
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
    let module = GetModuleHandleW(kernel32.as_ptr());
    if module.is_null() {
        return;
    }

    // Slice 7c diagnostic: emit a sentinel line *before* and *after*
    // each install_one call. If only one of the pair shows up in
    // stderr, we know exactly which install hung or panicked. Goal
    // is to narrow down which retour call (CreateFileW? WriteFile?
    // CloseHandle?) is misbehaving inside cmd.exe / our testbin
    // probe.
    emit_line("RPP_HOOK install begin=CreateFileW\n");
    install_one(
        module,
        c"CreateFileW",
        create_file_w_detour as *const (),
        &REAL_CREATE_FILE_W,
    );
    emit_line("RPP_HOOK install end=CreateFileW\n");

    emit_line("RPP_HOOK install begin=WriteFile\n");
    install_one(
        module,
        c"WriteFile",
        write_file_detour as *const (),
        &REAL_WRITE_FILE,
    );
    emit_line("RPP_HOOK install end=WriteFile\n");

    emit_line("RPP_HOOK install begin=CloseHandle\n");
    install_one(
        module,
        c"CloseHandle",
        close_handle_detour as *const (),
        &REAL_CLOSE_HANDLE,
    );
    emit_line("RPP_HOOK install end=CloseHandle\n");

    emit_line("RPP_HOOK install begin=DeleteFileW\n");
    install_one(
        module,
        c"DeleteFileW",
        delete_file_w_detour as *const (),
        &REAL_DELETE_FILE_W,
    );
    emit_line("RPP_HOOK install end=DeleteFileW\n");

    emit_line("RPP_HOOK install begin=MoveFileExW\n");
    install_one(
        module,
        c"MoveFileExW",
        move_file_ex_w_detour as *const (),
        &REAL_MOVE_FILE_EX_W,
    );
    emit_line("RPP_HOOK install end=MoveFileExW\n");
}

/// `CreateThread`-compatible worker entrypoint that drives
/// `install_detours()` off the loader lock. Returns 0 on completion
/// (the return code is captured by `GetExitCodeThread` but we
/// don't read it).
unsafe extern "system" fn install_thread_main(_param: *mut core::ffi::c_void) -> u32 {
    // Start the stderr drain thread before emitting anything (issue #590,
    // cluster E): from here on `emit_bytes` only enqueues, and this thread
    // does the blocking writes so a full stderr can't wedge a host file op.
    start_emit_drain_thread();
    // Diagnostic line so the slice 7 integration test can confirm
    // the worker thread actually ran.
    emit_line("RPP_HOOK install-thread-start\n");
    install_detours();
    emit_line("RPP_HOOK install-thread-done\n");
    0
}

/// DLL entry point. Windows calls this when the DLL is loaded into a
/// process (via `LoadLibrary` from the sidecar injector) and when
/// it's unloaded.
///
/// # Safety
///
/// Called by the Windows loader with the documented `DllMain`
/// signature. **Critical**: DllMain runs under the Windows loader
/// lock. retour-rs's `RawDetour::new` disassembles the target's
/// prologue (iced-x86) and calls `VirtualProtect`; both can
/// re-enter the loader lock if the target function lives in a
/// module that's still being initialized. Empirically this hangs
/// when injecting into `cmd.exe` (the slice 7a integration test
/// surfaced it).
///
/// Mitigation: spawn a worker thread that does the install + return
/// TRUE immediately. The worker runs *after* DllMain returns, so
/// it's outside the loader lock. The injector side has its own
/// post-inject grace period so detours have time to install before
/// the test exercises them.
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            // Slice 7b: defer detour install to a worker thread.
            // Failing to spawn the worker is a no-op — the host
            // process keeps running, just without our hooks
            // (same failure mode as a retour install error).
            let handle = CreateThread(
                core::ptr::null(),
                0,
                Some(install_thread_main),
                core::ptr::null(),
                0,
                core::ptr::null_mut(),
            );
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                // We don't need to wait on it from DllMain (in
                // fact, doing so would re-introduce the deadlock).
                // The thread runs on its own; close the handle so
                // it doesn't leak.
                CloseHandle(handle);
            }
        }
        DLL_PROCESS_DETACH => {
            // Detours are leaked in install_detours so they stay
            // installed for the life of the process. The OS is
            // tearing the address space down — nothing to do here.
        }
        _ => {
            // DLL_THREAD_ATTACH / DLL_THREAD_DETACH — no-op.
        }
    }
    TRUE
}
