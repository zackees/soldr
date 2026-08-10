//! macOS `DYLD_INSERT_LIBRARIES` interposer for the running-process
//! file-hook tier (#551 slice 5).
//!
//! Built as a cdylib `librunning_process_probe_interposer_macos.dylib`.
//! At load time (when a target process is launched with
//! `DYLD_INSERT_LIBRARIES=…/librunning_process_probe_interposer_macos.dylib`),
//! the dynamic linker loads this library before the C runtime and any
//! symbols we export shadow libc's. A load-time constructor eagerly
//! resolves the real functions via `dlsym(RTLD_NEXT, "…")`; each shadow
//! invokes its resolved function, then emits an
//! `RPP_HOOK …` line on stderr matching the Linux interposer's
//! format (#551 slice 4).
//!
//! ## Differences from the Linux interposer
//!
//! - **Loader env var**: `DYLD_INSERT_LIBRARIES` vs. `LD_PRELOAD`.
//! - **SIP / hardened-runtime**: macOS refuses to inject into binaries
//!   signed with the hardened runtime + library validation flag
//!   unless they also have
//!   `com.apple.security.cs.allow-dyld-environment-variables`. System
//!   binaries fall in this bucket. Same boundary as the rest of the
//!   LaunchedProcessTree tier — works against processes the user owns.
//! - **Path-from-fd resolution**: no `/proc/self/fd/<n>` on macOS.
//!   We use `fcntl(fd, F_GETPATH, buf)` instead (`F_GETPATH` is a
//!   macOS-specific extension that writes a path of up to
//!   `MAXPATHLEN` bytes into `buf`).
//! - **AT_FDCWD value**: `-2` on Darwin (vs. Linux's `-100`).
//! - **Variadic `open`/`openat`**: same caveat as Linux — Rust stable
//!   doesn't support `c_variadic`, so the mode argument on
//!   `O_CREAT` opens is ignored. Tests should use existing files.
//!
//! ## Slice 5 scope (this commit covers 5a–5d in one bundle)
//!
//! `open`, `openat`, `close`, `write`, `unlink`, `unlinkat`,
//! `rename`, `renameat` — full port of the Linux file-mutation
//! surface. Same line shapes and behavior as the Linux interposer
//! so the downstream consumer parses a single format.

#![cfg(target_os = "macos")]

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, TryLockError};
use std::time::Duration;

// ── Function-pointer types ──

type OpenFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
type OpenatFn = unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int;
type CloseFn = unsafe extern "C" fn(c_int) -> c_int;
type WriteFn = unsafe extern "C" fn(c_int, *const libc::c_void, libc::size_t) -> libc::ssize_t;
type UnlinkFn = unsafe extern "C" fn(*const c_char) -> c_int;
type UnlinkatFn = unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int;
type RenameFn = unsafe extern "C" fn(*const c_char, *const c_char) -> c_int;
type RenameatFn = unsafe extern "C" fn(c_int, *const c_char, c_int, *const c_char) -> c_int;

// ── Eagerly resolved function pointers ──

static REAL_OPEN: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
static REAL_OPENAT: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
static REAL_CLOSE: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
static REAL_WRITE: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
static REAL_UNLINK: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
static REAL_UNLINKAT: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
static REAL_RENAME: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
static REAL_RENAMEAT: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
static POST_FORK_CHILD: AtomicBool = AtomicBool::new(false);

/// Process-global fd→path map. Same purpose as the Linux interposer's
/// table: populated on successful open/openat, queried on
/// close/write, removed on close. See module-level docs.
static FD_TABLE: OnceLock<Mutex<HashMap<c_int, String>>> = OnceLock::new();

fn fd_table() -> &'static Mutex<HashMap<c_int, String>> {
    FD_TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(feature = "test-seams")]
unsafe fn test_signal_and_wait(ready_fd: c_int, release_fd: c_int) {
    // Darwin's libc crate does not expose SYS_read/SYS_write. These stable
    // BSD syscall numbers keep the seam from re-entering our write hook while
    // it deliberately holds FD_TABLE.
    const SYS_READ: libc::c_int = 3;
    const SYS_WRITE: libc::c_int = 4;
    let byte = [1u8; 1];
    libc::syscall(SYS_WRITE, ready_fd, byte.as_ptr(), byte.len());
    let mut release = [0u8; 1];
    libc::syscall(SYS_READ, release_fd, release.as_mut_ptr(), release.len());
}

/// Test seam: hold the real fd-table lock across a fork.
#[doc(hidden)]
#[cfg(feature = "test-seams")]
#[no_mangle]
pub unsafe extern "C" fn rpo_test_hold_fd_table(ready_fd: c_int, release_fd: c_int) {
    let _guard = fd_table().lock().expect("fd table lock");
    test_signal_and_wait(ready_fd, release_fd);
}

thread_local! {
    /// Reentrancy guard — falls through to the real fn without
    /// emitting if called from within our own shadow.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

// ── dlsym helpers ──

macro_rules! resolve_real {
    ($lock:ident, $name:literal, $fn_ty:ty) => {{
        let raw = $lock.load(Ordering::Acquire);
        if raw.is_null() {
            unsafe { libc::abort() }
        }
        unsafe { std::mem::transmute::<*mut libc::c_void, $fn_ty>(raw) }
    }};
}

fn real_open() -> OpenFn {
    resolve_real!(REAL_OPEN, "open", OpenFn)
}
fn real_openat() -> OpenatFn {
    resolve_real!(REAL_OPENAT, "openat", OpenatFn)
}
fn real_close() -> CloseFn {
    resolve_real!(REAL_CLOSE, "close", CloseFn)
}
fn real_write() -> WriteFn {
    resolve_real!(REAL_WRITE, "write", WriteFn)
}
fn real_unlink() -> UnlinkFn {
    resolve_real!(REAL_UNLINK, "unlink", UnlinkFn)
}
fn real_unlinkat() -> UnlinkatFn {
    resolve_real!(REAL_UNLINKAT, "unlinkat", UnlinkatFn)
}
fn real_rename() -> RenameFn {
    resolve_real!(REAL_RENAME, "rename", RenameFn)
}
fn real_renameat() -> RenameatFn {
    resolve_real!(REAL_RENAMEAT, "renameat", RenameatFn)
}

extern "C" fn post_fork_child() {
    POST_FORK_CHILD.store(true, Ordering::Release);
}

extern "C" fn interposer_init() {
    unsafe fn resolve(name: &CStr) -> *mut libc::c_void {
        let raw = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
        if raw.is_null() {
            libc::abort();
        }
        raw
    }
    unsafe {
        REAL_OPEN.store(resolve(c"open"), Ordering::Release);
        REAL_OPENAT.store(resolve(c"openat"), Ordering::Release);
        REAL_CLOSE.store(resolve(c"close"), Ordering::Release);
        REAL_WRITE.store(resolve(c"write"), Ordering::Release);
        REAL_UNLINK.store(resolve(c"unlink"), Ordering::Release);
        REAL_UNLINKAT.store(resolve(c"unlinkat"), Ordering::Release);
        REAL_RENAME.store(resolve(c"rename"), Ordering::Release);
        REAL_RENAMEAT.store(resolve(c"renameat"), Ordering::Release);
    }
    let _ = fd_table();
    let _ = emit_queue();
    unsafe {
        if libc::pthread_atfork(None, None, Some(post_fork_child)) != 0 {
            libc::abort();
        }
    }
}

#[used]
#[link_section = "__DATA,__mod_init_func"]
static INTERPOSER_INIT: extern "C" fn() = interposer_init;

// ── Event emission ──

struct EmitQueue {
    lines: Mutex<VecDeque<Vec<u8>>>,
    wakeup: Condvar,
}

static EMIT_QUEUE: OnceLock<EmitQueue> = OnceLock::new();
static EMIT_PENDING: AtomicUsize = AtomicUsize::new(0);

/// Cap memory use when stderr is slow or permanently undrained.
const EMIT_QUEUE_MAX_LINES: usize = 4096;

fn emit_queue() -> &'static EmitQueue {
    EMIT_QUEUE.get_or_init(|| EmitQueue {
        lines: Mutex::new(VecDeque::new()),
        wakeup: Condvar::new(),
    })
}

/// Put an event on the bounded drain queue without ever waiting.
///
/// Contention, poisoning, and a full queue all degrade to event loss: hook
/// telemetry must never delay the host file operation that produced it.
fn emit_bytes(bytes: &[u8]) {
    let queue = emit_queue();
    let mut lines = match queue.lines.try_lock() {
        Ok(lines) => lines,
        Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => return,
    };
    if lines.len() >= EMIT_QUEUE_MAX_LINES {
        lines.pop_front();
        EMIT_PENDING.fetch_sub(1, Ordering::Release);
    }
    lines.push_back(bytes.to_vec());
    EMIT_PENDING.fetch_add(1, Ordering::Release);
    queue.wakeup.notify_one();
}

/// Blocking writes are isolated to the dedicated drain thread.
fn write_line_to_stderr(bytes: &[u8]) {
    let real = real_write();
    unsafe {
        let _ = real(
            libc::STDERR_FILENO,
            bytes.as_ptr() as *const libc::c_void,
            bytes.len(),
        );
    }
}

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
        EMIT_PENDING.fetch_sub(1, Ordering::Release);
    }
}

/// Give short-lived processes a bounded opportunity to emit their final line.
///
/// A full stderr can keep the drain blocked, so this must remain a small,
/// fixed budget. Process exit proceeds after the budget regardless.
extern "C" fn flush_pending_at_exit() {
    for _ in 0..25 {
        if EMIT_PENDING.load(Ordering::Acquire) == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn start_emit_drain_thread() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        // Resolve on the hook thread while its reentrancy guard is active.
        // Resolving from the new thread could itself call an interposed file
        // API and recursively wait on this `OnceLock` initialization.
        let _ = real_write();
        // SAFETY: the callback has C ABI, takes no arguments, and remains
        // valid for the lifetime of the loaded interposer.
        unsafe {
            libc::atexit(flush_pending_at_exit);
        }
        let _ = std::thread::spawn(emit_drain_loop);
    });
}

fn emit_line(line: &str) {
    start_emit_drain_thread();
    emit_bytes(line.as_bytes());
}

fn emit_open(path: &str, flags: c_int, fd: c_int) {
    emit_line(&format!(
        "RPP_HOOK file-open path={path:?} flags={flags} fd={fd}\n"
    ));
}

fn emit_close(path: &str, fd: c_int) {
    emit_line(&format!("RPP_HOOK file-close path={path:?} fd={fd}\n"));
}

fn emit_write(path: &str, fd: c_int, byte_count: i64) {
    emit_line(&format!(
        "RPP_HOOK file-write path={path:?} fd={fd} byte_count={byte_count}\n"
    ));
}

fn emit_unlink(path: &str) {
    emit_line(&format!("RPP_HOOK file-unlink path={path:?}\n"));
}

fn emit_rename(from: &str, to: &str) {
    emit_line(&format!("RPP_HOOK file-rename from={from:?} to={to:?}\n"));
}

// ── fd-table helpers ──

fn fd_table_insert(fd: c_int, path: String) {
    if let Ok(mut tbl) = fd_table().lock() {
        tbl.insert(fd, path);
    }
}

fn fd_table_get(fd: c_int) -> Option<String> {
    fd_table().lock().ok()?.get(&fd).cloned()
}

fn fd_table_remove(fd: c_int) -> Option<String> {
    fd_table().lock().ok()?.remove(&fd)
}

// ── Path resolution ──

/// `AT_FDCWD` on Darwin is -2 (vs. Linux's -100). Worth a constant
/// rather than a magic number in [`resolve_at`].
const DARWIN_AT_FDCWD: c_int = -2;

/// `F_GETPATH` (macOS-specific fcntl command) writes the path of an
/// open file descriptor into the supplied buffer. The buffer must
/// be at least `MAXPATHLEN` (1024) bytes — `PATH_MAX` on Darwin.
const F_GETPATH: c_int = 50;

/// Resolve a `(dirfd, pathname)` pair to a single absolute path.
/// Same shape as the Linux interposer's helper but uses
/// `fcntl(dirfd, F_GETPATH, buf)` instead of `/proc/self/fd/<n>`.
fn resolve_at(dirfd: c_int, pathname: &CStr) -> Option<String> {
    let path_str = pathname.to_str().ok()?;
    if path_str.starts_with('/') {
        return Some(path_str.to_string());
    }
    if dirfd == DARWIN_AT_FDCWD || dirfd < 0 {
        return Some(path_str.to_string());
    }
    let mut buf = [0u8; libc::PATH_MAX as usize];
    let r = unsafe { libc::fcntl(dirfd, F_GETPATH, buf.as_mut_ptr() as *mut c_char) };
    if r < 0 {
        return None;
    }
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let dir = std::str::from_utf8(&buf[..nul]).ok()?;
    Some(format!("{dir}/{path_str}"))
}

/// Resolve a path from a known-open fd via `fcntl(fd, F_GETPATH, buf)`.
/// Used as the fallback path when the fd table doesn't have an
/// entry — e.g. an fd opened before our interposer loaded.
fn fd_to_path(fd: c_int) -> Option<String> {
    let mut buf = [0u8; libc::PATH_MAX as usize];
    let r = unsafe { libc::fcntl(fd, F_GETPATH, buf.as_mut_ptr() as *mut c_char) };
    if r < 0 {
        return None;
    }
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..nul]).ok().map(str::to_string)
}

// ── Shadows ──

/// DYLD_INSERT_LIBRARIES shadow for `open(2)`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. Arguments match POSIX `open(2)`. See the
/// module header for the variadic-mode caveat.
#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int) -> c_int {
    let real = real_open();
    if POST_FORK_CHILD.load(Ordering::Acquire) {
        return real(path, flags);
    }
    if IN_HOOK.with(|c| c.get()) {
        return real(path, flags);
    }
    IN_HOOK.with(|c| c.set(true));
    let fd = real(path, flags);
    if fd >= 0 && !path.is_null() {
        if let Ok(p) = CStr::from_ptr(path).to_str() {
            emit_open(p, flags, fd);
            fd_table_insert(fd, p.to_string());
        }
    }
    IN_HOOK.with(|c| c.set(false));
    fd
}

/// DYLD_INSERT_LIBRARIES shadow for `openat(2)`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. Arguments match POSIX `openat(2)`.
#[no_mangle]
pub unsafe extern "C" fn openat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    let real = real_openat();
    if POST_FORK_CHILD.load(Ordering::Acquire) {
        return real(dirfd, path, flags);
    }
    if IN_HOOK.with(|c| c.get()) {
        return real(dirfd, path, flags);
    }
    IN_HOOK.with(|c| c.set(true));
    let fd = real(dirfd, path, flags);
    if fd >= 0 && !path.is_null() {
        if let Some(resolved) = resolve_at(dirfd, CStr::from_ptr(path)) {
            emit_open(&resolved, flags, fd);
            fd_table_insert(fd, resolved);
        }
    }
    IN_HOOK.with(|c| c.set(false));
    fd
}

/// DYLD_INSERT_LIBRARIES shadow for `close(2)`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. Arguments match POSIX `close(2)`.
#[no_mangle]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    let real = real_close();
    if POST_FORK_CHILD.load(Ordering::Acquire) {
        return real(fd);
    }
    if IN_HOOK.with(|c| c.get()) {
        return real(fd);
    }
    IN_HOOK.with(|c| c.set(true));
    // Use fd_table first; fall back to fcntl(F_GETPATH) for fds
    // opened before our interposer loaded.
    if let Some(path) = fd_table_get(fd).or_else(|| fd_to_path(fd)) {
        emit_close(&path, fd);
    }
    let r = real(fd);
    if r == 0 {
        let _ = fd_table_remove(fd);
    }
    IN_HOOK.with(|c| c.set(false));
    r
}

/// DYLD_INSERT_LIBRARIES shadow for `write(2)`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. Arguments match POSIX `write(2)`. The
/// buf+count region must be valid for `count` bytes of read; we
/// don't dereference it ourselves.
#[no_mangle]
pub unsafe extern "C" fn write(
    fd: c_int,
    buf: *const libc::c_void,
    count: libc::size_t,
) -> libc::ssize_t {
    let real = real_write();
    if POST_FORK_CHILD.load(Ordering::Acquire) {
        return real(fd, buf, count);
    }
    if IN_HOOK.with(|c| c.get()) {
        return real(fd, buf, count);
    }
    IN_HOOK.with(|c| c.set(true));
    let n = real(fd, buf, count);
    if n > 0 {
        if let Some(path) = fd_table_get(fd).or_else(|| fd_to_path(fd)) {
            emit_write(&path, fd, n as i64);
        }
    }
    IN_HOOK.with(|c| c.set(false));
    n
}

/// DYLD_INSERT_LIBRARIES shadow for `unlink(2)`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. Arguments match POSIX `unlink(2)`.
#[no_mangle]
pub unsafe extern "C" fn unlink(path: *const c_char) -> c_int {
    let real = real_unlink();
    if POST_FORK_CHILD.load(Ordering::Acquire) {
        return real(path);
    }
    if IN_HOOK.with(|c| c.get()) {
        return real(path);
    }
    IN_HOOK.with(|c| c.set(true));
    let r = real(path);
    if r == 0 && !path.is_null() {
        if let Ok(p) = CStr::from_ptr(path).to_str() {
            emit_unlink(p);
        }
    }
    IN_HOOK.with(|c| c.set(false));
    r
}

/// DYLD_INSERT_LIBRARIES shadow for `unlinkat(2)`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. Arguments match POSIX `unlinkat(2)`.
#[no_mangle]
pub unsafe extern "C" fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    let real = real_unlinkat();
    if POST_FORK_CHILD.load(Ordering::Acquire) {
        return real(dirfd, path, flags);
    }
    if IN_HOOK.with(|c| c.get()) {
        return real(dirfd, path, flags);
    }
    IN_HOOK.with(|c| c.set(true));
    let r = real(dirfd, path, flags);
    if r == 0 && !path.is_null() {
        if let Some(resolved) = resolve_at(dirfd, CStr::from_ptr(path)) {
            emit_unlink(&resolved);
        }
    }
    IN_HOOK.with(|c| c.set(false));
    r
}

/// DYLD_INSERT_LIBRARIES shadow for `rename(2)`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. Arguments match POSIX `rename(2)`.
#[no_mangle]
pub unsafe extern "C" fn rename(old: *const c_char, new: *const c_char) -> c_int {
    let real = real_rename();
    if POST_FORK_CHILD.load(Ordering::Acquire) {
        return real(old, new);
    }
    if IN_HOOK.with(|c| c.get()) {
        return real(old, new);
    }
    IN_HOOK.with(|c| c.set(true));
    let r = real(old, new);
    if r == 0 && !old.is_null() && !new.is_null() {
        if let (Ok(o), Ok(n)) = (CStr::from_ptr(old).to_str(), CStr::from_ptr(new).to_str()) {
            emit_rename(o, n);
        }
    }
    IN_HOOK.with(|c| c.set(false));
    r
}

/// DYLD_INSERT_LIBRARIES shadow for `renameat(2)`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. Arguments match POSIX `renameat(2)`.
#[no_mangle]
pub unsafe extern "C" fn renameat(
    olddirfd: c_int,
    old: *const c_char,
    newdirfd: c_int,
    new: *const c_char,
) -> c_int {
    let real = real_renameat();
    if POST_FORK_CHILD.load(Ordering::Acquire) {
        return real(olddirfd, old, newdirfd, new);
    }
    if IN_HOOK.with(|c| c.get()) {
        return real(olddirfd, old, newdirfd, new);
    }
    IN_HOOK.with(|c| c.set(true));
    let r = real(olddirfd, old, newdirfd, new);
    if r == 0 && !old.is_null() && !new.is_null() {
        if let (Some(o), Some(n)) = (
            resolve_at(olddirfd, CStr::from_ptr(old)),
            resolve_at(newdirfd, CStr::from_ptr(new)),
        ) {
            emit_rename(&o, &n);
        }
    }
    IN_HOOK.with(|c| c.set(false));
    r
}
