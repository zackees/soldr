use std::thread;
use std::time::{Duration, Instant};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use sysinfo::{Signal, System};

use crate::helpers::{descendant_pids, system_pid, to_py_err};
use crate::registry::{process_created_at, same_process_identity, DetachedLaunchEntry};

pub(crate) fn kill_process_tree_impl(pid: u32, timeout_seconds: f64) {
    let mut system = System::new();
    system.refresh_processes();
    let pid = system_pid(pid);
    let Some(_) = system.process(pid) else {
        return;
    };

    let mut kill_order = descendant_pids(&system, pid);
    kill_order.reverse();
    kill_order.push(pid);

    for target in &kill_order {
        if let Some(process) = system.process(*target) {
            if !process.kill_with(Signal::Kill).unwrap_or(false) {
                process.kill();
            }
        }
    }

    let deadline = Instant::now()
        .checked_add(Duration::from_secs_f64(timeout_seconds.max(0.0)))
        .unwrap_or_else(Instant::now);
    loop {
        system.refresh_processes();
        if kill_order
            .iter()
            .all(|target| system.process(*target).is_none())
        {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[pyfunction]
pub(crate) fn native_get_process_tree_info(pid: u32) -> String {
    let mut system = System::new();
    system.refresh_processes();
    let pid = system_pid(pid);
    let Some(process) = system.process(pid) else {
        return format!("Could not get process info for PID {}", pid.as_u32());
    };

    let mut info = vec![
        format!("Process {} ({})", pid.as_u32(), process.name()),
        format!("Status: {:?}", process.status()),
    ];
    let children = descendant_pids(&system, pid);
    if !children.is_empty() {
        info.push("Child processes:".to_string());
        for child_pid in children {
            if let Some(child) = system.process(child_pid) {
                info.push(format!("  Child {} ({})", child_pid.as_u32(), child.name()));
            }
        }
    }
    info.join("\n")
}

#[pyfunction]
#[pyo3(signature = (pid, timeout_seconds=3.0))]
pub(crate) fn native_kill_process_tree(pid: u32, timeout_seconds: f64) {
    kill_process_tree_impl(pid, timeout_seconds);
}

#[pyfunction]
pub(crate) fn native_process_created_at(pid: u32) -> Option<f64> {
    process_created_at(pid)
}

#[pyfunction]
#[pyo3(signature = (pid, created_at, tolerance_seconds=1.0))]
pub(crate) fn native_is_same_process(pid: u32, created_at: f64, tolerance_seconds: f64) -> bool {
    same_process_identity(pid, created_at, tolerance_seconds)
}

#[pyfunction]
#[pyo3(signature = (command, cwd=None, env=None, originator=None))]
pub(crate) fn native_launch_detached(
    py: Python<'_>,
    command: String,
    cwd: Option<String>,
    env: Option<Bound<'_, PyDict>>,
    originator: Option<String>,
) -> PyResult<DetachedLaunchEntry> {
    let command = command.trim().to_string();
    if command.is_empty() {
        return Err(PyValueError::new_err("command must not be empty"));
    }

    let env_pairs = env
        .map(|mapping| {
            mapping
                .iter()
                .map(|(key, value)| Ok((key.extract::<String>()?, value.extract::<String>()?)))
                .collect::<PyResult<Vec<(String, String)>>>()
        })
        .transpose()?
        .unwrap_or_default();

    let spawned = py
        .detach(move || {
            let mut request = running_process::client::SpawnCommandRequest::shell(command);
            if let Some(cwd) = cwd {
                request = request.with_cwd(cwd);
            }
            for (key, value) in env_pairs {
                request = request.with_env(key, value);
            }
            if let Some(originator) = originator {
                request = request.with_originator(originator);
            }

            let mut client = running_process::client::connect_or_start(None)?;
            client.spawn_command(&request)
        })
        .map_err(to_py_err)?;

    Ok((
        spawned.pid,
        spawned.created_at,
        spawned.command,
        spawned.cwd,
        spawned.originator,
        spawned.containment,
    ))
}
