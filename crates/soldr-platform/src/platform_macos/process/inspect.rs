//! macOS PID inspection: liveness, zombie state, and image lookup.

use std::path::{Path, PathBuf};

/// macOS does not use the Windows console-attachment policy probe.
pub fn console_attached(_pid: u32) -> Option<bool> {
    None
}
use std::time::Duration;

/// A process observed running from inside a directory tree.
///
/// macOS cannot enumerate image paths process-wide, so [`holders_under`]
/// answers empty here — the diagnosis it feeds is Windows-specific
/// (elsewhere an unlink succeeds against a running image).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessHolder {
    /// The process id.
    pub pid: u32,
    /// The fully-resolved executable image path.
    pub exe: PathBuf,
}

/// True while `pid` names a live macOS process.
///
/// A zombie (an exited child awaiting reap) still answers `kill(pid, 0)`
/// but can never serve IPC again, so it is reported as dead.
pub fn is_alive(pid: u32) -> bool {
    // A pid the kernel cannot name is not alive. Callers hand us `u32` from
    // pid files, state rows and environment variables, and `pid_t` is signed:
    // 4294967295 would reach `kill` as -1, which means "every process I may
    // signal" and would answer this probe with a confident `true`.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) is a well-defined liveness probe — no signal is
    // delivered, the syscall just returns 0 if the pid exists and the
    // caller has permission to signal it.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    !is_zombie(pid as u32)
}

/// True when `pid` names a process that has exited but is still awaiting
/// collection by its parent.
///
/// macOS has no `/proc`: `proc_pidinfo(PROC_PIDTBSDINFO)` is the supported
/// libproc query for a process's BSD state, and `pbi_status` reports `SZOMB`
/// for an unreaped child. Without this probe a daemon spawned as a direct
/// child stays "alive" to `kill(pid, 0)` forever, so every synchronous
/// shutdown wait burns its full timeout.
pub fn is_zombie(pid: u32) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // `arg` MUST be non-zero: for PROC_PIDTBSDINFO the kernel only falls
    // back to `proc_find_zombref` when `arg != 0` (xnu bsd/kern/proc_info.c).
    // With `arg == 0` a zombie fails the `proc_find` lookup and the call
    // returns ESRCH, which would defeat the entire purpose of this probe.
    const FIND_ZOMBIE: u64 = 1;
    // SAFETY: `proc_pidinfo` writes at most `size` bytes into the buffer and
    // reports how many it wrote. The struct is plain-old-data and is only
    // read after a full-size write is confirmed.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            FIND_ZOMBIE,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return false;
    }
    // SAFETY: the call above filled the whole struct.
    unsafe { info.assume_init() }.pbi_status == libc::SZOMB
}

/// Read the running process's executable path via `ps`.
pub fn executable_path(pid: u32) -> Option<PathBuf> {
    use std::io::Read;

    let mut command = std::process::Command::new("/bin/ps");
    command.args(["-p", &pid.to_string(), "-o", "comm="]);
    let stdio = running_process::SpawnStdio {
        stdin: running_process::StdioSource::Null,
        stdout: running_process::StdioSource::Pipe,
        stderr: running_process::StdioSource::Null,
        drain_timeout: Some(Duration::from_secs(2)),
        show_console: false,
    };
    let mut child = running_process::spawn(&mut command, stdio).ok()?;
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    if child.wait().ok()? != 0 {
        return None;
    }
    let image = String::from_utf8(stdout).ok()?;
    let image = image.trim();
    (!image.is_empty()).then(|| PathBuf::from(image))
}

/// True when the running image's file stem matches `expected_stem`.
/// Absence is deliberately a mismatch: callers use this check immediately
/// before signalling a PID, so an unavailable probe must never turn a stale
/// claim into authority to terminate an unrelated process.
pub fn executable_stem_matches(pid: u32, expected_stem: &str) -> bool {
    executable_path(pid)
        .as_deref()
        .and_then(Path::file_stem)
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == expected_stem)
}

/// True when the running image resolves to `expected_path`.
pub fn executable_path_matches(pid: u32, expected_path: &Path) -> bool {
    let Some(actual) = executable_path(pid) else {
        return false;
    };
    let actual = std::fs::canonicalize(&actual).unwrap_or(actual);
    let expected = std::fs::canonicalize(expected_path)
        .unwrap_or_else(|_| expected_path.to_path_buf());
    actual == expected
}

/// macOS cannot walk process images; the running-image deletion problem
/// this diagnoses is Windows-specific.
pub fn holders_under(_dir: &Path) -> Vec<ProcessHolder> {
    Vec::new()
}

/// A PID-reuse-safe identity token for `pid`: its creation time.
///
/// `proc_pidinfo(PROC_PIDTBSDINFO)` -- the same call `is_zombie` above uses
/// for `pbi_status` -- also reports `pbi_start_tvsec`/`pbi_start_tvusec`, the
/// process's creation time to microsecond resolution. That pairing is stable
/// for the life of the process and changes whenever the kernel reuses the pid
/// onto something else, which is exactly the identity soldr#3054's broker
/// route reaper needs so a recycled requester pid cannot be mistaken for the
/// process that originally asked for a route.
///
/// `None` on any failure -- an exited or unreadable process. `None` must
/// never be treated as a match by a caller comparing tokens.
pub fn process_start_token(pid: u32) -> Option<u64> {
    // Same out-of-range guard as `is_alive`/`signal_pid`.
    let pid = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    const FIND_ZOMBIE: u64 = 1;
    // SAFETY: `proc_pidinfo` writes at most `size` bytes into the buffer and
    // reports how many it wrote. The struct is plain-old-data and is only
    // read after a full-size write is confirmed.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            FIND_ZOMBIE,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the call above filled the whole struct.
    let info = unsafe { info.assume_init() };
    // Microseconds since the epoch fit comfortably in a u64 (the seconds
    // component alone will not overflow it until year 584942), and combining
    // both fields gives sub-second resolution -- unlike `broker_lease`'s
    // whole-seconds `sysinfo` token, which is too coarse to tell apart two
    // processes started in the same second, a routine occurrence when a
    // recycled pid is handed straight back out.
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}
