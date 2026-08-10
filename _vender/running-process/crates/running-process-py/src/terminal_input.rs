use pyo3::exceptions::{PyRuntimeError, PyTimeoutError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use running_process::pty::terminal_input::{
    TerminalInputCore, TerminalInputError, TerminalInputEventRecord,
};

use crate::helpers::to_py_err;

#[pyclass(skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct NativeTerminalInputEvent {
    pub(crate) data: Vec<u8>,
    pub(crate) submit: bool,
    pub(crate) shift: bool,
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) virtual_key_code: u16,
    pub(crate) repeat_count: u16,
}

#[pyclass]
pub(crate) struct NativeTerminalInput {
    pub(crate) inner: TerminalInputCore,
}

impl NativeTerminalInput {
    fn event_to_py(
        py: Python<'_>,
        event: TerminalInputEventRecord,
    ) -> PyResult<Py<NativeTerminalInputEvent>> {
        Py::new(
            py,
            NativeTerminalInputEvent {
                data: event.data,
                submit: event.submit,
                shift: event.shift,
                ctrl: event.ctrl,
                alt: event.alt,
                virtual_key_code: event.virtual_key_code,
                repeat_count: event.repeat_count,
            },
        )
    }

    fn wait_for_event(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<TerminalInputEventRecord> {
        py.detach(|| {
            self.inner.wait_for_event(timeout).map_err(|err| match err {
                TerminalInputError::Closed => {
                    PyRuntimeError::new_err("Native terminal input is closed")
                }
                TerminalInputError::Timeout => {
                    PyTimeoutError::new_err("No terminal input available before timeout")
                }
                other => to_py_err(other),
            })
        })
    }
}

#[pymethods]
impl NativeTerminalInputEvent {
    #[getter]
    fn data(&self, py: Python<'_>) -> Py<PyAny> {
        PyBytes::new(py, &self.data).into_any().unbind()
    }

    #[getter]
    fn submit(&self) -> bool {
        self.submit
    }

    #[getter]
    fn shift(&self) -> bool {
        self.shift
    }

    #[getter]
    fn ctrl(&self) -> bool {
        self.ctrl
    }

    #[getter]
    fn alt(&self) -> bool {
        self.alt
    }

    #[getter]
    fn virtual_key_code(&self) -> u16 {
        self.virtual_key_code
    }

    #[getter]
    fn repeat_count(&self) -> u16 {
        self.repeat_count
    }

    pub(crate) fn __repr__(&self) -> String {
        format!(
            "NativeTerminalInputEvent(data={:?}, submit={}, shift={}, ctrl={}, alt={}, virtual_key_code={}, repeat_count={})",
            self.data,
            self.submit,
            self.shift,
            self.ctrl,
            self.alt,
            self.virtual_key_code,
            self.repeat_count,
        )
    }
}

#[pymethods]
impl NativeTerminalInput {
    #[new]
    pub(crate) fn new() -> Self {
        Self {
            inner: TerminalInputCore::new(),
        }
    }

    pub(crate) fn start(&self) -> PyResult<()> {
        #[cfg(windows)]
        {
            self.inner.start_impl().map_err(to_py_err)
        }

        #[cfg(not(windows))]
        {
            Err(PyRuntimeError::new_err(
                "NativeTerminalInput is only available on Windows consoles",
            ))
        }
    }

    fn stop(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.stop_impl().map_err(to_py_err))
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.stop_impl().map_err(to_py_err))
    }

    pub(crate) fn available(&self) -> bool {
        self.inner.available()
    }

    #[getter]
    pub(crate) fn capturing(&self) -> bool {
        self.inner.capturing()
    }

    #[getter]
    fn original_console_mode(&self) -> Option<u32> {
        self.inner.original_console_mode()
    }

    #[getter]
    fn active_console_mode(&self) -> Option<u32> {
        self.inner.active_console_mode()
    }

    #[pyo3(signature = (timeout=None))]
    fn read_event(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<Py<NativeTerminalInputEvent>> {
        let event = self.wait_for_event(py, timeout)?;
        Self::event_to_py(py, event)
    }

    fn read_event_non_blocking(
        &self,
        py: Python<'_>,
    ) -> PyResult<Option<Py<NativeTerminalInputEvent>>> {
        if let Some(event) = self.inner.next_event() {
            return Self::event_to_py(py, event).map(Some);
        }
        if self
            .inner
            .state
            .lock()
            .expect("terminal input mutex poisoned")
            .closed
        {
            return Err(PyRuntimeError::new_err("Native terminal input is closed"));
        }
        Ok(None)
    }

    #[pyo3(signature = (timeout=None))]
    fn read(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        let event = self.wait_for_event(py, timeout)?;
        Ok(PyBytes::new(py, &event.data).into_any().unbind())
    }

    fn read_non_blocking(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        if let Some(event) = self.inner.next_event() {
            return Ok(Some(PyBytes::new(py, &event.data).into_any().unbind()));
        }
        if self
            .inner
            .state
            .lock()
            .expect("terminal input mutex poisoned")
            .closed
        {
            return Err(PyRuntimeError::new_err("Native terminal input is closed"));
        }
        Ok(None)
    }

    fn drain(&self, py: Python<'_>) -> Vec<Py<PyAny>> {
        self.inner
            .drain_events()
            .into_iter()
            .map(|event| PyBytes::new(py, &event.data).into_any().unbind())
            .collect()
    }

    fn drain_events(&self, py: Python<'_>) -> PyResult<Vec<Py<NativeTerminalInputEvent>>> {
        self.inner
            .drain_events()
            .into_iter()
            .map(|event| Self::event_to_py(py, event))
            .collect()
    }

    /// Wait for at least one input event, then drain all queued events and
    /// return their data merged into a single `bytes` object plus a `submit`
    /// flag.  This avoids per-event Python round-trips during large pastes.
    ///
    /// Returns ``(data: bytes, submit: bool)``.
    #[pyo3(signature = (timeout=None))]
    fn read_batch(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<(Py<PyAny>, bool)> {
        // Block (releasing the GIL) until the first event arrives.
        let first = self.wait_for_event(py, timeout)?;

        // Drain everything else already queued.
        let remaining = self.inner.drain_events();

        // Merge all data into one buffer.
        let capacity = first.data.len() + remaining.iter().map(|e| e.data.len()).sum::<usize>();
        let mut merged = Vec::with_capacity(capacity);
        let mut submit = first.submit;
        merged.extend_from_slice(&first.data);
        for event in &remaining {
            merged.extend_from_slice(&event.data);
            submit = submit || event.submit;
        }

        Ok((PyBytes::new(py, &merged).into_any().unbind(), submit))
    }
}

// Drop is now handled by TerminalInputCore's Drop impl
