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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Env escape hatch (soldr#1495): set `SOLDR_DAEMON_DISPLACE=off` (or
/// `0`/`false`/`no`) to disable displacement of a stale-version daemon
/// and revert to the pre-#1495 "first daemon wins" behavior.
pub(crate) const SOLDR_DAEMON_DISPLACE_ENV: &str = "SOLDR_DAEMON_DISPLACE";

pub(crate) fn displacement_enabled() -> bool {
    match std::env::var(SOLDR_DAEMON_DISPLACE_ENV) {
        Ok(v) => {
            let v = v.trim();
            !(v.eq_ignore_ascii_case("off")
                || v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no"))
        }
        Err(_) => true,
    }
}

/// True when the running daemon's published version claim matches this
/// build's `CARGO_PKG_VERSION`. A missing claim (a pre-#1495 daemon that
/// never wrote a manifest) is treated as a mismatch — version-unknown is
/// stale — so a newer client always converges to a daemon it can name.
pub(crate) fn current_version_claim_matches(paths: &SoldrPaths) -> bool {
    crate::daemon::broker_discovery::read_claimed_service_version(paths)
        .is_some_and(|claimed| claimed == env!("CARGO_PKG_VERSION"))
}

/// Version-aware liveness: the daemon is live (passes the existing PID +
/// nonce/IPC probe, which already rejects a different `PROTOCOL_VERSION`)
/// **and** its package-version claim matches this build. Returns the PID
/// only when both hold. Used by the spawn path and the managed-build
/// preflight to decide whether the running daemon is the one this client
/// wants, or a stale version to displace.
pub fn is_live_current_version(paths: &SoldrPaths) -> Option<u32> {
    let pid = is_live(paths)?;
    current_version_claim_matches(paths).then_some(pid)
}

/// Version-blind occupancy check: is the singleton endpoint held by a
/// live soldr daemon process at all? Accepts both the sibling
/// `soldr-daemon` binary and the via-self `soldr` form (CI / slim
/// deployments spawn `soldr daemon start`). Returns its PID. This is the
/// signal that a stale daemon must be *displaced* before a new one can
/// bind — distinct from `is_live` (which additionally requires the IPC
/// probe to succeed, and so misses a protocol-mismatched daemon that is
/// nonetheless holding the socket).
pub fn stale_daemon_occupies_endpoint(paths: &SoldrPaths) -> Option<u32> {
    let (pid, _exe) = read_pid_file(paths)?;
    pid_is_soldr_daemon(pid).then_some(pid)
}

/// PID-recycling-safe identity gate for a signalled kill: the PID must be
/// alive and its running image must look like one of soldr's own daemon
/// process names. A recycled PID running an unrelated program fails the
/// stem check, so we never signal a stranger.
fn pid_is_soldr_daemon(pid: u32) -> bool {
    pid_is_alive(pid)
        && (pid_exe_stem_matches(
            pid,
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME,
        ) || pid_exe_stem_matches(pid, "soldr"))
}

/// Displace the stale daemon currently holding the endpoint so a
/// current-version daemon can take over (soldr#1495). Graceful first:
/// ask it to shut down over the wire (works for a same-`PROTOCOL_VERSION`
/// stale release). If that does not evict it within the budget, fall back
/// to a verified-PID signal (SIGTERM→SIGKILL / TerminateProcess) — always
/// re-verifying the PID is alive and still a soldr daemon before
/// signalling. On success the stale PID file, socket, and version claim
/// are removed so the successor can bind cleanly. Returns whether the
/// endpoint was freed.
pub fn displace_stale_daemon(paths: &SoldrPaths) -> bool {
    let Some(pid) = stale_daemon_occupies_endpoint(paths) else {
        // Nothing live is holding the endpoint; clear any stale files.
        cleanup_after_displacement(paths);
        return true;
    };

    append_lifecycle_event(paths, "displace-stale-requested");

    // 1) Graceful wire shutdown.
    let sock = crate::daemon::client::default_sock_path(paths);
    let _ = crate::daemon::client::shutdown(&sock);
    if wait_for_pid_exit(pid, Duration::from_secs(5)) {
        cleanup_after_displacement(paths);
        return true;
    }

    // 2) Verified-PID kill fallback (protocol-mismatched daemon can't
    //    answer the wire Shutdown verb).
    if pid_is_soldr_daemon(pid) {
        append_lifecycle_event(paths, "displace-kill-fallback");
        terminate_pid(pid);
        wait_for_pid_exit(pid, Duration::from_secs(5));
    }

    let freed = !pid_is_alive(pid);
    if freed {
        cleanup_after_displacement(paths);
    }
    freed
}

/// One-shot preflight for the managed-build front door (soldr#1495).
/// Mode 1 — a same-`PROTOCOL_VERSION` older release serving our compiles
/// — never fails the hot path, so `try_spawn_detached`'s displacement is
/// never reached during a build. Run this once at `soldr cargo` startup:
/// if a stale-version daemon holds the endpoint, displace it here so the
/// build's first wrapper call spawns a current-version daemon. A no-op
/// when displacement is disabled or the running daemon is already current.
pub fn preflight_displace_stale_daemon(paths: &SoldrPaths) {
    if !displacement_enabled() {
        return;
    }
    if is_live_current_version(paths).is_some() {
        return;
    }
    if stale_daemon_occupies_endpoint(paths).is_some() {
        displace_stale_daemon(paths);
    }
}

fn cleanup_after_displacement(paths: &SoldrPaths) {
    remove_pid_file(paths);
    crate::daemon::broker_discovery::remove_root_version_claim(paths);
    // Unix domain socket file must be unlinked before a new daemon can
    // bind the same path. On Windows the named pipe has no filesystem
    // artifact, so this is a no-op there.
    #[cfg(unix)]
    {
        let _ = fs::remove_file(crate::cache_lib::daemon_sock_path(paths));
    }
}

fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !pid_is_alive(pid)
}

#[cfg(unix)]
fn terminate_pid(pid: u32) {
    // SAFETY: kill(2) with SIGTERM then (if needed) SIGKILL. The PID was
    // just verified alive + soldr-daemon by the caller.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    if wait_for_pid_exit(pid, Duration::from_secs(3)) {
        return;
    }
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(windows)]
#[allow(non_snake_case)]
fn terminate_pid(pid: u32) {
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
    // SAFETY: OpenProcess for a verified soldr-daemon PID; TerminateProcess
    // is the Windows equivalent of SIGKILL (the daemon holds no
    // filesystem lock we need graceful about — cleanup_after_displacement
    // removes the pid file + version claim).
    let h = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if h.is_null() {
        return;
    }
    unsafe {
        TerminateProcess(h, 1);
        CloseHandle(h);
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
/// Best-effort: returns Ok(()) on spawn success, Err otherwise. The
/// spawn owner keeps the herd lock briefly after spawn while waiting
/// for the daemon endpoint to become live; if readiness still races or
/// times out, callers must keep using their normal retry budget.
pub fn try_spawn_detached() -> Result<(), LifecycleError> {
    let current = std::env::current_exe().map_err(|_| LifecycleError::NoExe)?;
    // Prefer the sibling `soldr-daemon` binary (dev builds + maturin
    // wheels ship both). Fall back to the running soldr binary itself
    // invoked as `soldr daemon start --foreground` when the sibling
    // isn't present — this lets CI workflows and slimmed-down
    // deployments (which historically distributed only `soldr`) still
    // bring up the daemon now that Phase 5/7 made the embedded
    // backend mandatory. The daemon subcommand is already a clap-
    // matched verb in `cli_args.rs`; the `soldr-daemon` argv[0] alias
    // routes through the main binary.
    let sibling = crate::daemon::service_definition::sibling_daemon_binary(&current);
    let (daemon_src, daemon_via_self) = if sibling.exists() {
        (sibling, false)
    } else {
        (current.clone(), true)
    };

    let paths = SoldrPaths::new().ok();
    let _spawn_lock = paths.as_ref().and_then(acquire_spawn_lock);
    // Re-check liveness while holding the lock (or after failing to
    // acquire it): if a daemon of THIS version already brought the
    // endpoint up we short-circuit before doing relocate + spawn.
    // soldr#1495: if a *stale-version* daemon holds the endpoint instead,
    // displace it (graceful shutdown → verified-PID kill) so our spawn
    // can bind — this is what breaks the version shadow, both for a
    // silently-serving older release and a protocol-mismatched daemon.
    if let Some(p) = paths.as_ref() {
        if is_live_current_version(p).is_some() {
            return Ok(());
        }
        if displacement_enabled() && stale_daemon_occupies_endpoint(p).is_some() {
            displace_stale_daemon(p);
        }
    }
    // Without the lock, another wrapper is currently mid-spawn. Don't
    // pile on — the next wrapper will reprobe.
    if paths.is_some() && _spawn_lock.is_none() {
        return Ok(());
    }

    let relocated = resolve_daemon_spawn_image(paths.as_ref(), &daemon_src);

    if !daemon_via_self && !crate::daemon::backend_handle_adoption::running_process_disabled() {
        let _ = crate::daemon::service_definition::install_service_definition(&relocated);
    }
    let spawn_result = if daemon_via_self {
        spawn_detached_self_inner(&relocated).map_err(LifecycleError::Spawn)
    } else {
        spawn_detached_inner(&relocated).map_err(LifecycleError::Spawn)
    };
    spawn_result?;

    // Keep the spawn lock held until the daemon has written its PID file and
    // answered the active endpoint probe. Without this, a cargo fan-out on
    // Windows can acquire the lock sequentially in several rustc-wrapper
    // processes and spawn multiple `soldr daemon start --foreground` children
    // before the first one is ready.
    if let Some(paths) = paths.as_ref() {
        wait_for_spawned_daemon_ready(paths, Duration::from_secs(5));
    }

    Ok(())
}

/// Resolve the on-disk image the daemon child will exec from.
///
/// Every spawn shape — the sibling `soldr-daemon` binary AND the
/// via-self `soldr daemon start --foreground` fallback — routes through
/// `ensure_daemon_relocated` into `~/.soldr/runtime/soldr-daemon/`
/// (issue #1516). Via-self used to skip relocation and pin whatever
/// `current_exe` resolved to; when the self-relocation guard env vars
/// leak in from a parent process (or on slim installs), that pinned the
/// package-manager-owned `Scripts\soldr.exe`, so `pip install
/// --force-reinstall` wedged with WinError 5 while the daemon lived.
/// Relocating decouples the long-lived daemon from the installed
/// binary; `ensure_daemon_relocated` still runs maturin-repaired-wheel
/// layouts in place (soldr#1300) and no-ops when the source already
/// lives under the daemon runtime root.
///
/// Relocation failures fall back to running the source in place — a
/// pinned daemon beats no daemon.
fn resolve_daemon_spawn_image(paths: Option<&SoldrPaths>, daemon_src: &Path) -> PathBuf {
    match paths {
        Some(paths) => crate::self_relocate::ensure_daemon_relocated(paths, daemon_src)
            .inspect(|r| {
                crate::self_relocate::run_periodic_daemon_runtime_gc(paths, Some(r));
            })
            .unwrap_or_else(|_| daemon_src.to_path_buf()),
        // No cache root resolved → run in place. The daemon itself
        // tries SoldrPaths::new() at startup and will surface the
        // same error there.
        None => daemon_src.to_path_buf(),
    }
}

fn wait_for_spawned_daemon_ready(paths: &SoldrPaths, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_live(paths).is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
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
    spawn_detached_windows_no_inherit(daemon, daemon, &["--foreground"])
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
    // The process that discovers a missing daemon may itself be the
    // `zccache-soldr` hardlink. Force argv[0] back to the main CLI identity;
    // otherwise multicall dispatch treats `daemon` as a compiler path and
    // recursively enters the wrapper fallback instead of starting a daemon.
    force_daemon_via_self_cli_identity(&mut cmd);
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

#[cfg(unix)]
fn force_daemon_via_self_cli_identity(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.arg0("soldr");
}

#[cfg(windows)]
fn spawn_detached_self_inner(soldr_self: &Path) -> Result<(), std::io::Error> {
    spawn_detached_windows_no_inherit(
        soldr_self,
        Path::new("soldr"),
        &["daemon", "start", "--foreground"],
    )
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms)]
fn spawn_detached_windows_no_inherit(
    program: &Path,
    argv0: &Path,
    args: &[&str],
) -> Result<(), std::io::Error> {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::raw::HANDLE;
    use std::ptr::{null, null_mut};

    #[allow(non_camel_case_types)]
    // Win32 API spelling — clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(non_camel_case_types)]
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    #[allow(non_camel_case_types)]
    type LPVOID = *mut c_void;
    #[allow(non_camel_case_types)]
    type LPCVOID = *const c_void;
    #[allow(non_camel_case_types)]
    type LPCWSTR = *const u16;
    #[allow(non_camel_case_types)]
    type LPWSTR = *mut u16;
    #[allow(non_camel_case_types)]
    type WORD = u16;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct STARTUPINFOW {
        cb: DWORD,
        lpReserved: LPWSTR,
        lpDesktop: LPWSTR,
        lpTitle: LPWSTR,
        dwX: DWORD,
        dwY: DWORD,
        dwXSize: DWORD,
        dwYSize: DWORD,
        dwXCountChars: DWORD,
        dwYCountChars: DWORD,
        dwFillAttribute: DWORD,
        dwFlags: DWORD,
        wShowWindow: WORD,
        cbReserved2: WORD,
        lpReserved2: *mut u8,
        hStdInput: HANDLE,
        hStdOutput: HANDLE,
        hStdError: HANDLE,
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct PROCESS_INFORMATION {
        hProcess: HANDLE,
        hThread: HANDLE,
        dwProcessId: DWORD,
        dwThreadId: DWORD,
    }

    extern "system" {
        fn CreateProcessW(
            lpApplicationName: LPCWSTR,
            lpCommandLine: LPWSTR,
            lpProcessAttributes: LPVOID,
            lpThreadAttributes: LPVOID,
            bInheritHandles: BOOL,
            dwCreationFlags: DWORD,
            lpEnvironment: LPCVOID,
            lpCurrentDirectory: LPCWSTR,
            lpStartupInfo: *mut STARTUPINFOW,
            lpProcessInformation: *mut PROCESS_INFORMATION,
        ) -> BOOL;
        fn CloseHandle(hObject: HANDLE) -> BOOL;
    }

    // CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW.
    const FLAGS: DWORD = 0x0000_0200 | 0x0000_0008 | 0x0800_0000;

    let application: Vec<u16> = program.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut command_line = build_windows_command_line(argv0, args);
    // SAFETY: STARTUPINFOW and PROCESS_INFORMATION are plain Win32 POD
    // structs. Zero initialization is the documented baseline before setting
    // STARTUPINFOW.cb and passing both structs to CreateProcessW.
    let mut startup: STARTUPINFOW = unsafe { zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as DWORD;
    let mut process_info: PROCESS_INFORMATION = unsafe { zeroed() };

    // SAFETY: application and command_line are null-terminated UTF-16 buffers
    // that live for the duration of the call. All optional pointer parameters
    // are null by design, and bInheritHandles is FALSE so the child cannot
    // inherit Cargo/test pipe handles from the wrapper process.
    let ok = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            null_mut(),
            null_mut(),
            0,
            FLAGS,
            null(),
            null(),
            &mut startup,
            &mut process_info,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: CreateProcessW initialized these handles on success; this
    // process does not need to retain either handle after the detached spawn.
    unsafe {
        CloseHandle(process_info.hThread);
        CloseHandle(process_info.hProcess);
    }
    Ok(())
}

#[cfg(windows)]
fn build_windows_command_line(program: &Path, args: &[&str]) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let mut out = Vec::new();
    out.push('"' as u16);
    out.extend(program.as_os_str().encode_wide());
    out.push('"' as u16);
    for arg in args {
        out.push(' ' as u16);
        out.extend(OsStr::new(arg).encode_wide());
    }
    out.push(0);
    out
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
    // Win32 API spelling — clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
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
    // Win32 API spelling — clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
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
mod daemon_spawn_image_tests {
    use super::*;
    use crate::core::SoldrPaths;
    use tempfile::TempDir;

    #[cfg(unix)]
    crate::timed_test!(via_self_daemon_forces_main_cli_argv0, {
        let mut command = std::process::Command::new("/bin/sh");
        force_daemon_via_self_cli_identity(&mut command);
        let output = command
            .args(["-c", "printf %s \"$0\""])
            .output()
            .expect("run shell probe");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "soldr");
    });

    #[cfg(windows)]
    crate::timed_test!(via_self_daemon_windows_command_line_uses_main_cli_argv0, {
        let command_line =
            build_windows_command_line(Path::new("soldr"), &["daemon", "start", "--foreground"]);
        let rendered = String::from_utf16_lossy(&command_line[..command_line.len() - 1]);
        assert_eq!(rendered, "\"soldr\" daemon start --foreground");
    });

    // #1516 regression: a via-self daemon (no sibling `soldr-daemon`
    // binary) must NOT exec the invoking soldr binary in place — its
    // image must live under the daemon runtime root so the installed
    // binary can be deleted/replaced while the daemon is alive.
    crate::timed_test!(
        via_self_daemon_image_is_relocated_off_the_invoking_binary,
        {
            let temp = TempDir::new().expect("tempdir");
            let install_dir = temp.path().join("Scripts");
            std::fs::create_dir_all(&install_dir).expect("install dir");
            let installed_soldr = install_dir.join("soldr.exe");
            std::fs::write(&installed_soldr, b"installed-soldr").expect("write soldr");
            let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

            let image = resolve_daemon_spawn_image(Some(&paths), &installed_soldr);

            assert_ne!(
                image, installed_soldr,
                "via-self daemon must not pin the invoking binary"
            );
            assert!(
                !image.starts_with(&install_dir),
                "daemon image {} must not live in the install dir {}",
                image.display(),
                install_dir.display()
            );
            assert!(
                image.starts_with(crate::self_relocate::daemon_runtime_root(&paths)),
                "daemon image {} must live under the daemon runtime root",
                image.display()
            );
            assert_eq!(
                std::fs::read(&image).expect("read relocated image"),
                b"installed-soldr",
                "relocated image must be a byte-identical copy"
            );
        }
    );

    // soldr#1300 constraint: maturin-repaired wheel layouts keep
    // running in place — the via-self relocation must not break them.
    crate::timed_test!(via_self_daemon_in_repaired_wheel_layout_runs_in_place, {
        let temp = TempDir::new().expect("tempdir");
        let scripts = temp.path().join("site-packages").join("soldr.scripts");
        std::fs::create_dir_all(&scripts).expect("scripts dir");
        std::fs::create_dir_all(temp.path().join("site-packages").join("soldr.dylibs"))
            .expect("dylibs dir");
        let wheel_soldr = scripts.join("soldr");
        std::fs::write(&wheel_soldr, b"wheel-soldr").expect("write soldr");
        let paths = SoldrPaths::with_root(temp.path().join("soldr-root"));

        let image = resolve_daemon_spawn_image(Some(&paths), &wheel_soldr);
        assert_eq!(
            image, wheel_soldr,
            "repaired-wheel binaries must run in place (soldr#1300)"
        );
    });

    // Without a resolvable cache root the source runs in place.
    crate::timed_test!(daemon_image_runs_in_place_without_cache_root, {
        let temp = TempDir::new().expect("tempdir");
        let src = temp.path().join("soldr.exe");
        std::fs::write(&src, b"soldr").expect("write soldr");
        assert_eq!(resolve_daemon_spawn_image(None, &src), src);
    });
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
    fn displacement_enabled_by_default_and_off_via_env() {
        // Default (unset) → enabled. The explicit off-values disable it.
        // Uses a process-global env var; keep this the only test that
        // touches SOLDR_DAEMON_DISPLACE so it can't race a sibling.
        let prior = std::env::var_os(SOLDR_DAEMON_DISPLACE_ENV);
        std::env::remove_var(SOLDR_DAEMON_DISPLACE_ENV);
        assert!(displacement_enabled(), "unset must be enabled");
        for off in ["off", "0", "false", "no", "OFF"] {
            std::env::set_var(SOLDR_DAEMON_DISPLACE_ENV, off);
            assert!(!displacement_enabled(), "{off} must disable");
        }
        std::env::set_var(SOLDR_DAEMON_DISPLACE_ENV, "on");
        assert!(displacement_enabled(), "any other value stays enabled");
        match prior {
            Some(v) => std::env::set_var(SOLDR_DAEMON_DISPLACE_ENV, v),
            None => std::env::remove_var(SOLDR_DAEMON_DISPLACE_ENV),
        }
    }

    #[test]
    fn current_version_claim_matches_only_for_this_build() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        // No claim → version-unknown → not current.
        assert!(!current_version_claim_matches(&paths));

        // This build's own claim → current.
        crate::daemon::broker_discovery::write_root_version_claim(&paths).expect("write claim");
        assert!(current_version_claim_matches(&paths));

        // A stale writer's claim → not current (the mismatch that drives
        // displacement).
        use running_process::broker::protocol_v2::{write_to_root_v2, CacheManifestBuilder};
        let stale = CacheManifestBuilder::new(
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME,
            "0.0.0-stale",
        )
        .build();
        write_to_root_v2(&paths.root, &stale).expect("write stale");
        assert!(!current_version_claim_matches(&paths));
    }

    #[test]
    fn stale_daemon_occupancy_ignores_dead_pid() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        std::fs::create_dir_all(soldr_daemon_dir(&paths)).expect("daemon dir");
        // A large positive PID that is almost certainly not a running
        // process. (Not `u32::MAX`, which casts to the `-1` "all
        // processes" wildcard on Unix and would spuriously look alive.)
        std::fs::write(
            daemon_pid_path(&paths),
            format!("{}\nsoldr-daemon\n", i32::MAX as u32),
        )
        .expect("pid file");
        assert!(stale_daemon_occupies_endpoint(&paths).is_none());
        // Displacing a non-occupied endpoint is a successful no-op that
        // clears any stale files.
        assert!(displace_stale_daemon(&paths));
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
