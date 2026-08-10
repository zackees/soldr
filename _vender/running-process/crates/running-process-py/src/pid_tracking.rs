use pyo3::prelude::*;

use crate::helpers::unix_now_seconds;
use crate::process_tree::kill_process_tree_impl;
use crate::registry::{
    active_process_registry, process_created_at, register_active_process, same_process_identity,
    tracked_process_db_path, unregister_active_process, ActiveProcessEntry, ActiveProcessRecord,
    TrackedProcessEntry,
};

#[pyfunction]
pub(crate) fn tracked_pid_db_path_py() -> PyResult<String> {
    Ok(tracked_process_db_path()?.to_string_lossy().into_owned())
}

#[pyfunction]
#[pyo3(signature = (pid, created_at, kind, command, cwd=None))]
pub(crate) fn track_process_pid(
    pid: u32,
    created_at: f64,
    kind: &str,
    command: &str,
    cwd: Option<String>,
) -> PyResult<()> {
    let _ = created_at;
    register_active_process(pid, kind, command, cwd, unix_now_seconds());
    Ok(())
}

#[pyfunction]
#[pyo3(signature = (pid, kind, command, cwd=None))]
pub(crate) fn native_register_process(
    pid: u32,
    kind: &str,
    command: &str,
    cwd: Option<String>,
) -> PyResult<()> {
    register_active_process(pid, kind, command, cwd, unix_now_seconds());
    Ok(())
}

#[pyfunction]
pub(crate) fn untrack_process_pid(pid: u32) -> PyResult<()> {
    unregister_active_process(pid);
    Ok(())
}

#[pyfunction]
pub(crate) fn native_unregister_process(pid: u32) -> PyResult<()> {
    unregister_active_process(pid);
    Ok(())
}

#[pyfunction]
pub(crate) fn list_tracked_processes() -> PyResult<Vec<TrackedProcessEntry>> {
    let snapshot: Vec<ActiveProcessRecord> = {
        let registry = active_process_registry()
            .lock()
            .expect("active process registry mutex poisoned");
        registry.values().cloned().collect()
    };
    let mut entries: Vec<_> = snapshot
        .into_iter()
        .map(|entry| {
            (
                entry.pid,
                process_created_at(entry.pid).unwrap_or(entry.started_at),
                entry.kind,
                entry.command,
                entry.cwd,
            )
        })
        .collect();
    entries.sort_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    Ok(entries)
}

#[pyfunction]
#[pyo3(signature = (tolerance_seconds=1.0, kill_timeout_seconds=3.0))]
pub(crate) fn native_cleanup_tracked_processes(
    tolerance_seconds: f64,
    kill_timeout_seconds: f64,
) -> PyResult<Vec<TrackedProcessEntry>> {
    let entries = list_tracked_processes()?;

    let mut killed = Vec::new();
    for entry in entries {
        let pid = entry.0;
        if !same_process_identity(pid, entry.1, tolerance_seconds) {
            unregister_active_process(pid);
            continue;
        }
        kill_process_tree_impl(pid, kill_timeout_seconds);
        unregister_active_process(pid);
        killed.push(entry);
    }
    Ok(killed)
}

#[pyfunction]
pub(crate) fn native_list_active_processes() -> Vec<ActiveProcessEntry> {
    let registry = active_process_registry()
        .lock()
        .expect("active process registry mutex poisoned");
    let mut items: Vec<_> = registry
        .values()
        .map(|entry| {
            (
                entry.pid,
                entry.kind.clone(),
                entry.command.clone(),
                entry.cwd.clone(),
                entry.started_at,
            )
        })
        .collect();
    items.sort_by(|left, right| {
        left.4
            .partial_cmp(&right.4)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    items
}
