use std::collections::HashMap;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use pyo3::exceptions::{PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyList;
use running_process::pty::terminal_input as core_terminal_input;
use running_process::{CommandSpec, ProcessError, StderrMode, StdinMode, StreamKind};
use sysinfo::{Pid, System};

pub(crate) fn to_py_err(err: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

pub(crate) fn process_err_to_py(err: ProcessError) -> PyErr {
    match err {
        ProcessError::Timeout => PyTimeoutError::new_err("process timed out"),
        other => to_py_err(other),
    }
}

pub(crate) fn system_pid(pid: u32) -> Pid {
    Pid::from_u32(pid)
}

pub(crate) fn descendant_pids(system: &System, pid: Pid) -> Vec<Pid> {
    // Build parent→children index in one pass.
    let mut children_map: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (child_pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children_map.entry(parent).or_default().push(*child_pid);
        }
    }
    // BFS from pid.
    let mut descendants = Vec::new();
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        if let Some(children) = children_map.get(&current) {
            for &child in children {
                descendants.push(child);
                stack.push(child);
            }
        }
    }
    descendants
}

// unix_now_seconds is now in running_process::pty::terminal_input
pub(crate) fn unix_now_seconds() -> f64 {
    core_terminal_input::unix_now_seconds()
}

#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static Mutex<()> {
    static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_ENV_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
pub(crate) fn with_locked_env_var<T>(
    key: &'static str,
    value: Option<&str>,
    f: impl FnOnce() -> T + std::panic::UnwindSafe,
) -> T {
    let _guard = test_env_lock().lock().unwrap();
    let previous = std::env::var_os(key);
    match value {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }

    let result = std::panic::catch_unwind(f);

    match previous {
        Some(previous) => std::env::set_var(key, previous),
        None => std::env::remove_var(key),
    }

    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

pub(crate) fn parse_command(command: &Bound<'_, PyAny>, shell: bool) -> PyResult<CommandSpec> {
    if let Ok(command) = command.extract::<String>() {
        if !shell {
            return Err(PyValueError::new_err(
                "String commands require shell=True. Use shell=True or provide command as list[str].",
            ));
        }
        return Ok(CommandSpec::Shell(command));
    }

    if let Ok(command) = command.cast::<PyList>() {
        let argv = command.extract::<Vec<String>>()?;
        if argv.is_empty() {
            return Err(PyValueError::new_err("command cannot be empty"));
        }
        if shell {
            return Ok(CommandSpec::Shell(argv.join(" ")));
        }
        return Ok(CommandSpec::Argv(argv));
    }

    Err(PyValueError::new_err(
        "command must be either a string or a list[str]",
    ))
}

pub(crate) fn stream_kind(name: &str) -> PyResult<StreamKind> {
    match name {
        "stdout" => Ok(StreamKind::Stdout),
        "stderr" => Ok(StreamKind::Stderr),
        _ => Err(PyValueError::new_err("stream must be 'stdout' or 'stderr'")),
    }
}

pub(crate) fn stdin_mode(name: &str) -> PyResult<StdinMode> {
    match name {
        "inherit" => Ok(StdinMode::Inherit),
        "piped" => Ok(StdinMode::Piped),
        "null" => Ok(StdinMode::Null),
        _ => Err(PyValueError::new_err(
            "stdin_mode must be 'inherit', 'piped', or 'null'",
        )),
    }
}

pub(crate) fn stderr_mode(name: &str) -> PyResult<StderrMode> {
    match name {
        "stdout" => Ok(StderrMode::Stdout),
        "pipe" => Ok(StderrMode::Pipe),
        _ => Err(PyValueError::new_err(
            "stderr_mode must be 'stdout' or 'pipe'",
        )),
    }
}
