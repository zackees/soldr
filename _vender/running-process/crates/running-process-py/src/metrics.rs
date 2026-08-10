use std::sync::Mutex;

use pyo3::prelude::*;
use sysinfo::{Pid, ProcessRefreshKind, System, UpdateKind};

use crate::helpers::system_pid;

#[pyclass]
pub(crate) struct NativeProcessMetrics {
    pub(crate) pid: Pid,
    pub(crate) system: Mutex<System>,
}

#[pymethods]
impl NativeProcessMetrics {
    #[new]
    fn new(pid: u32) -> Self {
        let pid = system_pid(pid);
        let mut system = System::new();
        system.refresh_process_specifics(
            pid,
            ProcessRefreshKind::new()
                .with_cpu()
                .with_disk_usage()
                .with_memory()
                .with_exe(UpdateKind::Never),
        );
        Self {
            pid,
            system: Mutex::new(system),
        }
    }

    pub(crate) fn prime(&self) {
        let mut system = self.system.lock().expect("process metrics mutex poisoned");
        system.refresh_process_specifics(
            self.pid,
            ProcessRefreshKind::new()
                .with_cpu()
                .with_disk_usage()
                .with_memory()
                .with_exe(UpdateKind::Never),
        );
    }

    pub(crate) fn sample(&self) -> (bool, f32, u64, u64) {
        let mut system = self.system.lock().expect("process metrics mutex poisoned");
        system.refresh_process_specifics(
            self.pid,
            ProcessRefreshKind::new()
                .with_cpu()
                .with_disk_usage()
                .with_memory()
                .with_exe(UpdateKind::Never),
        );
        let Some(process) = system.process(self.pid) else {
            return (false, 0.0, 0, 0);
        };
        let disk = process.disk_usage();
        (
            true,
            process.cpu_usage(),
            disk.total_read_bytes
                .saturating_add(disk.total_written_bytes),
            0,
        )
    }
}
