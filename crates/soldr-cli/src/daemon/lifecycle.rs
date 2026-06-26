//! PID-file based lifecycle for soldr-daemon: detect a live daemon,
//! spawn one detached when none is found, append structured JSONL
//! lifecycle events.
//!
//! The PID file stores two lines: the decimal PID and the absolute path
//! to the daemon executable. Readers verify both — the file is only
//! authoritative if the PID is alive AND its exe stem is
//! `soldr-daemon`. Defends against recycled PIDs the way zccache does.

use crate::cache_lib::{daemon_lifecycle_log_path, daemon_pid_path, soldr_daemon_dir};
use crate::core::SoldrPaths;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum LifecycleError {
    Io(std::io::Error),
    NoExe,
    Spawn(std::io::Error),
}

impl From<std::io::Error> for LifecycleError {
    fn from(e: std::io::Error) -> Self {
        LifecycleError::Io(e)
    }
}

/// Read the PID file. Returns `(pid, exe_path)` if the file is well
/// formed; absent / malformed reads return None. **Does not** verify
/// liveness — that's `is_live`.
pub fn read_pid_file(paths: &SoldrPaths) -> Option<(u32, PathBuf)> {
    let raw = fs::read_to_string(daemon_pid_path(paths)).ok()?;
    let mut lines = raw.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let exe = lines.next()?.trim();
    if exe.is_empty() {
        return None;
    }
    Some((pid, PathBuf::from(exe)))
}

/// Verify the daemon recorded in the PID file is still alive AND its
/// running exe stem looks like a soldr-daemon. Returns the PID on
/// success, None on any mismatch / missing file.
pub fn is_live(paths: &SoldrPaths) -> Option<u32> {
    is_live_with_running_process_disabled(
        paths,
        crate::daemon::backend_handle_adoption::running_process_disabled(),
    )
}

pub(crate) fn is_live_with_running_process_disabled(
    paths: &SoldrPaths,
    running_process_disabled: bool,
) -> Option<u32> {
    if running_process_disabled {
        return direct_pid_file_live(paths);
    }

    // Full v1 broker adoption (zackees/running-process#434): try broker
    // discovery first. The broker negotiates a verified backend endpoint via a
    // Hello handshake. When the broker is unreachable, refuses, or is disabled,
    // `broker_discovery::soldr_daemon_pid_via_broker` returns None and we fall
    // through to the existing direct `BackendHandle` probe — keeping the direct
    // soldr-daemon path active during the rollout window.
    crate::daemon::broker_discovery::soldr_daemon_pid_via_broker(paths)
        .or_else(|| direct_backend_handle_probe(paths))
}

/// The pre-#434 direct discovery path: probe the local PID file's recorded
/// daemon with the `running-process` `BackendHandle` nonce challenge. Kept as
/// the fall-through when broker discovery does not resolve a backend.
pub(crate) fn direct_backend_handle_probe(paths: &SoldrPaths) -> Option<u32> {
    crate::daemon::backend_handle_adoption::probe_soldr_daemon(paths).map(|handle| handle.pid())
}

pub(crate) fn direct_pid_file_live(paths: &SoldrPaths) -> Option<u32> {
    direct_pid_file_live_for_stem(
        paths,
        crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME,
    )
}

fn direct_pid_file_live_for_stem(paths: &SoldrPaths, expected_stem: &str) -> Option<u32> {
    let (pid, _exe_path) = read_pid_file(paths)?;
    if pid_is_alive(pid) && pid_exe_stem_matches(pid, expected_stem) {
        Some(pid)
    } else {
        None
    }
}

/// Write the PID file for the running daemon. Overwrites any stale
/// content. Caller is responsible for ensuring `soldr_daemon_dir`
/// exists.
pub fn write_pid_file(paths: &SoldrPaths) -> Result<(), LifecycleError> {
    let exe = std::env::current_exe().map_err(|_| LifecycleError::NoExe)?;
    fs::create_dir_all(soldr_daemon_dir(paths))?;
    let pid = std::process::id();
    let contents = format!("{pid}\n{}\n", exe.display());
    fs::write(daemon_pid_path(paths), contents)?;
    Ok(())
}

pub fn remove_pid_file(paths: &SoldrPaths) {
    let _ = fs::remove_file(daemon_pid_path(paths));
}

#[derive(Serialize)]
struct LifecycleEvent<'a> {
    ts_ms: i64,
    pid: u32,
    event: &'a str,
}

pub fn append_lifecycle_event(paths: &SoldrPaths, event: &str) {
    let pid = std::process::id();
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let line = match serde_json::to_string(&LifecycleEvent { ts_ms, pid, event }) {
        Ok(s) => s,
        Err(_) => return,
    };
    let path = daemon_lifecycle_log_path(paths);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Attempt to spawn a detached `soldr-daemon`. Resolves the daemon
/// binary as a sibling of the current `soldr` executable, **relocates
/// it into `~/.soldr/runtime/soldr-daemon/<hash>/`** so the long-lived
/// daemon doesn't hold a file lock on the original (worktree target/,
/// site-packages, package upgrade, etc.) — and only then spawns.
/// Mirrors the pattern `self_relocate.rs` already uses for `soldr`
/// itself, sharing the same hash / lock / periodic-GC machinery via a
/// sibling `runtime/soldr-daemon/` sub-tree.
///
/// Spawn-herd safety (issue #474): when a `soldr cargo build` fans out
/// hundreds of parallel rustc invocations, EVERY wrapper sees the
/// daemon missing simultaneously and races to spawn it. To keep that
/// from forking N children, we take an OS-level non-blocking exclusive
/// lock on `<cache>/soldr-daemon/.spawn.lock` before relocating /
/// spawning. Lock losers re-check liveness (the lock holder is racing
/// `write_pid_file`) and short-circuit with `Ok(())` once the daemon
/// shows up. If the lock is unobtainable AND no daemon ever appears
/// within a short window, the loser still returns `Ok(())` — the next
/// wrapper invocation will reprobe and try again.
///
/// Best-effort: returns Ok(()) on spawn success, Err otherwise. Caller
/// MUST treat the daemon as eventually-consistent — the spawn returns
/// before the socket is ready.
pub fn try_spawn_detached() -> Result<(), LifecycleError> {
    let current = std::env::current_exe().map_err(|_| LifecycleError::NoExe)?;
    // Prefer the sibling `soldr-daemon` binary (dev builds + maturin
    // wheels ship both). Fall back to the running soldr binary itself
    // invoked as `soldr daemon start --foreground` when the sibling
    // isn't present — this lets CI workflows and slimmed-down
    // deployments (which historically distributed only `soldr`) still
    // bring up the daemon now that Phase 5/7 made the embedded
    // backend mandatory. The daemon subcommand is already a clap-
    // matched verb in `cli_args.rs`; the bin target at
    // `src/bin/soldr_daemon.rs` is just an alias for that subcommand
    // routed through the main binary.
    let sibling = crate::daemon::service_definition::sibling_daemon_binary(&current);
    let (daemon_src, daemon_via_self) = if sibling.exists() {
        (sibling, false)
    } else {
        (current.clone(), true)
    };

    let paths = SoldrPaths::new().ok();
    let _spawn_lock = paths.as_ref().and_then(acquire_spawn_lock);
    // Re-check liveness while holding the lock (or after failing to
    // acquire it): if another wrapper already brought the daemon up
    // we can short-circuit before doing relocate + spawn.
    if let Some(p) = paths.as_ref() {
        if is_live(p).is_some() {
            return Ok(());
        }
    }
    // Without the lock, another wrapper is currently mid-spawn. Don't
    // pile on — the next wrapper will reprobe.
    if paths.is_some() && _spawn_lock.is_none() {
        return Ok(());
    }

    let relocated = match (paths.as_ref(), daemon_via_self) {
        // When falling back to the running soldr binary, skip the
        // self_relocate dance — it's specifically for the sibling
        // daemon binary, not for soldr-self invocations. The current
        // exe stays where it is.
        (_, true) => daemon_src.clone(),
        (Some(paths), false) => crate::self_relocate::ensure_daemon_relocated(paths, &daemon_src)
            .inspect(|r| {
                crate::self_relocate::run_periodic_daemon_runtime_gc(paths, Some(r));
            })
            .unwrap_or_else(|_| daemon_src.clone()),
        // No cache root resolved → run in place. The daemon itself
        // tries SoldrPaths::new() at startup and will surface the
        // same error there.
        (None, false) => daemon_src,
    };

    if !daemon_via_self && !crate::daemon::backend_handle_adoption::running_process_disabled() {
        let _ = crate::daemon::service_definition::install_service_definition(&relocated);
    }
    if daemon_via_self {
        spawn_detached_self_inner(&relocated).map_err(LifecycleError::Spawn)
    } else {
        spawn_detached_inner(&relocated).map_err(LifecycleError::Spawn)
    }
}

/// Acquire the spawn-herd lock. Returns `Some(file)` when the
/// non-blocking exclusive lock was claimed (this caller is the spawn
/// owner), `None` when another wrapper already holds it (this caller
/// is a loser and should bail). The lock is released when the returned
/// `File` is dropped — typically at the end of `try_spawn_detached`.
///
/// Errors creating/opening the lock file are treated as "no lock
/// available" so a broken filesystem doesn't gate progress; we'd
/// rather have the herd-spawn fallback than block the build.
///
/// Exposed as `pub(crate)` so the unit tests below can verify the
/// exclusivity invariant without spawning a real daemon binary.
pub(crate) fn acquire_spawn_lock(paths: &SoldrPaths) -> Option<std::fs::File> {
    use fs2::FileExt;
    let dir = crate::cache_lib::soldr_daemon_dir(paths);
    std::fs::create_dir_all(&dir).ok()?;
    let lock_path = dir.join(".spawn.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    match file.try_lock_exclusive() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

#[cfg(unix)]
fn spawn_detached_inner(daemon: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(daemon);
    cmd.arg("--foreground").stdin(Stdio::null());
    // Diagnostic redirect: spawn the daemon's stderr/stdout to a
    // log file under the soldr cache root so a startup crash leaves
    // an artifact the wrapper can surface later. Falls back to
    // /dev/null if the path can't be created (preserves the original
    // contract).
    let log_path = SoldrPaths::new()
        .ok()
        .map(|p| p.root.join("daemon-spawn.log"));
    if let Some(path) = &log_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let stdout_file = file.try_clone().unwrap_or_else(|_| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .expect("dev/null must open")
            });
            cmd.stdout(stdout_file).stderr(file);
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    unsafe {
        cmd.pre_exec(|| {
            // Detach from the parent's process group so the daemon
            // survives the wrapper's exit.
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(windows)]
fn spawn_detached_inner(daemon: &Path) -> Result<(), std::io::Error> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW
    const FLAGS: u32 = 0x0000_0200 | 0x0000_0008 | 0x0800_0000;

    Command::new(daemon)
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(FLAGS)
        .spawn()?;
    Ok(())
}

/// Spawn the daemon via `<current-soldr-exe> daemon start --foreground`
/// rather than via the sibling `soldr-daemon` binary. Used by
/// [`try_spawn_detached`] when the sibling daemon binary is missing —
/// CI environments and slimmed-down deployments historically ship only
/// the soldr binary. Same detach semantics as
/// [`spawn_detached_inner`].
#[cfg(unix)]
fn spawn_detached_self_inner(soldr_self: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(soldr_self);
    cmd.args(["daemon", "start", "--foreground"])
        .stdin(Stdio::null());
    let log_path = SoldrPaths::new()
        .ok()
        .map(|p| p.root.join("daemon-spawn.log"));
    if let Some(path) = &log_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let stdout_file = file.try_clone().unwrap_or_else(|_| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .expect("dev/null must open")
            });
            cmd.stdout(stdout_file).stderr(file);
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

#[cfg(windows)]
fn spawn_detached_self_inner(soldr_self: &Path) -> Result<(), std::io::Error> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const FLAGS: u32 = 0x0000_0200 | 0x0000_0008 | 0x0800_0000;

    Command::new(soldr_self)
        .args(["daemon", "start", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(FLAGS)
        .spawn()?;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is a well-defined liveness probe — no
    // signal is delivered, the syscall just returns 0 if the pid
    // exists and the caller has permission to signal it.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms, non_snake_case)]
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    use std::os::windows::raw::HANDLE;
    type DWORD = u32;
    type BOOL = i32;
    const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
    const STILL_ACTIVE: DWORD = 259;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn CloseHandle(h: HANDLE) -> BOOL;
        fn GetExitCodeProcess(h: HANDLE, code: *mut DWORD) -> BOOL;
    }
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut code: DWORD = 0;
    let ok = unsafe { GetExitCodeProcess(h, &mut code) };
    unsafe { CloseHandle(h) };
    ok != 0 && code == STILL_ACTIVE
}

#[cfg(unix)]
pub(crate) fn pid_exe_stem_matches(pid: u32, expected_stem: &str) -> bool {
    let link = PathBuf::from(format!("/proc/{pid}/exe"));
    match fs::read_link(&link) {
        Ok(p) => p
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s == expected_stem)
            .unwrap_or(false),
        // macOS / BSDs don't have /proc/<pid>/exe. The liveness probe
        // alone is already a strong signal; degrade gracefully by
        // trusting the PID file rather than rejecting it.
        Err(_) => true,
    }
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms, non_snake_case)]
pub(crate) fn pid_exe_stem_matches(pid: u32, expected_stem: &str) -> bool {
    use std::os::windows::raw::HANDLE;
    type DWORD = u32;
    type BOOL = i32;
    type WCHAR = u16;
    const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn CloseHandle(h: HANDLE) -> BOOL;
        fn QueryFullProcessImageNameW(
            h: HANDLE,
            flags: DWORD,
            buf: *mut WCHAR,
            size: *mut DWORD,
        ) -> BOOL;
    }
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut buf: Vec<WCHAR> = vec![0; 1024];
    let mut size: DWORD = buf.len() as DWORD;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return false;
    }
    let s: String = String::from_utf16_lossy(&buf[..size as usize]);
    Path::new(&s)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case(expected_stem))
        .unwrap_or(false)
}

#[cfg(test)]
mod spawn_lock_tests {
    use super::*;
    use crate::core::SoldrPaths;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    #[test]
    fn spawn_lock_is_exclusive_within_a_single_process() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        let first = acquire_spawn_lock(&paths).expect("first acquire");
        // Within the same process, a second non-blocking exclusive
        // lock attempt against the same file MUST be refused —
        // otherwise the spawn-herd cap (issue #474 acceptance
        // criterion) can't possibly hold.
        let second = acquire_spawn_lock(&paths);
        assert!(
            second.is_none(),
            "second acquire while first is held must return None",
        );
        drop(first);
        // After release, the next call gets the lock back.
        let third = acquire_spawn_lock(&paths).expect("third acquire after release");
        drop(third);
    }

    #[test]
    fn direct_pid_file_live_accepts_expected_process_stem() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        std::fs::create_dir_all(soldr_daemon_dir(&paths)).expect("daemon dir");
        let current_exe = std::env::current_exe().expect("current exe");
        let current_stem = current_exe
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("current exe stem")
            .to_string();
        std::fs::write(
            daemon_pid_path(&paths),
            format!("{}\n{}\n", std::process::id(), current_exe.display()),
        )
        .expect("pid file");

        assert_eq!(
            direct_pid_file_live_for_stem(&paths, &current_stem),
            Some(std::process::id())
        );
    }

    #[test]
    fn spawn_lock_serializes_concurrent_threads() {
        let temp = TempDir::new().expect("tempdir");
        let paths = Arc::new(SoldrPaths::with_root(temp.path().to_path_buf()));
        const THREADS: usize = 16;
        let barrier = Arc::new(Barrier::new(THREADS));
        let success_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let paths = paths.clone();
            let barrier = barrier.clone();
            let counter = success_count.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                // Race-start: try to acquire. Holders hold the lock
                // briefly to simulate the relocate+spawn work the
                // real call would do.
                if let Some(guard) = acquire_spawn_lock(&paths) {
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    drop(guard);
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }
        // Each successful acquire holds the lock for ~10ms; we expect
        // at MOST a handful to land sequentially in the few hundred
        // ms the test takes, but cap at THREADS - 1 because if all
        // threads acquired the lock it would defeat the purpose.
        let count = success_count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            count >= 1,
            "at least one thread must acquire the lock; got {count}",
        );
        assert!(
            count < THREADS,
            "lock must serialize — fewer than {THREADS} acquires expected (the spawn-herd cap from #474); got {count}",
        );
    }
}
