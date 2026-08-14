//! Linux process termination.

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
    // SAFETY: kill(2) on a pid the caller captured. ESRCH — the process
    // exited between the probe and the signal — is success for a
    // terminate request.
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn signal_an_unallocated_pid_is_success_not_error() {
        // PID 999999 is effectively unallocated; ESRCH must map to Ok.
        signal_pid(999_999, false).expect("esrch is success");
    }

    #[test]
    fn terminate_tree_reports_esrch_group_as_killed() {
        // A child that already exited: the group is gone, so killpg fails
        // with ESRCH which signal_process_group treats as success.
        let mut child = Command::new("/bin/true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn true");
        child.wait().expect("reap true");
        assert_eq!(
            terminate_tree(&mut child).expect("tree kill"),
            TreeKill::TreeKilled
        );
    }
}
