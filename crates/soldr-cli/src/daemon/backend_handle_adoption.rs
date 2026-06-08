//! soldr-daemon probe boundary prepared for `running-process` `BackendHandle`.
//!
//! The current crates.io `running-process` release checked for this work is
//! 4.0.3, which does not expose
//! `running_process::broker::backend_handle::BackendHandle`. This module keeps
//! soldr's existing PID-file probe behavior in one place and records the exact
//! dependency gate for the future migration.
//!
//! Once a published `running-process` release contains
//! zackees/running-process#232, `probe_soldr_daemon` is the call site to replace
//! with `BackendHandle::probe_with_service`. soldr's IPC
//! [`crate::daemon::protocol::PROTOCOL_VERSION`] check remains the wire-version
//! guard after a handle opens a connection.

use crate::cache_lib::daemon_pid_path;
use crate::core::SoldrPaths;
use crate::daemon::client;
use crate::daemon::lifecycle::{pid_exe_stem_matches, pid_is_alive, read_pid_file};
use crate::daemon::protocol::PROTOCOL_VERSION;
use std::path::{Path, PathBuf};

pub(crate) const SOLDR_DAEMON_SERVICE_NAME: &str = "soldr-daemon";
pub(crate) const SOLDR_DAEMON_SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) const RUNNING_PROCESS_BACKEND_HANDLE_GATE: RunningProcessBackendHandleGate =
    RunningProcessBackendHandleGate {
        crate_name: "running-process",
        checked_published_version: "4.0.3",
        required_symbol: "running_process::broker::backend_handle::BackendHandle",
        running_process_issue: "zackees/running-process#232",
        soldr_issue: "zackees/soldr#718",
        remaining_gate:
            "publish a running-process release that exposes BackendHandle, then replace this \
             PID-file adapter with BackendHandle::probe_with_service",
    };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunningProcessBackendHandleGate {
    pub(crate) crate_name: &'static str,
    pub(crate) checked_published_version: &'static str,
    pub(crate) required_symbol: &'static str,
    pub(crate) running_process_issue: &'static str,
    pub(crate) soldr_issue: &'static str,
    pub(crate) remaining_gate: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SoldrDaemonBackendHandle {
    pub(crate) service_name: &'static str,
    pub(crate) service_version: &'static str,
    pub(crate) protocol_version: u32,
    pub(crate) pid: u32,
    pub(crate) exe_path: PathBuf,
    pub(crate) endpoint: PathBuf,
    pub(crate) pid_file: PathBuf,
    pub(crate) adoption_gate: RunningProcessBackendHandleGate,
}

impl SoldrDaemonBackendHandle {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn is_alive(&self) -> bool {
        pid_is_alive(self.pid) && pid_exe_stem_matches(self.pid, SOLDR_DAEMON_SERVICE_NAME)
    }
}

pub(crate) fn probe_soldr_daemon(paths: &SoldrPaths) -> Option<SoldrDaemonBackendHandle> {
    probe_daemon_with_expected_stem(paths, SOLDR_DAEMON_SERVICE_NAME)
}

fn probe_daemon_with_expected_stem(
    paths: &SoldrPaths,
    expected_stem: &str,
) -> Option<SoldrDaemonBackendHandle> {
    let (pid, exe_path) = read_pid_file(paths)?;
    if !(pid_is_alive(pid) && pid_exe_stem_matches(pid, expected_stem)) {
        return None;
    }
    Some(SoldrDaemonBackendHandle {
        service_name: SOLDR_DAEMON_SERVICE_NAME,
        service_version: SOLDR_DAEMON_SERVICE_VERSION,
        protocol_version: PROTOCOL_VERSION,
        pid,
        exe_path,
        endpoint: client::default_sock_path(paths),
        pid_file: daemon_pid_path(paths),
        adoption_gate: RUNNING_PROCESS_BACKEND_HANDLE_GATE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_lib::soldr_daemon_dir;
    use tempfile::TempDir;

    fn write_pid_file(paths: &SoldrPaths, pid: u32, exe_path: &Path) {
        std::fs::create_dir_all(soldr_daemon_dir(paths)).expect("daemon dir");
        std::fs::write(
            daemon_pid_path(paths),
            format!("{pid}\n{}\n", exe_path.display()),
        )
        .expect("write pid file");
    }

    #[test]
    fn dependency_gate_documents_published_backend_handle_blocker() {
        let gate = RUNNING_PROCESS_BACKEND_HANDLE_GATE;
        assert_eq!(gate.crate_name, "running-process");
        assert_eq!(gate.checked_published_version, "4.0.3");
        assert_eq!(
            gate.required_symbol,
            "running_process::broker::backend_handle::BackendHandle"
        );
        assert_eq!(gate.running_process_issue, "zackees/running-process#232");
        assert_eq!(gate.soldr_issue, "zackees/soldr#718");
        assert!(gate
            .remaining_gate
            .contains("publish a running-process release"));
    }

    #[test]
    fn probe_missing_pid_file_reports_no_handle() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        assert!(probe_soldr_daemon(&paths).is_none());
    }

    #[test]
    fn probe_stale_pid_file_reports_no_handle() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        write_pid_file(&paths, u32::MAX, Path::new("soldr-daemon"));

        assert!(probe_soldr_daemon(&paths).is_none());
    }

    #[test]
    fn probe_current_process_records_backend_handle_shape() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let current_exe = std::env::current_exe().expect("current exe");
        let current_stem = current_exe.file_stem().and_then(|s| s.to_str()).unwrap();
        write_pid_file(&paths, std::process::id(), &current_exe);

        let handle = probe_daemon_with_expected_stem(&paths, current_stem)
            .expect("current test process should probe");

        assert_eq!(handle.service_name, SOLDR_DAEMON_SERVICE_NAME);
        assert_eq!(handle.service_version, SOLDR_DAEMON_SERVICE_VERSION);
        assert_eq!(handle.protocol_version, PROTOCOL_VERSION);
        assert_eq!(handle.pid(), std::process::id());
        assert_eq!(handle.exe_path, current_exe);
        assert_eq!(handle.endpoint, client::default_sock_path(&paths));
        assert_eq!(handle.pid_file, daemon_pid_path(&paths));
        assert_eq!(handle.adoption_gate, RUNNING_PROCESS_BACKEND_HANDLE_GATE);
    }
}
