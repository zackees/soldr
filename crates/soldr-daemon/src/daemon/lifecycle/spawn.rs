//! Launching the detached daemon process.
//!
//! `running-process` is the sole process-creation boundary. This module owns
//! only Soldr's command arguments, environment overlay, and startup-log path.

use std::path::Path;
use std::process::Command;

use crate::core::SoldrPaths;

use super::spawn_env::daemon_spawn_env;

pub(crate) fn spawn_detached_inner(daemon: &Path, args: &[String]) -> Result<(), std::io::Error> {
    spawn_detached(daemon, false, args)
}

/// Spawn the daemon via `<current-soldr-exe> daemon start --foreground`
/// rather than via the sibling `soldr-daemon` binary.
pub(crate) fn spawn_detached_self_inner(
    soldr_self: &Path,
    args: &[String],
) -> Result<(), std::io::Error> {
    spawn_detached(soldr_self, true, args)
}

fn spawn_detached(program: &Path, via_self: bool, args: &[String]) -> Result<(), std::io::Error> {
    let mut command = Command::new(program);
    command.envs(daemon_spawn_env());
    // arg0 replacement is a Unix primitive; on Windows the image decides
    // the identity and the platform facade is a no-op.
    if via_self {
        force_daemon_via_self_cli_identity(&mut command);
    }
    command.args(args);

    let log_file = open_spawn_log();
    let stdio = daemon_stdio(log_file.as_ref());
    running_process::spawn_daemon_with_stdio_and_env_policy(
        &mut command,
        stdio,
        running_process::EnvironmentPolicy::UserBaseline,
    )?;
    Ok(())
}

pub(crate) fn force_daemon_via_self_cli_identity(command: &mut Command) {
    crate::platform::process::command::arg0(command, "soldr");
}

/// Open `daemon-spawn.log` for append.
///
/// `running-process` duplicates this file into the detached child's sanitized
/// handle list. Failure degrades to null stdio and never blocks daemon start.
pub(crate) fn open_spawn_log() -> Option<std::fs::File> {
    open_spawn_log_at(&SoldrPaths::new().ok()?.root.join("daemon-spawn.log"))
}

pub(crate) fn open_spawn_log_at(path: &Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

pub(crate) fn daemon_stdio(log: Option<&std::fs::File>) -> running_process::DaemonStdio<'_> {
    crate::platform::process::spawn::daemon_stdio(log)
}
