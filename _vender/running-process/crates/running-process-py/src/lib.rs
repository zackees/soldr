use pyo3::prelude::*;
use pyo3::types::PyString;

mod daemon_client;
mod public_symbols;

mod containment;
mod debug_traces;
mod helpers;
mod idle_detector;
mod metrics;
mod originator;
mod pid_tracking;
mod priority;
mod probe;
mod process;
mod process_tree;
mod pty_buffer;
mod pty_process;
mod py_native_process;
mod registry;
mod signal_bool;
mod terminal_input;
mod window_icon;

#[cfg(test)]
mod tests;

// Re-exports for cross-module access (used by public_symbols.rs and tests).
pub(crate) use containment::PyContainedProcessGroup;
#[cfg(windows)]
pub(crate) use debug_traces::native_test_hang_in_rust;
pub(crate) use debug_traces::{
    monitor_console_windows, native_dump_rust_debug_traces, native_test_capture_rust_debug_trace,
    native_windows_terminal_input_bytes,
};
pub(crate) use idle_detector::NativeIdleDetector;
pub(crate) use metrics::NativeProcessMetrics;
pub(crate) use originator::{py_find_processes_by_originator, PyOriginatorProcessInfo};
pub(crate) use pid_tracking::{
    list_tracked_processes, native_cleanup_tracked_processes, native_list_active_processes,
    native_register_process, native_unregister_process, track_process_pid, tracked_pid_db_path_py,
    untrack_process_pid,
};
pub(crate) use priority::native_apply_process_nice;
pub(crate) use probe::{
    native_probe_enable_faulthandler, native_probe_install, native_probe_is_armed,
    native_probe_snapshot, native_probe_snapshot_supported, native_probe_uninstall,
};
pub(crate) use process::NativeRunningProcess;
pub(crate) use process_tree::{
    native_get_process_tree_info, native_is_same_process, native_kill_process_tree,
    native_launch_detached, native_process_created_at,
};
pub(crate) use pty_buffer::NativePtyBuffer;
pub(crate) use pty_process::NativePtyProcess;
pub(crate) use py_native_process::PyNativeProcess;
pub(crate) use signal_bool::NativeSignalBool;
pub(crate) use terminal_input::{NativeTerminalInput, NativeTerminalInputEvent};
pub(crate) use window_icon::{
    native_set_window_icon_from_bytes, native_set_window_icon_from_path,
    native_set_window_icon_stock, native_stock_icon_names, native_window_icon_support,
};

#[pymodule]
fn _native(_py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_class::<PyNativeProcess>()?;
    module.add_class::<NativeRunningProcess>()?;
    module.add_class::<PyContainedProcessGroup>()?;
    module.add_class::<PyOriginatorProcessInfo>()?;
    module.add_function(wrap_pyfunction!(py_find_processes_by_originator, module)?)?;
    module.add_class::<NativePtyProcess>()?;
    module.add_class::<NativeProcessMetrics>()?;
    module.add_class::<NativeSignalBool>()?;
    module.add_class::<NativeIdleDetector>()?;
    module.add_class::<NativePtyBuffer>()?;
    module.add_class::<NativeTerminalInput>()?;
    module.add_class::<NativeTerminalInputEvent>()?;
    module.add_function(wrap_pyfunction!(tracked_pid_db_path_py, module)?)?;
    module.add_function(wrap_pyfunction!(track_process_pid, module)?)?;
    module.add_function(wrap_pyfunction!(untrack_process_pid, module)?)?;
    module.add_function(wrap_pyfunction!(native_register_process, module)?)?;
    module.add_function(wrap_pyfunction!(native_unregister_process, module)?)?;
    module.add_function(wrap_pyfunction!(list_tracked_processes, module)?)?;
    module.add_function(wrap_pyfunction!(native_list_active_processes, module)?)?;
    module.add_function(wrap_pyfunction!(native_launch_detached, module)?)?;
    module.add_function(wrap_pyfunction!(native_get_process_tree_info, module)?)?;
    module.add_function(wrap_pyfunction!(native_kill_process_tree, module)?)?;
    module.add_function(wrap_pyfunction!(native_process_created_at, module)?)?;
    module.add_function(wrap_pyfunction!(native_is_same_process, module)?)?;
    module.add_function(wrap_pyfunction!(native_cleanup_tracked_processes, module)?)?;
    module.add_function(wrap_pyfunction!(native_apply_process_nice, module)?)?;
    module.add_function(wrap_pyfunction!(
        native_windows_terminal_input_bytes,
        module
    )?)?;
    module.add_function(wrap_pyfunction!(native_dump_rust_debug_traces, module)?)?;
    module.add_function(wrap_pyfunction!(
        native_test_capture_rust_debug_trace,
        module
    )?)?;
    #[cfg(windows)]
    module.add_function(wrap_pyfunction!(native_test_hang_in_rust, module)?)?;
    module.add_function(wrap_pyfunction!(native_probe_install, module)?)?;
    module.add_function(wrap_pyfunction!(native_probe_enable_faulthandler, module)?)?;
    module.add_function(wrap_pyfunction!(native_probe_uninstall, module)?)?;
    module.add_function(wrap_pyfunction!(native_probe_is_armed, module)?)?;
    module.add_function(wrap_pyfunction!(native_probe_snapshot, module)?)?;
    module.add_function(wrap_pyfunction!(native_probe_snapshot_supported, module)?)?;
    module.add_function(wrap_pyfunction!(native_window_icon_support, module)?)?;
    module.add_function(wrap_pyfunction!(native_set_window_icon_from_path, module)?)?;
    module.add_function(wrap_pyfunction!(native_set_window_icon_from_bytes, module)?)?;
    module.add_function(wrap_pyfunction!(native_set_window_icon_stock, module)?)?;
    module.add_function(wrap_pyfunction!(native_stock_icon_names, module)?)?;
    module.add_function(wrap_pyfunction!(monitor_console_windows, module)?)?;
    module.add("VERSION", PyString::new(_py, env!("CARGO_PKG_VERSION")))?;
    Ok(())
}
