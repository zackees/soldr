use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use running_process::pty::NativePtyProcess as CoreNativePtyProcess;

use crate::process::NativeRunningProcess;
use crate::pty_process::NativePtyProcess;
use crate::registry::ExpectResult;

pub(crate) enum NativeProcessBackend {
    Running(NativeRunningProcess),
    Pty(NativePtyProcess),
}

#[pyclass(name = "NativeProcess")]
pub(crate) struct PyNativeProcess {
    pub(crate) backend: NativeProcessBackend,
}

#[pymethods]
impl PyNativeProcess {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (command, cwd=None, shell=false, capture=true, env=None, creationflags=None, text=true, encoding=None, errors=None, stdin_mode_name="inherit", stderr_mode_name="stdout", nice=None, create_process_group=false))]
    fn new(
        command: &Bound<'_, PyAny>,
        cwd: Option<String>,
        shell: bool,
        capture: bool,
        env: Option<Bound<'_, PyDict>>,
        creationflags: Option<u32>,
        text: bool,
        encoding: Option<String>,
        errors: Option<String>,
        stdin_mode_name: &str,
        stderr_mode_name: &str,
        nice: Option<i32>,
        create_process_group: bool,
    ) -> PyResult<Self> {
        Ok(Self {
            backend: NativeProcessBackend::Running(NativeRunningProcess::new(
                command,
                cwd,
                shell,
                capture,
                env,
                creationflags,
                text,
                encoding,
                errors,
                stdin_mode_name,
                stderr_mode_name,
                nice,
                create_process_group,
            )?),
        })
    }

    #[staticmethod]
    #[pyo3(signature = (argv, cwd=None, env=None, rows=24, cols=80, nice=None))]
    fn for_pty(
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
            .map_err(NativePtyProcess::pty_err_to_py)?;
        Ok(Self {
            backend: NativeProcessBackend::Pty(NativePtyProcess { inner }),
        })
    }

    fn start(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.start(),
            NativeProcessBackend::Pty(process) => process.start(),
        }
    }

    fn poll(&self) -> PyResult<Option<i32>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.poll(),
            NativeProcessBackend::Pty(process) => process.poll(),
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn wait(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<i32> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.wait(py, timeout),
            NativeProcessBackend::Pty(process) => py.detach(|| process.wait(timeout)),
        }
    }

    fn kill(&self, py: Python<'_>) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.kill(),
            NativeProcessBackend::Pty(process) => process.kill(py),
        }
    }

    fn terminate(&self, py: Python<'_>) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.terminate(),
            NativeProcessBackend::Pty(process) => process.terminate(py),
        }
    }

    fn terminate_group(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.terminate_group(),
            NativeProcessBackend::Pty(process) => process.terminate_tree(),
        }
    }

    fn kill_group(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.kill_group(),
            NativeProcessBackend::Pty(process) => process.kill_tree(),
        }
    }

    fn has_pending_combined(&self) -> PyResult<bool> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.has_pending_combined()),
            NativeProcessBackend::Pty(_) => Ok(false),
        }
    }

    fn has_pending_stream(&self, stream: &str) -> PyResult<bool> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.has_pending_stream(stream),
            NativeProcessBackend::Pty(_) => Ok(false),
        }
    }

    fn drain_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.drain_combined(py),
            NativeProcessBackend::Pty(_) => Ok(Vec::new()),
        }
    }

    fn drain_stream(&self, py: Python<'_>, stream: &str) -> PyResult<Vec<Py<PyAny>>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.drain_stream(py, stream),
            NativeProcessBackend::Pty(_) => {
                let _ = stream;
                Ok(Vec::new())
            }
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn take_combined_line(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<String>, Option<Py<PyAny>>)> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.take_combined_line(py, timeout),
            NativeProcessBackend::Pty(_) => Ok(("eof".into(), None, None)),
        }
    }

    #[pyo3(signature = (stream, timeout=None))]
    fn take_stream_line(
        &self,
        py: Python<'_>,
        stream: &str,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<Py<PyAny>>)> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.take_stream_line(py, stream, timeout),
            NativeProcessBackend::Pty(_) => {
                let _ = (py, stream, timeout);
                Ok(("eof".into(), None))
            }
        }
    }

    fn captured_stdout(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.captured_stdout(py),
            NativeProcessBackend::Pty(_) => Ok(Vec::new()),
        }
    }

    fn captured_stderr(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.captured_stderr(py),
            NativeProcessBackend::Pty(_) => Ok(Vec::new()),
        }
    }

    fn captured_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.captured_combined(py),
            NativeProcessBackend::Pty(_) => Ok(Vec::new()),
        }
    }

    fn captured_stream_bytes(&self, stream: &str) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.captured_stream_bytes(stream),
            NativeProcessBackend::Pty(_) => Ok(0),
        }
    }

    fn captured_combined_bytes(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.captured_combined_bytes()),
            NativeProcessBackend::Pty(_) => Ok(0),
        }
    }

    fn clear_captured_stream(&self, stream: &str) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.clear_captured_stream(stream),
            NativeProcessBackend::Pty(_) => Ok(0),
        }
    }

    fn clear_captured_combined(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.clear_captured_combined()),
            NativeProcessBackend::Pty(_) => Ok(0),
        }
    }

    fn write_stdin(&self, data: &[u8]) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.write_stdin(data),
            NativeProcessBackend::Pty(process) => process.write(data, false),
        }
    }

    #[pyo3(signature = (data, submit=false))]
    fn write(&self, data: &[u8], submit: bool) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.write_stdin(data),
            NativeProcessBackend::Pty(process) => process.write(data, submit),
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn read_chunk(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.read_chunk(py, timeout),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "read_chunk is only available for PTY-backed NativeProcess",
            )),
        }
    }

    #[pyo3(signature = (timeout=None))]
    fn wait_for_pty_reader_closed(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<bool> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.wait_for_reader_closed(py, timeout),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "wait_for_pty_reader_closed is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn respond_to_queries(&self, data: &[u8]) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.respond_to_queries(data),
            NativeProcessBackend::Running(_) => Ok(()),
        }
    }

    fn resize(&self, rows: u16, cols: u16) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.resize(rows, cols),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "resize is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn send_interrupt(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.send_interrupt(),
            NativeProcessBackend::Pty(process) => process.send_interrupt(),
        }
    }

    #[pyo3(signature = (stream, pattern, is_regex=false, timeout=None))]
    fn expect(
        &self,
        py: Python<'_>,
        stream: &str,
        pattern: &str,
        is_regex: bool,
        timeout: Option<f64>,
    ) -> PyResult<ExpectResult> {
        match &self.backend {
            NativeProcessBackend::Running(process) => {
                process.expect(py, stream, pattern, is_regex, timeout)
            }
            NativeProcessBackend::Pty(_) => Err(PyRuntimeError::new_err(
                "expect is only available for subprocess-backed NativeProcess",
            )),
        }
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Running(process) => process.close(py),
            NativeProcessBackend::Pty(process) => process.close(py),
        }
    }

    fn start_terminal_input_relay(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => process.start_terminal_input_relay(),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "terminal input relay is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn stop_terminal_input_relay(&self) -> PyResult<()> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => {
                process.stop_terminal_input_relay();
                Ok(())
            }
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "terminal input relay is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn terminal_input_relay_active(&self) -> PyResult<bool> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => Ok(process.terminal_input_relay_active()),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "terminal input relay is only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn pty_input_bytes_total(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => Ok(process.pty_input_bytes_total()),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "PTY input metrics are only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn pty_newline_events_total(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => Ok(process.pty_newline_events_total()),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "PTY input metrics are only available for PTY-backed NativeProcess",
            )),
        }
    }

    fn pty_submit_events_total(&self) -> PyResult<usize> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => Ok(process.pty_submit_events_total()),
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "PTY input metrics are only available for PTY-backed NativeProcess",
            )),
        }
    }

    #[getter]
    fn pid(&self) -> PyResult<Option<u32>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.pid()),
            NativeProcessBackend::Pty(process) => process.pid(),
        }
    }

    #[getter]
    fn returncode(&self) -> PyResult<Option<i32>> {
        match &self.backend {
            NativeProcessBackend::Running(process) => Ok(process.returncode()),
            NativeProcessBackend::Pty(process) => Ok(*process
                .inner
                .returncode
                .lock()
                .expect("pty returncode mutex poisoned")),
        }
    }

    fn is_pty(&self) -> bool {
        matches!(self.backend, NativeProcessBackend::Pty(_))
    }

    /// Wait for exit then drain remaining output (PTY only).
    #[pyo3(signature = (timeout=None, drain_timeout=2.0))]
    fn wait_and_drain(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
        drain_timeout: f64,
    ) -> PyResult<i32> {
        match &self.backend {
            NativeProcessBackend::Pty(process) => {
                process.wait_and_drain(py, timeout, drain_timeout)
            }
            NativeProcessBackend::Running(_) => Err(PyRuntimeError::new_err(
                "wait_and_drain is only available for PTY-backed NativeProcess",
            )),
        }
    }
}
