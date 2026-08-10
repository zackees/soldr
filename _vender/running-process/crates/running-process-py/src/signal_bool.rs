use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pyo3::prelude::*;

#[pyclass(skip_from_py_object)]
#[derive(Clone)]
pub(crate) struct NativeSignalBool {
    pub(crate) value: Arc<AtomicBool>,
    pub(crate) write_lock: Arc<Mutex<()>>,
}

#[pymethods]
impl NativeSignalBool {
    #[new]
    #[pyo3(signature = (value=false))]
    pub(crate) fn new(value: bool) -> Self {
        Self {
            value: Arc::new(AtomicBool::new(value)),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    #[getter]
    fn value(&self) -> bool {
        self.load_nolock()
    }

    #[setter]
    fn set_value(&self, value: bool) {
        self.store_locked(value);
    }

    pub(crate) fn load_nolock(&self) -> bool {
        self.value.load(Ordering::Acquire)
    }

    pub(crate) fn store_locked(&self, value: bool) {
        let _guard = self.write_lock.lock().expect("signal bool mutex poisoned");
        self.value.store(value, Ordering::Release);
    }

    pub(crate) fn compare_and_swap_locked(&self, current: bool, new: bool) -> bool {
        let _guard = self.write_lock.lock().expect("signal bool mutex poisoned");
        self.value
            .compare_exchange(current, new, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}
