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
    let (pid, _) = read_pid_file(paths)?;
    if pid_is_alive(pid) && pid_exe_stem_matches(pid, "soldr-daemon") {
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
/// Best-effort: returns Ok(()) on spawn success, Err otherwise. Caller
/// MUST treat the daemon as eventually-consistent — the spawn returns
/// before the socket is ready.
pub fn try_spawn_detached() -> Result<(), LifecycleError> {
    let current = std::env::current_exe().map_err(|_| LifecycleError::NoExe)?;
    let daemon_src = sibling_daemon_binary(&current);
    if !daemon_src.exists() {
        return Err(LifecycleError::NoExe);
    }

    let relocated = match SoldrPaths::new() {
        Ok(paths) => {
            let r = crate::self_relocate::ensure_daemon_relocated(&paths, &daemon_src)
                .unwrap_or_else(|_| daemon_src.clone());
            crate::self_relocate::run_periodic_daemon_runtime_gc(&paths, Some(&r));
            r
        }
        // No cache root resolved → run in place. The daemon itself
        // tries SoldrPaths::new() at startup and will surface the
        // same error there.
        Err(_) => daemon_src,
    };

    spawn_detached_inner(&relocated).map_err(LifecycleError::Spawn)
}

fn sibling_daemon_binary(current: &Path) -> PathBuf {
    let stem = if cfg!(windows) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    current
        .parent()
        .map(|p| p.join(stem))
        .unwrap_or_else(|| PathBuf::from(stem))
}

#[cfg(unix)]
fn spawn_detached_inner(daemon: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(daemon);
    cmd.arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is a well-defined liveness probe — no
    // signal is delivered, the syscall just returns 0 if the pid
    // exists and the caller has permission to signal it.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms, non_snake_case)]
fn pid_is_alive(pid: u32) -> bool {
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
fn pid_exe_stem_matches(pid: u32, expected_stem: &str) -> bool {
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
fn pid_exe_stem_matches(pid: u32, expected_stem: &str) -> bool {
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
