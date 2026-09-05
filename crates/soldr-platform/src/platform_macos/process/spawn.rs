//! macOS detached spawn and exec mechanics.

use std::io;
use std::process::{Child, Command, ExitStatus};

/// Spawn `command` in its own process group so a tree kill can target the
/// whole group. (Full session detachment for the daemon is handled by
/// `running-process`, which owns process creation.)
pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    // soldr#3098: spawns share, staged writes exclude.
    let _spawn = crate::platform::process::spawn_exclusion::spawn_shared();
    command.spawn()
}

/// Map an optional log file onto the running-process daemon stdio type.
pub fn daemon_stdio(log: Option<&std::fs::File>) -> running_process::DaemonStdio<'_> {
    let Some(log) = log else {
        return running_process::DaemonStdio::default();
    };
    running_process::DaemonStdio {
        stdout: running_process::DaemonStdioSource::File(log),
        stderr: running_process::DaemonStdioSource::File(log),
    }
}

/// Replace the current process image with `command` (exec). On success this
/// never returns; on failure it returns the exec error.
pub fn exec_or_status(command: &mut Command) -> io::Result<ExitStatus> {
    use std::os::unix::process::CommandExt;
    Err(command.exec())
}
