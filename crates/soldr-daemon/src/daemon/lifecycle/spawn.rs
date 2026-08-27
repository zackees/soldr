//! Launching the detached daemon process.
//!
//! `running-process` is the sole process-creation boundary. This module keeps
//! the Soldr-owned startup-log and stdio adapters used at that boundary; it
//! deliberately contains no daemon process launcher.

use std::path::Path;
use crate::core::SoldrPaths;

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
