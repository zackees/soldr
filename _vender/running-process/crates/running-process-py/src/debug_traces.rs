use std::time::Duration;

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use running_process::render_rust_debug_traces;
#[cfg(windows)]
use std::fs;

#[cfg(windows)]
use crate::helpers::to_py_err;

#[pyfunction]
pub(crate) fn native_windows_terminal_input_bytes(py: Python<'_>, data: &[u8]) -> Py<PyAny> {
    #[cfg(windows)]
    let payload = running_process::pty::windows_terminal_input_payload(data);
    #[cfg(not(windows))]
    let payload = data.to_vec();
    PyBytes::new(py, &payload).into_any().unbind()
}

#[pyfunction]
pub(crate) fn native_dump_rust_debug_traces() -> String {
    render_rust_debug_traces()
}

#[pyfunction]
pub(crate) fn native_test_capture_rust_debug_trace() -> String {
    #[inline(never)]
    fn inner() -> String {
        running_process::rp_rust_debug_scope!(
            "running_process_py::native_test_capture_rust_debug_trace::inner"
        );
        render_rust_debug_traces()
    }

    #[inline(never)]
    fn outer() -> String {
        running_process::rp_rust_debug_scope!(
            "running_process_py::native_test_capture_rust_debug_trace::outer"
        );
        inner()
    }

    outer()
}

#[cfg(windows)]
#[no_mangle]
#[inline(never)]
pub fn running_process_py_debug_hang_inner(release_path: &std::path::Path) -> PyResult<()> {
    running_process::rp_rust_debug_scope!("running_process_py::debug_hang_inner");
    while !release_path.exists() {
        std::hint::spin_loop();
    }
    Ok(())
}

#[cfg(windows)]
#[no_mangle]
#[inline(never)]
pub fn running_process_py_debug_hang_outer(
    ready_path: &std::path::Path,
    release_path: &std::path::Path,
) -> PyResult<()> {
    running_process::rp_rust_debug_scope!("running_process_py::debug_hang_outer");
    fs::write(ready_path, b"ready").map_err(to_py_err)?;
    running_process_py_debug_hang_inner(release_path)
}

#[pyfunction]
#[cfg(windows)]
#[inline(never)]
pub(crate) fn native_test_hang_in_rust(ready_path: String, release_path: String) -> PyResult<()> {
    running_process::rp_rust_debug_scope!("running_process_py::native_test_hang_in_rust");
    running_process_py_debug_hang_outer(
        std::path::Path::new(&ready_path),
        std::path::Path::new(&release_path),
    )
}

/// Monitor for new visible windows that appear within the given duration.
///
/// Returns a list of dicts, each with keys: ``pid`` (int), ``title`` (str),
/// ``hwnd`` (int).  On non-Windows platforms this always returns an empty list.
#[pyfunction]
pub(crate) fn monitor_console_windows(py: Python<'_>, duration_secs: f64) -> PyResult<Py<PyAny>> {
    let duration = Duration::from_secs_f64(duration_secs);
    let infos = running_process::monitor_console_windows(duration);
    let list = PyList::empty(py);
    for info in infos {
        let dict = PyDict::new(py);
        dict.set_item("pid", info.pid)?;
        dict.set_item("title", &info.title)?;
        dict.set_item("hwnd", info.hwnd)?;
        list.append(dict)?;
    }
    Ok(list.into_any().unbind())
}
