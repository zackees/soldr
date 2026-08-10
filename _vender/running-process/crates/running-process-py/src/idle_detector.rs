use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use pyo3::prelude::*;

use running_process::pty::{IdleDetectorCore, IdleMonitorState};

use crate::signal_bool::NativeSignalBool;

#[pyclass]
pub(crate) struct NativeIdleDetector {
    pub(crate) core: Arc<IdleDetectorCore>,
}

#[pymethods]
impl NativeIdleDetector {
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (timeout_seconds, stability_window_seconds, sample_interval_seconds, enabled_signal, reset_on_input=true, reset_on_output=true, count_control_churn_as_output=true, initial_idle_for_seconds=0.0))]
    pub(crate) fn new(
        py: Python<'_>,
        timeout_seconds: f64,
        stability_window_seconds: f64,
        sample_interval_seconds: f64,
        enabled_signal: Py<NativeSignalBool>,
        reset_on_input: bool,
        reset_on_output: bool,
        count_control_churn_as_output: bool,
        initial_idle_for_seconds: f64,
    ) -> Self {
        let now = Instant::now();
        let initial_idle_for_seconds = initial_idle_for_seconds.max(0.0);
        let last_reset_at = now
            .checked_sub(Duration::from_secs_f64(initial_idle_for_seconds))
            .unwrap_or(now);
        let enabled = enabled_signal.borrow(py).value.clone();
        Self {
            core: Arc::new(IdleDetectorCore {
                timeout_seconds,
                stability_window_seconds,
                sample_interval_seconds,
                reset_on_input,
                reset_on_output,
                count_control_churn_as_output,
                enabled,
                state: Mutex::new(IdleMonitorState {
                    last_reset_at,
                    returncode: None,
                    interrupted: false,
                }),
                condvar: Condvar::new(),
            }),
        }
    }

    #[getter]
    pub(crate) fn enabled(&self) -> bool {
        self.core.enabled()
    }

    #[setter]
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.core.set_enabled(enabled);
    }

    pub(crate) fn record_input(&self, byte_count: usize) {
        self.core.record_input(byte_count);
    }

    pub(crate) fn record_output(&self, data: &[u8]) {
        self.core.record_output(data);
    }

    pub(crate) fn mark_exit(&self, returncode: i32, interrupted: bool) {
        self.core.mark_exit(returncode, interrupted);
    }

    #[pyo3(signature = (timeout=None))]
    pub(crate) fn wait(
        &self,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> (bool, String, f64, Option<i32>) {
        py.detach(|| self.core.wait(timeout))
    }
}
