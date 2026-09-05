//! macOS process termination.

use std::io;
use std::process::Child;
use std::time::Duration;

/// How a tree kill was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeKill {
    /// The whole tree was killed (`taskkill /T /F` or the signal group).
    TreeKilled,
    /// The tree kill failed; the direct child was killed instead.
    ProcessKilled,
}

/// Kill `child`'s whole process group (SIGTERM, then SIGKILL after a short
/// grace), falling back to killing the child itself.
pub fn terminate_tree(child: &mut Child) -> io::Result<TreeKill> {
    let pgid = child.id() as libc::pid_t;
    let term_result = signal_process_group(pgid, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(100));
    let kill_result = signal_process_group(pgid, libc::SIGKILL);
    if term_result.is_ok() || kill_result.is_ok() {
        return Ok(TreeKill::TreeKilled);
    }
    child.kill()?;
    Ok(TreeKill::ProcessKilled)
}

/// Terminate a single PID with SIGTERM.
pub fn terminate_pid(pid: u32) {
    let _ = signal_pid(pid, false);
}

/// Signal a single PID: SIGTERM, or SIGKILL when `force` is set. A pid that
/// is already gone (ESRCH) counts as success.
pub fn signal_pid(pid: u32, force: bool) -> io::Result<()> {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    // Refuse anything `pid_t` cannot represent as a single process, BEFORE
    // the syscall. `pid_t` is signed, so a `u32` above `i32::MAX` arrives at
    // `kill` as a negative number, and negative arguments do not mean "that
    // process": -N signals process group N, and -1 signals every process the
    // caller may signal. `signal_pid(4294967295, true)` would therefore
    // SIGKILL the user's entire session rather than one stale pid. Real
    // Linux and macOS pids never come near that range, but these values
    // arrive from pid files and environment variables, not only from
    // `Child::id()`.
    let target = libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0);
    let Some(target) = target else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to signal pid {pid}: not a single-process id"),
        ));
    };
    // SAFETY: kill(2) on a pid the caller captured. ESRCH — the process
    // exited between the probe and the signal — is success for a
    // terminate request.
    let rc = unsafe { libc::kill(target, signal) };
    if rc == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}

fn signal_process_group(pgid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: `pgid` is the spawned child's PID after `process_group(0)`.
    // Negating it asks the kernel to signal every member of the group.
    let rc = unsafe { libc::kill(-pgid, signal) };
    if rc == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}
