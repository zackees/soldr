use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use pyo3::prelude::*;
use sysinfo::{ProcessRefreshKind, System};

use crate::daemon_client;
use crate::helpers::system_pid;

const PID_DB_ENV: &str = "RUNNING_PROCESS_PID_DB";

#[derive(Clone)]
pub(crate) struct ActiveProcessRecord {
    pub(crate) pid: u32,
    pub(crate) kind: String,
    pub(crate) command: String,
    pub(crate) cwd: Option<String>,
    pub(crate) started_at: f64,
}

pub(crate) type TrackedProcessEntry = (u32, f64, String, String, Option<String>);
pub(crate) type ActiveProcessEntry = (u32, String, String, Option<String>, f64);
pub(crate) type DetachedLaunchEntry = (u32, f64, String, Option<String>, Option<String>, String);
pub(crate) type ExpectDetails = (String, usize, usize, Vec<String>);
pub(crate) type ExpectResult = (
    String,
    String,
    Option<String>,
    Option<usize>,
    Option<usize>,
    Vec<String>,
);

pub(crate) fn active_process_registry() -> &'static Mutex<HashMap<u32, ActiveProcessRecord>> {
    static ACTIVE_PROCESSES: OnceLock<Mutex<HashMap<u32, ActiveProcessRecord>>> = OnceLock::new();
    ACTIVE_PROCESSES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn register_active_process(
    pid: u32,
    kind: &str,
    command: &str,
    cwd: Option<String>,
    started_at: f64,
) {
    let mut registry = active_process_registry()
        .lock()
        .expect("active process registry mutex poisoned");
    registry.insert(
        pid,
        ActiveProcessRecord {
            pid,
            kind: kind.to_string(),
            command: command.to_string(),
            cwd: cwd.clone(),
            started_at,
        },
    );
    drop(registry); // release lock before IPC

    // Fire-and-forget daemon notification.
    daemon_client::daemon_register(pid, started_at, kind, command, cwd.as_deref());
}

pub(crate) fn unregister_active_process(pid: u32) {
    let mut registry = active_process_registry()
        .lock()
        .expect("active process registry mutex poisoned");
    registry.remove(&pid);
    drop(registry); // release lock before IPC

    // Fire-and-forget daemon notification.
    daemon_client::daemon_unregister(pid);
}

pub(crate) fn process_created_at(pid: u32) -> Option<f64> {
    let pid = system_pid(pid);
    let mut system = System::new();
    system.refresh_process_specifics(pid, ProcessRefreshKind::new());
    system
        .process(pid)
        .map(|process| process.start_time() as f64)
}

pub(crate) fn same_process_identity(pid: u32, created_at: f64, tolerance_seconds: f64) -> bool {
    let Some(actual) = process_created_at(pid) else {
        return false;
    };
    (actual - created_at).abs() <= tolerance_seconds
}

pub(crate) fn tracked_process_db_path() -> PyResult<PathBuf> {
    if let Ok(value) = std::env::var(PID_DB_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }

    #[cfg(windows)]
    let base_dir = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    #[cfg(not(windows))]
    let base_dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let mut path = PathBuf::from(home);
                path.push(".local");
                path.push("state");
                path
            })
        })
        .unwrap_or_else(std::env::temp_dir);

    Ok(base_dir
        .join("running-process")
        .join("tracked-pids.sqlite3"))
}
