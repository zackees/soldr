use std::sync::Arc;
use std::thread;
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyTimeoutError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use running_process::pty::{self as core_pty, NativePtyProcess as CoreNativePtyProcess, PtyError};

use crate::helpers::to_py_err;
use crate::idle_detector::NativeIdleDetector;

#[pyclass]
pub(crate) struct NativePtyProcess {
    pub(crate) inner: CoreNativePtyProcess,
}

impl NativePtyProcess {
    pub(crate) fn pty_err_to_py(err: PtyError) -> PyErr {
        match err {
            PtyError::Timeout => PyTimeoutError::new_err(err.to_string()),
            _ => PyRuntimeError::new_err(err.to_string()),
        }
    }

    pub(crate) fn start_terminal_input_relay_py(&self) -> PyResult<()> {
        self.inner
            .start_terminal_input_relay_impl()
            .map_err(Self::pty_err_to_py)
    }
}

#[pymethods]
impl NativePtyProcess {
    #[new]
    #[pyo3(signature = (argv, cwd=None, env=None, rows=24, cols=80, nice=None))]
    fn new(
        argv: Vec<String>,
        cwd: Option<String>,
        env: Option<Bound<'_, PyDict>>,
        rows: u16,
        cols: u16,
        nice: Option<i32>,
    ) -> PyResult<Self> {
        let env_pairs = env
            .map(|mapping| {
                mapping
                    .iter()
                    .map(|(key, value)| Ok((key.extract::<String>()?, value.extract::<String>()?)))
                    .collect::<PyResult<Vec<(String, String)>>>()
            })
            .transpose()?;
        let inner = CoreNativePtyProcess::new(argv, cwd, env_pairs, rows, cols, nice)
            .map_err(Self::pty_err_to_py)?;
        Ok(Self { inner })
    }

    #[inline(never)]
    pub(crate) fn start(&self) -> PyResult<()> {
        self.inner.start_impl().map_err(Self::pty_err_to_py)
    }

    #[pyo3(signature = (timeout=None))]
    pub(crate) fn read_chunk(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        let result = py.detach(|| self.inner.read_chunk_impl(timeout));
        match result {
            Ok(Some(chunk)) => Ok(PyBytes::new(py, &chunk).into_any().unbind()),
            Ok(None) => Err(PyTimeoutError::new_err(
                "No pseudo-terminal output available before timeout",
            )),
            Err(e) => Err(Self::pty_err_to_py(e)),
        }
    }

    #[pyo3(signature = (timeout=None))]
    pub(crate) fn wait_for_reader_closed(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<bool> {
        Ok(py.detach(|| self.inner.wait_for_reader_closed_impl(timeout)))
    }

    #[pyo3(signature = (data, submit=false))]
    pub(crate) fn write(&self, data: &[u8], submit: bool) -> PyResult<()> {
        self.inner
            .write_impl(data, submit)
            .map_err(Self::pty_err_to_py)
    }

    pub(crate) fn respond_to_queries(&self, data: &[u8]) -> PyResult<()> {
        self.inner
            .respond_to_queries_impl(data)
            .map_err(Self::pty_err_to_py)
    }

    #[inline(never)]
    pub(crate) fn resize(&self, rows: u16, cols: u16) -> PyResult<()> {
        self.inner
            .resize_impl(rows, cols)
            .map_err(Self::pty_err_to_py)
    }

    #[inline(never)]
    pub(crate) fn send_interrupt(&self) -> PyResult<()> {
        self.inner
            .send_interrupt_impl()
            .map_err(Self::pty_err_to_py)
    }

    pub(crate) fn poll(&self) -> PyResult<Option<i32>> {
        core_pty::poll_pty_process(&self.inner.handles, &self.inner.returncode).map_err(to_py_err)
    }

    #[pyo3(signature = (timeout=None))]
    #[inline(never)]
    pub(crate) fn wait(&self, timeout: Option<f64>) -> PyResult<i32> {
        self.inner.wait_impl(timeout).map_err(Self::pty_err_to_py)
    }

    #[inline(never)]
    pub(crate) fn terminate(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.terminate_impl().map_err(Self::pty_err_to_py))
    }

    #[inline(never)]
    pub(crate) fn kill(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.kill_impl().map_err(Self::pty_err_to_py))
    }

    #[inline(never)]
    pub(crate) fn terminate_tree(&self) -> PyResult<()> {
        self.inner
            .terminate_tree_impl()
            .map_err(Self::pty_err_to_py)
    }

    #[inline(never)]
    pub(crate) fn kill_tree(&self) -> PyResult<()> {
        self.inner.kill_tree_impl().map_err(Self::pty_err_to_py)
    }

    pub(crate) fn start_terminal_input_relay(&self) -> PyResult<()> {
        self.start_terminal_input_relay_py()
    }

    pub(crate) fn stop_terminal_input_relay(&self) {
        self.inner.stop_terminal_input_relay_impl();
    }

    pub(crate) fn terminal_input_relay_active(&self) -> bool {
        self.inner.terminal_input_relay_active()
    }

    pub(crate) fn pty_input_bytes_total(&self) -> usize {
        self.inner.pty_input_bytes_total()
    }

    pub(crate) fn pty_newline_events_total(&self) -> usize {
        self.inner.pty_newline_events_total()
    }

    pub(crate) fn pty_submit_events_total(&self) -> usize {
        self.inner.pty_submit_events_total()
    }

    fn pty_output_bytes_total(&self) -> usize {
        self.inner.pty_output_bytes_total()
    }

    fn pty_control_churn_bytes_total(&self) -> usize {
        self.inner.pty_control_churn_bytes_total()
    }

    #[pyo3(signature = (timeout=None, drain_timeout=2.0))]
    pub(crate) fn wait_and_drain(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
        drain_timeout: f64,
    ) -> PyResult<i32> {
        py.detach(|| {
            self.inner
                .wait_and_drain_impl(timeout, drain_timeout)
                .map_err(Self::pty_err_to_py)
        })
    }

    fn set_echo(&self, enabled: bool) {
        self.inner.set_echo(enabled);
    }

    fn echo_enabled(&self) -> bool {
        self.inner.echo_enabled()
    }

    fn attach_idle_detector(&self, detector: &NativeIdleDetector) {
        self.inner.attach_idle_detector(&detector.core);
    }

    fn detach_idle_detector(&self) {
        self.inner.detach_idle_detector();
    }

    #[pyo3(signature = (detector, timeout=None))]
    fn wait_for_idle(
        &self,
        py: Python<'_>,
        detector: &NativeIdleDetector,
        timeout: Option<f64>,
    ) -> PyResult<(bool, String, f64, Option<i32>)> {
        // Wire the detector into the reader thread.
        self.inner.attach_idle_detector(&detector.core);

        // Spawn exit watcher that marks the detector on process exit.
        let handles = Arc::clone(&self.inner.handles);
        let returncode = Arc::clone(&self.inner.returncode);
        let core = Arc::clone(&detector.core);
        let exit_watcher = thread::spawn(move || loop {
            match core_pty::poll_pty_process(&handles, &returncode) {
                Ok(Some(code)) => {
                    let interrupted = code == -2 || code == 130;
                    core.mark_exit(code, interrupted);
                    return;
                }
                Ok(None) => {}
                Err(_) => return,
            }
            thread::sleep(Duration::from_millis(1));
        });

        let result = py.detach(|| detector.core.wait(timeout));

        self.inner.detach_idle_detector();
        let _ = exit_watcher.join();
        Ok(result)
    }

    #[getter]
    pub(crate) fn pid(&self) -> PyResult<Option<u32>> {
        self.inner.pid().map_err(Self::pty_err_to_py)
    }

    pub(crate) fn close(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.close_impl().map_err(Self::pty_err_to_py))
    }
}
