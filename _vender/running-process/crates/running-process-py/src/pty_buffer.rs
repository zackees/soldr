use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use pyo3::exceptions::{PyRuntimeError, PyTimeoutError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};

pub(crate) struct PtyBufferState {
    pub(crate) chunks: VecDeque<Vec<u8>>,
    pub(crate) history: Vec<u8>,
    pub(crate) history_bytes: usize,
    pub(crate) closed: bool,
}

#[pyclass]
pub(crate) struct NativePtyBuffer {
    pub(crate) text: bool,
    pub(crate) encoding: String,
    pub(crate) errors: String,
    pub(crate) state: Mutex<PtyBufferState>,
    pub(crate) condvar: Condvar,
}

#[pymethods]
impl NativePtyBuffer {
    #[new]
    #[pyo3(signature = (text=false, encoding="utf-8", errors="replace"))]
    pub(crate) fn new(text: bool, encoding: &str, errors: &str) -> Self {
        Self {
            text,
            encoding: encoding.to_string(),
            errors: errors.to_string(),
            state: Mutex::new(PtyBufferState {
                chunks: VecDeque::new(),
                history: Vec::new(),
                history_bytes: 0,
                closed: false,
            }),
            condvar: Condvar::new(),
        }
    }

    pub(crate) fn available(&self) -> bool {
        !self
            .state
            .lock()
            .expect("pty buffer mutex poisoned")
            .chunks
            .is_empty()
    }

    pub(crate) fn record_output(&self, data: &[u8]) {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        guard.history_bytes += data.len();
        guard.history.extend_from_slice(data);
        guard.chunks.push_back(data.to_vec());
        self.condvar.notify_all();
    }

    pub(crate) fn close(&self) {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        guard.closed = true;
        self.condvar.notify_all();
    }

    #[pyo3(signature = (timeout=None))]
    pub(crate) fn read(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Py<PyAny>> {
        // Mirror NativePtyProcess::read_chunk: do the wait WITHOUT the GIL
        // so other Python threads (notably the test/main thread) can make
        // progress instead of being starved by our 100ms read poll loop.
        enum WaitOutcome {
            Chunk(Vec<u8>),
            Closed,
            Timeout,
        }

        let outcome = py.detach(|| -> WaitOutcome {
            let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs_f64(secs));
            let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
            loop {
                if let Some(chunk) = guard.chunks.pop_front() {
                    return WaitOutcome::Chunk(chunk);
                }
                if guard.closed {
                    return WaitOutcome::Closed;
                }
                match deadline {
                    Some(deadline) => {
                        let now = Instant::now();
                        if now >= deadline {
                            return WaitOutcome::Timeout;
                        }
                        let wait = deadline.saturating_duration_since(now);
                        let result = self
                            .condvar
                            .wait_timeout(guard, wait)
                            .expect("pty buffer mutex poisoned");
                        guard = result.0;
                    }
                    None => {
                        guard = self.condvar.wait(guard).expect("pty buffer mutex poisoned");
                    }
                }
            }
        });

        match outcome {
            WaitOutcome::Chunk(chunk) => self.decode_chunk(py, &chunk),
            WaitOutcome::Closed => Err(PyRuntimeError::new_err("Pseudo-terminal stream is closed")),
            WaitOutcome::Timeout => Err(PyTimeoutError::new_err(
                "No pseudo-terminal output available before timeout",
            )),
        }
    }

    pub(crate) fn read_non_blocking(&self, py: Python<'_>) -> PyResult<Option<Py<PyAny>>> {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        if let Some(chunk) = guard.chunks.pop_front() {
            return self.decode_chunk(py, &chunk).map(Some);
        }
        if guard.closed {
            return Err(PyRuntimeError::new_err("Pseudo-terminal stream is closed"));
        }
        Ok(None)
    }

    pub(crate) fn drain(&self, py: Python<'_>) -> PyResult<Vec<Py<PyAny>>> {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        guard
            .chunks
            .drain(..)
            .map(|chunk| self.decode_chunk(py, &chunk))
            .collect()
    }

    pub(crate) fn output(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let guard = self.state.lock().expect("pty buffer mutex poisoned");
        self.decode_chunk(py, &guard.history)
    }

    pub(crate) fn output_since(&self, py: Python<'_>, start: usize) -> PyResult<Py<PyAny>> {
        let guard = self.state.lock().expect("pty buffer mutex poisoned");
        let start = start.min(guard.history.len());
        self.decode_chunk(py, &guard.history[start..])
    }

    pub(crate) fn history_bytes(&self) -> usize {
        self.state
            .lock()
            .expect("pty buffer mutex poisoned")
            .history_bytes
    }

    pub(crate) fn clear_history(&self) -> usize {
        let mut guard = self.state.lock().expect("pty buffer mutex poisoned");
        let released = guard.history_bytes;
        guard.history.clear();
        guard.history_bytes = 0;
        released
    }
}

impl NativePtyBuffer {
    pub(crate) fn decode_chunk(&self, py: Python<'_>, line: &[u8]) -> PyResult<Py<PyAny>> {
        if !self.text {
            return Ok(PyBytes::new(py, line).into_any().unbind());
        }
        if self.encoding == "utf-8" && self.errors == "replace" {
            let s = String::from_utf8_lossy(line);
            return Ok(PyString::new(py, &s).into_any().unbind());
        }
        Ok(PyBytes::new(py, line)
            .call_method1("decode", (&self.encoding, &self.errors))?
            .into_any()
            .unbind())
    }
}
