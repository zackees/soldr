use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use running_process::{ContainedProcessGroup, DaemonChild, SpawnStdio, SpawnedChild};

use crate::helpers::to_py_err;

// ── ContainedProcessGroup Python wrapper ────────────────────────────────────

/// Python wrapper for `ContainedProcessGroup`.
///
/// In the v4 surface, `spawn()` and `spawn_daemon()` accept argv only;
/// stdio/pipe access is not yet exposed to Python (the underlying Rust
/// types use `BorrowedHandle` / `BorrowedFd` lifetimes that don't map
/// cleanly through PyO3 in this iteration). Both return the child PID;
/// the wrapper owns the child handles.
#[pyclass(name = "ContainedProcessGroup")]
pub(crate) struct PyContainedProcessGroup {
    pub(crate) inner: Option<ContainedProcessGroup>,
    pub(crate) wrapped_children: Vec<SpawnedChild>,
    pub(crate) daemon_children: Vec<DaemonChild>,
}

#[pymethods]
impl PyContainedProcessGroup {
    #[new]
    #[pyo3(signature = (originator=None))]
    fn new(originator: Option<String>) -> PyResult<Self> {
        let group = match originator {
            Some(ref orig) => ContainedProcessGroup::with_originator(orig).map_err(to_py_err)?,
            None => ContainedProcessGroup::new().map_err(to_py_err)?,
        };
        Ok(Self {
            inner: Some(group),
            wrapped_children: Vec::new(),
            daemon_children: Vec::new(),
        })
    }

    #[getter]
    fn originator(&self) -> Option<String> {
        self.inner.as_ref()?.originator().map(String::from)
    }

    #[getter]
    fn originator_value(&self) -> Option<String> {
        self.inner.as_ref()?.originator_value()
    }

    /// Spawn a contained child process (killed when group drops).
    fn spawn(&mut self, argv: Vec<String>) -> PyResult<u32> {
        let group = self
            .inner
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("group already closed"))?;
        if argv.is_empty() {
            return Err(PyValueError::new_err("argv must not be empty"));
        }
        let mut cmd = std::process::Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        let child = group
            .spawn(&mut cmd, SpawnStdio::default())
            .map_err(to_py_err)?;
        let pid = child.id();
        self.wrapped_children.push(child);
        Ok(pid)
    }

    /// Spawn a daemon child process (survives group drop, NUL stdio,
    /// sanitized handles).
    fn spawn_daemon(&mut self, argv: Vec<String>) -> PyResult<u32> {
        let group = self
            .inner
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("group already closed"))?;
        if argv.is_empty() {
            return Err(PyValueError::new_err("argv must not be empty"));
        }
        let mut cmd = std::process::Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }
        let child = group.spawn_daemon(&mut cmd).map_err(to_py_err)?;
        let pid = child.id();
        self.daemon_children.push(child);
        Ok(pid)
    }

    /// Close the group, killing all contained children. Daemon children
    /// are dropped but not terminated.
    fn close(&mut self) {
        self.inner.take();
        self.wrapped_children.clear();
        self.daemon_children.clear();
    }

    /// Context manager: __enter__ returns self.
    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    /// Context manager: __exit__ closes the group.
    #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
    fn __exit__(
        &mut self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) {
        self.close();
    }
}
