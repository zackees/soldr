#[cfg(windows)]
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

#[cfg(unix)]
use running_process::unix_set_priority;

use crate::helpers::to_py_err;
use crate::public_symbols;

#[pyfunction]
#[inline(never)]
pub(crate) fn native_apply_process_nice(pid: u32, nice: i32) -> PyResult<()> {
    public_symbols::rp_native_apply_process_nice_public(pid, nice)
}

pub(crate) fn native_apply_process_nice_impl(pid: u32, nice: i32) -> PyResult<()> {
    running_process::rp_rust_debug_scope!("running_process_py::native_apply_process_nice");
    #[cfg(windows)]
    {
        public_symbols::rp_windows_apply_process_priority_public(pid, nice)
    }

    #[cfg(unix)]
    {
        unix_set_priority(pid, nice).map_err(to_py_err)
    }
}

#[cfg(windows)]
pub(crate) fn windows_apply_process_priority_impl(pid: u32, nice: i32) -> PyResult<()> {
    running_process::rp_rust_debug_scope!("running_process_py::windows_apply_process_priority");
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::processthreadsapi::{OpenProcess, SetPriorityClass};
    use winapi::um::winbase::{
        ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
        IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    };
    use winapi::um::winnt::{PROCESS_QUERY_INFORMATION, PROCESS_SET_INFORMATION};

    let priority_class = if nice >= 15 {
        IDLE_PRIORITY_CLASS
    } else if nice >= 1 {
        BELOW_NORMAL_PRIORITY_CLASS
    } else if nice <= -15 {
        HIGH_PRIORITY_CLASS
    } else if nice <= -1 {
        ABOVE_NORMAL_PRIORITY_CLASS
    } else {
        NORMAL_PRIORITY_CLASS
    };

    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_SET_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(to_py_err(std::io::Error::last_os_error()));
    }
    let result = unsafe { SetPriorityClass(handle, priority_class) };
    let close_result = unsafe { CloseHandle(handle) };
    if close_result == 0 {
        return Err(to_py_err(std::io::Error::last_os_error()));
    }
    if result == 0 {
        return Err(to_py_err(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn windows_generate_console_ctrl_break_impl(
    pid: u32,
    creationflags: Option<u32>,
) -> PyResult<()> {
    running_process::rp_rust_debug_scope!(
        "running_process_py::windows_generate_console_ctrl_break"
    );
    use winapi::um::wincon::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};

    let new_process_group =
        creationflags.unwrap_or(0) & winapi::um::winbase::CREATE_NEW_PROCESS_GROUP;
    if new_process_group == 0 {
        return Err(PyRuntimeError::new_err(
            "send_interrupt on Windows requires CREATE_NEW_PROCESS_GROUP",
        ));
    }
    let result = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    if result == 0 {
        return Err(to_py_err(std::io::Error::last_os_error()));
    }
    Ok(())
}
