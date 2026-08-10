use pyo3::prelude::*;

use running_process::{find_processes_by_originator, OriginatorProcessInfo};

// ── Originator process scanning ─────────────────────────────────────────────

#[pyclass(name = "OriginatorProcessInfo", skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct PyOriginatorProcessInfo {
    #[pyo3(get)]
    pid: u32,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    command: String,
    #[pyo3(get)]
    originator: String,
    #[pyo3(get)]
    parent_pid: u32,
    #[pyo3(get)]
    parent_alive: bool,
}

#[pymethods]
impl PyOriginatorProcessInfo {
    fn __repr__(&self) -> String {
        format!(
            "OriginatorProcessInfo(pid={}, name={:?}, originator={:?}, parent_pid={}, parent_alive={})",
            self.pid, self.name, self.originator, self.parent_pid, self.parent_alive
        )
    }
}

impl From<OriginatorProcessInfo> for PyOriginatorProcessInfo {
    fn from(info: OriginatorProcessInfo) -> Self {
        Self {
            pid: info.pid,
            name: info.name,
            command: info.command,
            originator: info.originator,
            parent_pid: info.parent_pid,
            parent_alive: info.parent_alive,
        }
    }
}

/// Find all processes whose RUNNING_PROCESS_ORIGINATOR env var starts
/// with the given tool prefix.
#[pyfunction]
pub(crate) fn py_find_processes_by_originator(tool: &str) -> Vec<PyOriginatorProcessInfo> {
    find_processes_by_originator(tool)
        .into_iter()
        .map(PyOriginatorProcessInfo::from)
        .collect()
}
