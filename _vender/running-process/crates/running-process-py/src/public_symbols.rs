#![allow(improper_ctypes_definitions)]

use pyo3::prelude::*;

use crate::priority::native_apply_process_nice_impl;
#[cfg(windows)]
use crate::priority::{
    windows_apply_process_priority_impl, windows_generate_console_ctrl_break_impl,
};
use crate::process::NativeRunningProcess;

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_apply_process_nice_public(pid: u32, nice: i32) -> PyResult<()> {
    native_apply_process_nice_impl(pid, nice)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_windows_apply_process_priority_public(pid: u32, nice: i32) -> PyResult<()> {
    windows_apply_process_priority_impl(pid, nice)
}

#[cfg(windows)]
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_windows_generate_console_ctrl_break_public(
    pid: u32,
    creationflags: Option<u32>,
) -> PyResult<()> {
    windows_generate_console_ctrl_break_impl(pid, creationflags)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_start_public(
    process: &NativeRunningProcess,
) -> PyResult<()> {
    process.start_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_wait_public(
    process: &NativeRunningProcess,
    py: Python<'_>,
    timeout: Option<f64>,
) -> PyResult<i32> {
    process.wait_impl(py, timeout)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_kill_public(
    process: &NativeRunningProcess,
) -> PyResult<()> {
    process.kill_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_terminate_public(
    process: &NativeRunningProcess,
) -> PyResult<()> {
    process.terminate_impl()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_close_public(
    process: &NativeRunningProcess,
    py: Python<'_>,
) -> PyResult<()> {
    process.close_impl(py)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn rp_native_running_process_send_interrupt_public(
    process: &NativeRunningProcess,
) -> PyResult<()> {
    process.send_interrupt_impl()
}
