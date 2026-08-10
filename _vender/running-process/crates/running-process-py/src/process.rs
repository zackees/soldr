use std::path::PathBuf;
use std::time::{Duration, Instant};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyString};
use regex::Regex;

#[cfg(unix)]
use running_process::{unix_signal_process, unix_signal_process_group, UnixSignal};
use running_process::{NativeProcess, ProcessConfig, ReadStatus, StreamEvent, StreamKind};

use crate::helpers::{
    parse_command, process_err_to_py, stderr_mode, stdin_mode, stream_kind, to_py_err,
};
use crate::public_symbols;
use crate::registry::{ExpectDetails, ExpectResult};

#[pyclass]
pub(crate) struct NativeRunningProcess {
    pub(crate) inner: NativeProcess,
    pub(crate) text: bool,
    pub(crate) encoding: Option<String>,
    pub(crate) errors: Option<String>,
    #[cfg(windows)]
    pub(crate) creationflags: Option<u32>,
    #[cfg(unix)]
    pub(crate) create_process_group: bool,
}

#[pymethods]
impl NativeRunningProcess {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (command, cwd=None, shell=false, capture=true, env=None, creationflags=None, text=true, encoding=None, errors=None, stdin_mode_name="inherit", stderr_mode_name="stdout", nice=None, create_process_group=false))]
    pub(crate) fn new(
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
        let parsed = parse_command(command, shell)?;
        let env_pairs = env
            .map(|mapping| {
                mapping
                    .iter()
                    .map(|(key, value)| Ok((key.extract::<String>()?, value.extract::<String>()?)))
                    .collect::<PyResult<Vec<(String, String)>>>()
            })
            .transpose()?;

        Ok(Self {
            inner: NativeProcess::new(ProcessConfig {
                command: parsed,
                cwd: cwd.map(PathBuf::from),
                env: env_pairs,
                capture,
                stderr_mode: stderr_mode(stderr_mode_name)?,
                creationflags,
                create_process_group,
                stdin_mode: stdin_mode(stdin_mode_name)?,
                nice,
            }),
            text,
            encoding,
            errors,
            #[cfg(windows)]
            creationflags,
            #[cfg(unix)]
            create_process_group,
        })
    }

    #[inline(never)]
    pub(crate) fn start(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_start_public(self)
    }

    pub(crate) fn poll(&self) -> PyResult<Option<i32>> {
        self.inner.poll().map_err(to_py_err)
    }

    #[pyo3(signature = (timeout=None))]
    #[inline(never)]
    pub(crate) fn wait(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<i32> {
        public_symbols::rp_native_running_process_wait_public(self, py, timeout)
    }

    #[inline(never)]
    pub(crate) fn kill(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_kill_public(self)
    }

    #[inline(never)]
    pub(crate) fn terminate(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_terminate_public(self)
    }

    #[inline(never)]
    pub(crate) fn close(&self, py: Python<'_>) -> PyResult<()> {
        public_symbols::rp_native_running_process_close_public(self, py)
    }

    pub(crate) fn terminate_group(&self) -> PyResult<()> {
        #[cfg(unix)]
        {
            let pid = self
                .inner
                .pid()
                .ok_or_else(|| PyRuntimeError::new_err("process is not running"))?;
            if self.create_process_group {
                unix_signal_process_group(pid as i32, UnixSignal::Terminate).map_err(to_py_err)?;
                return Ok(());
            }
        }
        self.inner.terminate().map_err(to_py_err)
    }

    pub(crate) fn write_stdin(&self, data: &[u8]) -> PyResult<()> {
        self.inner.write_stdin(data).map_err(to_py_err)
    }

    #[getter]
    pub(crate) fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    #[getter]
    pub(crate) fn returncode(&self) -> Option<i32> {
        self.inner.returncode()
    }

    #[inline(never)]
    pub(crate) fn send_interrupt(&self) -> PyResult<()> {
        public_symbols::rp_native_running_process_send_interrupt_public(self)
    }

    pub(crate) fn kill_group(&self) -> PyResult<()> {
        #[cfg(unix)]
        {
            let pid = self
                .inner
                .pid()
                .ok_or_else(|| PyRuntimeError::new_err("process is not running"))?;
            if self.create_process_group {
                unix_signal_process_group(pid as i32, UnixSignal::Kill).map_err(to_py_err)?;
                return Ok(());
            }
        }
        self.inner.kill().map_err(to_py_err)
    }

    pub(crate) fn has_pending_combined(&self) -> bool {
        self.inner.has_pending_combined()
    }

    pub(crate) fn has_pending_stream(&self, stream: &str) -> PyResult<bool> {
        Ok(self.inner.has_pending_stream(stream_kind(stream)?))
    }

    pub(crate) fn drain_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        self.inner
            .drain_combined()
            .into_iter()
            .map(|event| {
                Ok((
                    event.stream.as_str().to_string(),
                    self.decode_line(py, &event.line)?,
                ))
            })
            .collect()
    }

    pub(crate) fn drain_stream(&self, py: Python<'_>, stream: &str) -> PyResult<Vec<Py<PyAny>>> {
        self.inner
            .drain_stream(stream_kind(stream)?)
            .into_iter()
            .map(|line| self.decode_line(py, &line))
            .collect()
    }

    #[pyo3(signature = (timeout=None))]
    pub(crate) fn take_combined_line(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<String>, Option<Py<PyAny>>)> {
        match self
            .inner
            .read_combined(timeout.map(Duration::from_secs_f64))
        {
            ReadStatus::Line(StreamEvent { stream, line }) => Ok((
                "line".into(),
                Some(stream.as_str().into()),
                Some(self.decode_line(py, &line)?),
            )),
            ReadStatus::Timeout => Ok(("timeout".into(), None, None)),
            ReadStatus::Eof => Ok(("eof".into(), None, None)),
        }
    }

    #[pyo3(signature = (stream, timeout=None))]
    pub(crate) fn take_stream_line(
        &self,
        py: Python<'_>,
        stream: &str,
        timeout: Option<f64>,
    ) -> PyResult<(String, Option<Py<PyAny>>)> {
        match self
            .inner
            .read_stream(stream_kind(stream)?, timeout.map(Duration::from_secs_f64))
        {
            ReadStatus::Line(line) => Ok(("line".into(), Some(self.decode_line(py, &line)?))),
            ReadStatus::Timeout => Ok(("timeout".into(), None)),
            ReadStatus::Eof => Ok(("eof".into(), None)),
        }
    }

    pub(crate) fn captured_stdout(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        self.inner
            .captured_stdout()
            .into_iter()
            .map(|line| self.decode_line(py, &line))
            .collect()
    }

    pub(crate) fn captured_stderr(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        self.inner
            .captured_stderr()
            .into_iter()
            .map(|line| self.decode_line(py, &line))
            .collect()
    }

    pub(crate) fn captured_combined(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        self.inner
            .captured_combined()
            .into_iter()
            .map(|event| {
                Ok((
                    event.stream.as_str().to_string(),
                    self.decode_line(py, &event.line)?,
                ))
            })
            .collect()
    }

    pub(crate) fn captured_stream_bytes(&self, stream: &str) -> PyResult<usize> {
        Ok(self.inner.captured_stream_bytes(stream_kind(stream)?))
    }

    pub(crate) fn captured_combined_bytes(&self) -> usize {
        self.inner.captured_combined_bytes()
    }

    pub(crate) fn clear_captured_stream(&self, stream: &str) -> PyResult<usize> {
        Ok(self.inner.clear_captured_stream(stream_kind(stream)?))
    }

    pub(crate) fn clear_captured_combined(&self) -> usize {
        self.inner.clear_captured_combined()
    }

    #[pyo3(signature = (stream, pattern, is_regex=false, timeout=None))]
    pub(crate) fn expect(
        &self,
        py: Python<'_>,
        stream: &str,
        pattern: &str,
        is_regex: bool,
        timeout: Option<f64>,
    ) -> PyResult<ExpectResult> {
        let stream_kind = if stream == "combined" {
            None
        } else {
            Some(stream_kind(stream)?)
        };
        let mut buffer = match stream_kind {
            Some(kind) => self.captured_stream_text(py, kind)?,
            None => self.captured_combined_text(py)?,
        };
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
        let compiled_regex = if is_regex {
            Some(Regex::new(pattern).map_err(to_py_err)?)
        } else {
            None
        };

        loop {
            if let Some((matched, start, end, groups)) =
                self.find_expect_match(&buffer, pattern, compiled_regex.as_ref())?
            {
                return Ok((
                    "match".to_string(),
                    buffer,
                    Some(matched),
                    Some(start),
                    Some(end),
                    groups,
                ));
            }

            let wait_timeout = deadline.map(|limit| {
                let now = Instant::now();
                if now >= limit {
                    Duration::from_secs(0)
                } else {
                    limit
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(100))
                }
            });
            if deadline.is_some_and(|limit| Instant::now() >= limit) {
                return Ok(("timeout".to_string(), buffer, None, None, None, Vec::new()));
            }

            match self.read_status_text(stream_kind, wait_timeout)? {
                ReadStatus::Line(line) => {
                    let decoded = self.decode_line_to_string(py, &line)?;
                    buffer.push_str(&decoded);
                    buffer.push('\n');
                }
                ReadStatus::Timeout => {
                    // Keep polling until the overall expect deadline expires.
                    continue;
                }
                ReadStatus::Eof => {
                    return Ok(("eof".to_string(), buffer, None, None, None, Vec::new()));
                }
            }
        }
    }

    #[staticmethod]
    pub(crate) fn is_pty_available() -> bool {
        false
    }
}

impl NativeRunningProcess {
    pub(crate) fn start_impl(&self) -> PyResult<()> {
        running_process::rp_rust_debug_scope!("running_process_py::NativeRunningProcess::start");
        self.inner.start().map_err(to_py_err)
    }

    pub(crate) fn wait_impl(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<i32> {
        running_process::rp_rust_debug_scope!("running_process_py::NativeRunningProcess::wait");
        py.detach(|| {
            self.inner
                .wait(timeout.map(Duration::from_secs_f64))
                .map_err(process_err_to_py)
        })
    }

    pub(crate) fn kill_impl(&self) -> PyResult<()> {
        running_process::rp_rust_debug_scope!("running_process_py::NativeRunningProcess::kill");
        self.inner.kill().map_err(to_py_err)
    }

    pub(crate) fn terminate_impl(&self) -> PyResult<()> {
        running_process::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::terminate"
        );
        self.inner.terminate().map_err(to_py_err)
    }

    pub(crate) fn close_impl(&self, py: Python<'_>) -> PyResult<()> {
        running_process::rp_rust_debug_scope!("running_process_py::NativeRunningProcess::close");
        py.detach(|| self.inner.close().map_err(process_err_to_py))
    }

    pub(crate) fn send_interrupt_impl(&self) -> PyResult<()> {
        running_process::rp_rust_debug_scope!(
            "running_process_py::NativeRunningProcess::send_interrupt"
        );
        let pid = self
            .inner
            .pid()
            .ok_or_else(|| PyRuntimeError::new_err("process is not running"))?;

        #[cfg(windows)]
        {
            public_symbols::rp_windows_generate_console_ctrl_break_public(pid, self.creationflags)
        }

        #[cfg(unix)]
        {
            if self.create_process_group {
                unix_signal_process_group(pid as i32, UnixSignal::Interrupt).map_err(to_py_err)?;
            } else {
                unix_signal_process(pid, UnixSignal::Interrupt).map_err(to_py_err)?;
            }
            Ok(())
        }
    }

    pub(crate) fn decode_line_to_string(&self, py: Python<'_>, line: &[u8]) -> PyResult<String> {
        if !self.text {
            return Ok(String::from_utf8_lossy(line).into_owned());
        }
        let encoding = self.encoding.as_deref().unwrap_or("utf-8");
        let errors = self.errors.as_deref().unwrap_or("replace");
        if encoding == "utf-8" && errors == "replace" {
            return Ok(String::from_utf8_lossy(line).into_owned());
        }
        PyBytes::new(py, line)
            .call_method1("decode", (encoding, errors))?
            .extract()
    }

    pub(crate) fn captured_stream_text(
        &self,
        py: Python<'_>,
        stream: StreamKind,
    ) -> PyResult<String> {
        let lines = match stream {
            StreamKind::Stdout => self.inner.captured_stdout(),
            StreamKind::Stderr => self.inner.captured_stderr(),
        };
        let mut text = String::new();
        for (index, line) in lines.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.push_str(&self.decode_line_to_string(py, line)?);
        }
        Ok(text)
    }

    pub(crate) fn captured_combined_text(&self, py: Python<'_>) -> PyResult<String> {
        let lines = self.inner.captured_combined();
        let mut text = String::new();
        for (index, event) in lines.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.push_str(&self.decode_line_to_string(py, &event.line)?);
        }
        Ok(text)
    }

    pub(crate) fn read_status_text(
        &self,
        stream: Option<StreamKind>,
        timeout: Option<Duration>,
    ) -> PyResult<ReadStatus<Vec<u8>>> {
        Ok(match stream {
            Some(kind) => self.inner.read_stream(kind, timeout),
            None => match self.inner.read_combined(timeout) {
                ReadStatus::Line(StreamEvent { line, .. }) => ReadStatus::Line(line),
                ReadStatus::Timeout => ReadStatus::Timeout,
                ReadStatus::Eof => ReadStatus::Eof,
            },
        })
    }

    pub(crate) fn find_expect_match(
        &self,
        buffer: &str,
        pattern: &str,
        compiled_regex: Option<&Regex>,
    ) -> PyResult<Option<ExpectDetails>> {
        if compiled_regex.is_none() {
            // Literal string match
            let Some(start) = buffer.find(pattern) else {
                return Ok(None);
            };
            return Ok(Some((
                pattern.to_string(),
                start,
                start + pattern.len(),
                Vec::new(),
            )));
        }

        let regex = compiled_regex.unwrap();
        let Some(captures) = regex.captures(buffer) else {
            return Ok(None);
        };
        let whole = captures
            .get(0)
            .ok_or_else(|| PyRuntimeError::new_err("regex capture missing group 0"))?;
        let groups = captures
            .iter()
            .skip(1)
            .map(|group| {
                group
                    .map(|value| value.as_str().to_string())
                    .unwrap_or_default()
            })
            .collect();
        Ok(Some((
            whole.as_str().to_string(),
            whole.start(),
            whole.end(),
            groups,
        )))
    }

    pub(crate) fn decode_line(&self, py: Python<'_>, line: &[u8]) -> PyResult<Py<PyAny>> {
        if !self.text {
            return Ok(PyBytes::new(py, line).into_any().unbind());
        }
        let encoding = self.encoding.as_deref().unwrap_or("utf-8");
        let errors = self.errors.as_deref().unwrap_or("replace");
        if encoding == "utf-8" && errors == "replace" {
            let s = String::from_utf8_lossy(line);
            return Ok(PyString::new(py, &s).into_any().unbind());
        }
        Ok(PyBytes::new(py, line)
            .call_method1("decode", (encoding, errors))?
            .into_any()
            .unbind())
    }
}
