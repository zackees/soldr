//! Linux LD_PRELOAD interposer for the running-process file-hook
//! tier (#551 slice 4).
//!
//! Built as a cdylib `librunning_process_probe_interposer_linux.so`.
//! At load time (when a target process is launched with
//! `LD_PRELOAD=…/librunning_process_probe_interposer_linux.so`),
//! this library shadows libc's file-operation symbols. Its load-time
//! constructor eagerly resolves the real functions via `dlsym(RTLD_NEXT, ...)`;
//! each shadow invokes its resolved function, then emits
//! an event line to **stderr** in the form:
//!
//! ```text
//! RPP_HOOK file-open path="<resolved path>" flags=<int> fd=<int>
//! ```
//!
//! Stderr is the slice-4a transport. Slice 4b will replace this with
//! a length-prefixed message over a named pipe whose path is supplied
//! via the `RP_OBSERVER_EVENT_PIPE` env var (set by the parent before
//! `execve()`).
//!
//! ## Slice 4 scope (4a-4d)
//!
//! Slice 4a: `open(2)`. Slice 4b: `openat(2)` with dirfd resolution.
//! Slice 4c: `close(2)` and `write(2)` plus the process-global
//! fd→path table that lets them resolve which file the syscall is
//! touching. Slice 4d (this commit): `unlink(2)`, `unlinkat(2)`,
//! `rename(2)`, `renameat(2)` — reuse the dirfd-join pattern from
//! slice 4b but emit `file-unlink` / `file-rename` events instead of
//! `file-open`.
//!
//! After slice 4d the Linux interposer covers the standard file
//! mutation surface. `pwrite`/`writev`/`pwritev`/`sendfile`/`splice`
//! are still gaps; they land alongside the macOS interposer (slice 5
//! of #551). `creat`, `open64`, `__open_2` (libc internal variants)
//! are intentionally NOT shadowed yet — they follow once we have a
//! test fixture that exercises one variant.
//!
//! ## Caveats
//!
//! - **Variadic `open`** — POSIX `open` is variadic when `O_CREAT` is
//!   in flags (mode argument). Rust stable doesn't support
//!   `c_variadic`. We declare our shadow as non-variadic
//!   `extern "C" fn(*const c_char, c_int) -> c_int`; the C calling
//!   convention happily ignores the unused `mode` argument on x86_64
//!   System V ABI (passed in `edx`/`xmm2` — caller cleans up). The
//!   resulting mode is forwarded to the real `open` as 0 unless we
//!   also re-read it, which is hard without `c_variadic`. **Slice 4a
//!   limitation**: `O_CREAT` opens through the shadow get mode=0,
//!   which means new files end up with mode 000 unless the caller's
//!   umask is set otherwise. Slice 4b uses `syscall(SYS_open, ...)`
//!   directly to dodge this entirely. Tests in slice 4a use existing
//!   files (no `O_CREAT`).
//! - **Reentrancy** — observer work may itself invoke an interposed
//!   operation. A `thread_local!` reentrancy flag makes re-entry fall
//!   through to the real function without emitting.
//! - **Async-signal safety** — `eprintln!` is not async-signal-safe.
//!   The shadow can be called from signal handlers in theory; in
//!   practice POSIX warns against this. Slice 4b uses `write(2)`
//!   directly to a fixed fd.

// A statically linked musl process has no dynamic loader namespace for
// LD_PRELOAD or RTLD_NEXT. The Cargo target still builds this crate as an
// inert rlib so workspace-wide musl builds remain supported.
#![cfg(all(target_os = "linux", not(target_env = "musl")))]

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock, TryLockError};
use std::time::Duration;

/// Type of libc `open(2)`. Declared non-variadic — see the module
/// doc's caveats section.
type OpenFn = unsafe extern "C" fn(*const c_char, c_int) -> c_int;

/// Type of libc `openat(2)`. Non-variadic for the same reason as
/// [`OpenFn`].
type OpenatFn = unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int;

/// Type of libc `close(2)`.
type CloseFn = unsafe extern "C" fn(c_int) -> c_int;

/// Type of libc `write(2)`. Return is `ssize_t` (signed pointer-sized);
/// libc::ssize_t is the portable name.
type WriteFn = unsafe extern "C" fn(c_int, *const libc::c_void, libc::size_t) -> libc::ssize_t;

/// Type of libc `unlink(2)`.
type UnlinkFn = unsafe extern "C" fn(*const c_char) -> c_int;

/// Type of libc `unlinkat(2)`.
type UnlinkatFn = unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int;

/// Type of libc `rename(2)`.
type RenameFn = unsafe extern "C" fn(*const c_char, *const c_char) -> c_int;

/// Type of libc `renameat(2)`.
type RenameatFn = unsafe extern "C" fn(c_int, *const c_char, c_int, *const c_char) -> c_int;

/// Eagerly resolved pointer to the real libc `open`.
static REAL_OPEN_PTR: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Cache of the real libc `openat`.
static REAL_OPENAT_PTR: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Cache of the real libc `close`.
static REAL_CLOSE_PTR: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Cache of the real libc `write`.
static REAL_WRITE_PTR: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Cache of the real libc `unlink`.
static REAL_UNLINK_PTR: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Cache of the real libc `unlinkat`.
static REAL_UNLINKAT_PTR: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Cache of the real libc `rename`.
static REAL_RENAME_PTR: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Cache of the real libc `renameat`.
static REAL_RENAMEAT_PTR: AtomicPtr<libc::c_void> = AtomicPtr::new(std::ptr::null_mut());
#[cfg(feature = "test-seams")]
static TEST_RESOLVER_INIT: OnceLock<()> = OnceLock::new();
static POST_FORK_CHILD: AtomicBool = AtomicBool::new(false);

/// Process-global fd→path map. Populated on each successful
/// `open`/`openat`, queried on `close`/`write` so the emitted event
/// carries a meaningful path. Removed on `close`. Heavy contention
/// is unlikely in practice — file I/O is much slower than a brief
/// mutex acquisition — but if it becomes a problem we can swap for
/// a sharded `RwLock<HashMap<...>>` per fd-modulo-N bucket.
///
/// **Not shared across processes**: each LD_PRELOAD'd target has its
/// own copy. `execve()` clears the static because the new process
/// image starts fresh (this is the desired behavior — fds across
/// exec are explicit via CLOEXEC handling, which the kernel does
/// for us).
static FD_TABLE: OnceLock<Mutex<HashMap<c_int, String>>> = OnceLock::new();

fn fd_table() -> &'static Mutex<HashMap<c_int, String>> {
    FD_TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(feature = "test-seams")]
unsafe fn test_signal_and_wait(ready_fd: c_int, release_fd: c_int) {
    let byte = [1u8; 1];
    libc::syscall(libc::SYS_write, ready_fd, byte.as_ptr(), byte.len());
    let mut release = [0u8; 1];
    libc::syscall(
        libc::SYS_read,
        release_fd,
        release.as_mut_ptr(),
        release.len(),
    );
}

/// Test seam: hold the real fd-table lock across a fork.
#[doc(hidden)]
#[cfg(feature = "test-seams")]
#[no_mangle]
pub unsafe extern "C" fn rpo_test_hold_fd_table(ready_fd: c_int, release_fd: c_int) {
    let _guard = fd_table().lock().expect("fd table lock");
    test_signal_and_wait(ready_fd, release_fd);
}

/// Test seam: hold the real renameat resolver OnceLock initializer.
#[doc(hidden)]
#[cfg(feature = "test-seams")]
#[no_mangle]
pub unsafe extern "C" fn rpo_test_hold_renameat_resolver_init(ready_fd: c_int, release_fd: c_int) {
    let _ = TEST_RESOLVER_INIT.get_or_init(|| {
        test_signal_and_wait(ready_fd, release_fd);
    });
}

thread_local! {
    /// Reentrancy guard. If we somehow re-enter `open` from within
    /// our own shadow (e.g. an event-emit path that does I/O the
    /// kernel routes back through libc), we want to fall through to
    /// the real `open` without firing another event. Without this a
    /// stack overflow is the failure mode.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

fn real_open() -> OpenFn {
    let raw = REAL_OPEN_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        unsafe { libc::abort() }
    }
    unsafe { std::mem::transmute::<*mut libc::c_void, OpenFn>(raw) }
}

fn real_openat() -> OpenatFn {
    let raw = REAL_OPENAT_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        unsafe { libc::abort() }
    }
    unsafe { std::mem::transmute::<*mut libc::c_void, OpenatFn>(raw) }
}

fn real_close() -> CloseFn {
    let raw = REAL_CLOSE_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        unsafe { libc::abort() }
    }
    unsafe { std::mem::transmute::<*mut libc::c_void, CloseFn>(raw) }
}

fn real_write() -> WriteFn {
    let raw = REAL_WRITE_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        unsafe { libc::abort() }
    }
    unsafe { std::mem::transmute::<*mut libc::c_void, WriteFn>(raw) }
}

fn real_unlink() -> UnlinkFn {
    let raw = REAL_UNLINK_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        unsafe { libc::abort() }
    }
    unsafe { std::mem::transmute::<*mut libc::c_void, UnlinkFn>(raw) }
}

fn real_unlinkat() -> UnlinkatFn {
    let raw = REAL_UNLINKAT_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        unsafe { libc::abort() }
    }
    unsafe { std::mem::transmute::<*mut libc::c_void, UnlinkatFn>(raw) }
}

fn real_rename() -> RenameFn {
    let raw = REAL_RENAME_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        unsafe { libc::abort() }
    }
    unsafe { std::mem::transmute::<*mut libc::c_void, RenameFn>(raw) }
}

fn real_renameat() -> RenameatFn {
    let raw = REAL_RENAMEAT_PTR.load(Ordering::Acquire);
    if raw.is_null() {
        unsafe { libc::abort() }
    }
    unsafe { std::mem::transmute::<*mut libc::c_void, RenameatFn>(raw) }
}

extern "C" fn post_fork_child() {
    POST_FORK_CHILD.store(true, Ordering::Release);
}

extern "C" fn interposer_init() {
    // Resolve every RTLD_NEXT symbol before application threads or forks can
    // exist. Hook hot paths only observe published atomic pointers afterward.
    unsafe fn resolve(name: &CStr) -> *mut libc::c_void {
        let raw = libc::dlsym(libc::RTLD_NEXT, name.as_ptr());
        if raw.is_null() {
            libc::abort();
        }
        raw
    }
    unsafe {
        REAL_OPEN_PTR.store(resolve(c"open"), Ordering::Release);
        REAL_OPENAT_PTR.store(resolve(c"openat"), Ordering::Release);
        REAL_CLOSE_PTR.store(resolve(c"close"), Ordering::Release);
        REAL_WRITE_PTR.store(resolve(c"write"), Ordering::Release);
        REAL_UNLINK_PTR.store(resolve(c"unlink"), Ordering::Release);
        REAL_UNLINKAT_PTR.store(resolve(c"unlinkat"), Ordering::Release);
        REAL_RENAME_PTR.store(resolve(c"rename"), Ordering::Release);
        REAL_RENAMEAT_PTR.store(resolve(c"renameat"), Ordering::Release);
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
#[link_section = ".init_array"]
static INTERPOSER_INIT: extern "C" fn() = interposer_init;

/// Resolve a `(dirfd, pathname)` pair to a single best-effort POSIX
/// path. Cases handled:
///
/// - `pathname` is absolute → return it as-is.
/// - `dirfd == AT_FDCWD` (-100) → return `pathname` unchanged; the
///   target process's cwd is the kernel's resolution context anyway.
/// - `dirfd >= 0` and `pathname` is relative → readlink
///   `/proc/self/fd/<dirfd>` to get the absolute dir, join.
///
/// Returns the resolved path as a Rust `String` on success, `None`
/// when the readlink fails (we don't want to block the open just to
/// resolve a name; the syscall has already happened by the time we
/// emit). All filesystem I/O here happens via libc syscalls that are
/// guarded by [`IN_HOOK`] so we don't re-enter our own shadows.
fn resolve_at(dirfd: c_int, pathname: &CStr) -> Option<String> {
    let path_str = pathname.to_str().ok()?;
    if path_str.starts_with('/') {
        return Some(path_str.to_string());
    }
    // AT_FDCWD = -100 per `<fcntl.h>`. libc has the constant but the
    // exact value is part of the stable kernel ABI.
    const AT_FDCWD: c_int = -100;
    if dirfd == AT_FDCWD {
        return Some(path_str.to_string());
    }
    if dirfd < 0 {
        return None;
    }
    // Read /proc/self/fd/<dirfd> via direct readlink to avoid Rust's
    // std::fs which would loop through our own shadows.
    let link = format!("/proc/self/fd/{dirfd}\0");
    let mut buf = [0u8; libc::PATH_MAX as usize];
    let n = unsafe {
        libc::readlink(
            link.as_ptr() as *const c_char,
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
        )
    };
    if n <= 0 {
        return None;
    }
    let dir = std::str::from_utf8(&buf[..n as usize]).ok()?;
    Some(format!("{dir}/{path_str}"))
}

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
    let line = format!("RPP_HOOK file-open path={path:?} flags={flags} fd={fd}\n");
    emit_line(&line);
}

fn emit_close(path: &str, fd: c_int) {
    let line = format!("RPP_HOOK file-close path={path:?} fd={fd}\n");
    emit_line(&line);
}

fn emit_write(path: &str, fd: c_int, byte_count: i64) {
    let line = format!("RPP_HOOK file-write path={path:?} fd={fd} byte_count={byte_count}\n");
    emit_line(&line);
}

fn emit_unlink(path: &str) {
    let line = format!("RPP_HOOK file-unlink path={path:?}\n");
    emit_line(&line);
}

fn emit_rename(from: &str, to: &str) {
    let line = format!("RPP_HOOK file-rename from={from:?} to={to:?}\n");
    emit_line(&line);
}

/// Insert a fd → path mapping after a successful open/openat. Replaces
/// any prior entry for the same fd (which would only happen if a
/// close was missed).
fn fd_table_insert(fd: c_int, path: String) {
    if let Ok(mut tbl) = fd_table().lock() {
        tbl.insert(fd, path);
    }
}

/// Look up the path associated with `fd` without removing it. Returns
/// `None` if the fd isn't tracked (e.g. the open was done before our
/// interposer loaded, or via a syscall we don't shadow).
fn fd_table_get(fd: c_int) -> Option<String> {
    fd_table().lock().ok()?.get(&fd).cloned()
}

/// Look up + remove the path associated with `fd`. Called on `close`
/// after emitting the event.
fn fd_table_remove(fd: c_int) -> Option<String> {
    fd_table().lock().ok()?.remove(&fd)
}

/// LD_PRELOAD shadow for `open(2)`. Resolves the real implementation
/// from the pointer resolved by the load-time constructor, calls it, then emits a
/// `file-open` event on stderr.
///
/// # Safety
///
/// This is a libc-ABI extern "C" fn. The C runtime invokes it with
/// arguments matching the standard POSIX `open(2)` signature; we
/// trust those.
#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int) -> c_int {
    let real = real_open();
    if POST_FORK_CHILD.load(Ordering::Acquire) {
        return real(path, flags);
    }

    if IN_HOOK.with(|c| c.get()) {
        // Reentrant call — fall through, do not emit.
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

/// LD_PRELOAD shadow for `openat(2)`. Same flow as [`open`] — resolve
/// the real implementation, call it, then emit a `file-open` event —
/// but joins `dirfd` + `pathname` into a single resolved path via
/// [`resolve_at`] so the consumer sees absolute paths regardless of
/// how the caller invoked `openat`.
///
/// # Safety
///
/// libc-ABI extern "C" fn. The C runtime invokes it with arguments
/// matching POSIX `openat(2)`; we trust those.
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

/// LD_PRELOAD shadow for `close(2)`. Looks up the fd in the table,
/// emits a `file-close` event if tracked, removes the entry, then
/// calls the real `close`.
///
/// We emit BEFORE the real close so the path lookup is still valid;
/// emitting after would risk the path being recycled if the kernel
/// reuses the fd quickly. The window during which a successful close
/// emits an event for an fd we tracked but didn't actually close is
/// vanishingly small; if the real close returns an error the
/// downstream consumer just sees a phantom close event, which is a
/// debugging signal in itself.
///
/// # Safety
///
/// libc-ABI extern "C" fn. The C runtime invokes it with arguments
/// matching POSIX `close(2)` (a single integer fd); we trust those.
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

    if let Some(path) = fd_table_get(fd) {
        emit_close(&path, fd);
    }
    let r = real(fd);
    if r == 0 {
        // Only forget the path on a successful close; if it failed
        // the consumer may legitimately retry with the same fd.
        let _ = fd_table_remove(fd);
    }

    IN_HOOK.with(|c| c.set(false));
    r
}

/// LD_PRELOAD shadow for `write(2)`. Looks up the fd in the table
/// and emits a `file-write` event with the returned byte count when
/// the call succeeds.
///
/// **Caveat**: this only covers `write`, not `pwrite`/`writev`/
/// `pwritev`/`sendfile`. Those land in slice 4d alongside the
/// unlink/rename family.
///
/// # Safety
///
/// libc-ABI extern "C" fn. The C runtime invokes it with arguments
/// matching POSIX `write(2)` — `(int fd, const void *buf, size_t
/// count)`. The buf+count region must be valid for `count` bytes
/// of read; we don't dereference it ourselves, just forward.
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
        if let Some(path) = fd_table_get(fd) {
            emit_write(&path, fd, n as i64);
        }
    }

    IN_HOOK.with(|c| c.set(false));
    n
}

/// LD_PRELOAD shadow for `unlink(2)`. Emits `file-unlink` on success.
///
/// # Safety
///
/// libc-ABI extern "C" fn. The C runtime invokes it with arguments
/// matching POSIX `unlink(2)` (a single null-terminated path); we
/// trust those.
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

/// LD_PRELOAD shadow for `unlinkat(2)`. Resolves `(dirfd, path)`
/// via [`resolve_at`] and emits `file-unlink` on success.
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

/// LD_PRELOAD shadow for `rename(2)`. Emits `file-rename` on success.
///
/// # Safety
///
/// libc-ABI extern "C" fn. Arguments match POSIX `rename(2)` —
/// two null-terminated paths.
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

/// LD_PRELOAD shadow for `renameat(2)`. Resolves both source +
/// destination via [`resolve_at`] and emits `file-rename` on success.
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
