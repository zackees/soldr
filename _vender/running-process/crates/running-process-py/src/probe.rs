//! Python bindings for probe enrollment (#634).
//!
//! Thin by design. The register/heartbeat/reconnect state machine lives in
//! `running_process::probe` and is shared with native Rust callers — duplicating
//! it here would mean two implementations of the same protocol drifting apart.
//! These functions only translate arguments and hand the resulting guard back
//! to Python as an opaque handle.
//!
//! # Why a handle table rather than a `#[pyclass]`
//!
//! `probe::Guard` deregisters on `Drop`, and the drop must happen when Python
//! asks for it — not whenever the interpreter happens to collect the wrapper.
//! Keeping guards in a table and removing them by id makes teardown explicit
//! and ordered. It also keeps the guard `Send`-agnostic: the table is behind a
//! mutex, so the object Python holds is just an integer.
//!
//! # The GIL
//!
//! `install` returns without doing I/O, so it does not need to release the GIL.
//! Everything that can block already runs on the Rust worker thread, which
//! never touches the interpreter.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule};
use running_process::probe::{self, Config, Guard, Runtime};

/// Live guards, keyed by the handle handed to Python.
static GUARDS: Mutex<Option<HashMap<u64, Guard>>> = Mutex::new(None);

/// Hands out handle ids. Never reused, so a stale handle is inert rather than
/// aliasing a later registration.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn with_guards<R>(f: impl FnOnce(&mut HashMap<u64, Guard>) -> R) -> R {
    let mut slot = match GUARDS.lock() {
        Ok(slot) => slot,
        // A panic elsewhere must not make enrollment permanently unavailable.
        Err(poisoned) => poisoned.into_inner(),
    };
    f(slot.get_or_insert_with(HashMap::new))
}

/// Assemble the config a Python caller's arguments describe.
///
/// Split out from [`native_probe_install`] so the runtime declaration can be
/// asserted without a daemon: `runtime=python` is the whole reason this binding
/// exists rather than callers using the native one, and it is otherwise only
/// observable on the wire.
#[allow(clippy::too_many_arguments)] // Mirrors the flat, keyword-only Python config surface.
fn build_config(
    app_class: &str,
    app_name: Option<String>,
    app_version: Option<String>,
    instance: Option<String>,
    socket_override: Option<String>,
    env_allowlist: Option<Vec<String>>,
    disclose_cwd: bool,
    enable_crash_handler: bool,
) -> Config {
    let mut config = Config::new(app_class).with_runtime(Runtime::Python);

    if let Some(name) = app_name {
        config.app_name = name;
    }
    if let Some(version) = app_version {
        config.app_version = version;
    }
    if let Some(instance) = instance {
        config = config.with_instance(instance);
    }
    if let Some(path) = socket_override {
        config.socket_override = Some(std::path::PathBuf::from(path));
    }
    // Env *values* stay deny-by-default; only names listed here are disclosed.
    if let Some(names) = env_allowlist {
        config.disclosure.env_allowlist = names;
    }
    config.disclosure.disclose_cwd = disclose_cwd;
    if !enable_crash_handler {
        config = config.crash_policy(probe::CrashPolicy::Off);
    }
    config
}

/// Enroll this process with the probe daemon, reporting `runtime=python`.
///
/// Returns an opaque handle for [`native_probe_uninstall`]. Local crash-spool
/// preparation and handler arming finish synchronously; daemon communication
/// never blocks this call and retries on the background worker.
#[pyfunction]
#[pyo3(signature = (app_class, app_name=None, app_version=None, instance=None, socket_override=None, env_allowlist=None, disclose_cwd=false, enable_crash_handler=true))]
#[allow(clippy::too_many_arguments)] // PyO3 exposes these as named Python arguments.
pub(crate) fn native_probe_install(
    app_class: &str,
    app_name: Option<String>,
    app_version: Option<String>,
    instance: Option<String>,
    socket_override: Option<String>,
    env_allowlist: Option<Vec<String>>,
    disclose_cwd: bool,
    enable_crash_handler: bool,
) -> PyResult<u64> {
    let config = build_config(
        app_class,
        app_name,
        app_version,
        instance,
        socket_override,
        env_allowlist,
        disclose_cwd,
        enable_crash_handler,
    );

    let guard = probe::install(config)
        .map_err(|e| PyRuntimeError::new_err(format!("probe install failed: {e}")))?;

    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    with_guards(|guards| guards.insert(handle, guard));
    Ok(handle)
}

/// Enable Python faulthandler beneath an already-armed native crash handler.
///
/// The first Python guard normally enables faulthandler before native
/// interception. A later guard may opt in after a native-only guard already
/// armed the process, though. In that case the native layer must briefly
/// detach and re-arm so faulthandler becomes its predecessor and survives
/// final native teardown.
#[pyfunction]
pub(crate) fn native_probe_enable_faulthandler(py: Python<'_>) -> PyResult<()> {
    match running_process_probe::crash::with_handler_suspended(|| {
        let module = PyModule::import(py, "faulthandler")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("all_threads", true)?;
        module.call_method("enable", (), Some(&kwargs))?;
        Ok(())
    }) {
        Ok(result) => result,
        Err(error) => Err(PyRuntimeError::new_err(format!(
            "cannot re-chain native crash handler around faulthandler: {error}"
        ))),
    }
}

/// Drop the guard for `handle`, deregistering this process.
///
/// Returns whether a guard was actually removed, so a double close is
/// observable rather than silently successful. Unknown handles are not an
/// error: teardown runs from `atexit` and must be idempotent.
#[pyfunction]
pub(crate) fn native_probe_uninstall(handle: u64) -> bool {
    // Taken out of the map first so the guard's Drop — which joins the worker
    // thread — runs after the lock is released. Joining while holding it would
    // block every other probe call for the duration.
    let guard = with_guards(|guards| guards.remove(&handle));
    guard.is_some()
}

/// Whether `handle` currently holds an armed registration.
///
/// False both before the first successful registration and while disconnected,
/// which is what lets a caller distinguish "enrolled" from "daemon reachable".
#[pyfunction]
pub(crate) fn native_probe_is_armed(handle: u64) -> bool {
    with_guards(|guards| guards.get(&handle).is_some_and(|g| g.is_armed()))
}

/// Capture the machine stacks of every other thread in this process.
///
/// Returns `{"modules": [{"name", "path", "base"}, ...],
///           "threads": {os_tid: [[module_index_or_None, offset], ...]}}`.
///
/// Frames are **module + offset**, not absolute addresses. An absolute address
/// is meaningless outside the process that produced it and outside the moment
/// it was captured, since the same build loads at a different base next time.
/// Module-relative frames survive both, which is what makes a capture
/// symbolizable later — and symbolization is the only reason to capture.
///
/// A frame whose address fell outside every loaded module has `None` for its
/// module index and keeps the raw address. It is not assigned to the nearest
/// module: a wrong attribution becomes a confident wrong function name that
/// nothing downstream can detect.
///
/// The calling thread is absent by construction: a thread cannot suspend
/// itself. In a Python process that means the interpreter thread running this
/// call contributes its Python frames (via `sys._current_frames()`) but no
/// native ones, which is why the Python layer merges the two views rather than
/// assuming every tid appears in both.
///
/// # The GIL
///
/// Released for the capture and the module enumeration. Capture suspends
/// sibling OS threads, and some of those threads hold the GIL — suspending a
/// GIL holder while this thread also wanted the GIL would deadlock the
/// interpreter.
#[pyfunction]
pub(crate) fn native_probe_snapshot(py: Python<'_>) -> PyResult<Py<PyAny>> {
    #[cfg(not(all(
        any(windows, target_os = "linux", target_os = "macos"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        // No capture backend and therefore no module inventory (#635).
        // Refuse instead of stubbing types that would have no meaning.
        let _ = py;
        Err(pyo3::exceptions::PyNotImplementedError::new_err(
            "native stack capture is not implemented on this platform",
        ))
    }

    #[cfg(all(
        any(windows, target_os = "linux", target_os = "macos"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        use running_process_probe::snapshot::{
            attribute::attribute, capture_and_resolve, modules::enumerate_modules, SnapshotConfig,
            SnapshotError,
        };

        let attributed = py
            .detach(|| -> Result<_, SnapshotError> {
                let snapshot = capture_and_resolve(&SnapshotConfig::default())?;
                // Enumerated in the same process, immediately after the
                // capture, because module bases describe this address space
                // and no other.
                let modules = enumerate_modules()?;
                Ok(attribute(&snapshot, &modules))
            })
            .map_err(|e| PyRuntimeError::new_err(format!("stack capture failed: {e}")))?;

        let modules = pyo3::types::PyList::empty(py);
        for module in &attributed.modules {
            let entry = pyo3::types::PyDict::new(py);
            entry.set_item("name", &module.name)?;
            entry.set_item("path", module.path.as_deref())?;
            entry.set_item("base", module.base)?;
            modules.append(entry)?;
        }

        let threads = pyo3::types::PyDict::new(py);
        for thread in &attributed.threads {
            let frames = pyo3::types::PyList::empty(py);
            for frame in &thread.frames {
                let pair = pyo3::types::PyTuple::new(
                    py,
                    [
                        frame.module_index.into_pyobject(py)?.into_any().unbind(),
                        frame
                            .relative_address
                            .into_pyobject(py)?
                            .into_any()
                            .unbind(),
                    ],
                )?;
                frames.append(pair)?;
            }
            threads.set_item(thread.os_tid, frames)?;
        }

        let out = pyo3::types::PyDict::new(py);
        out.set_item("modules", modules)?;
        out.set_item("threads", threads)?;
        Ok(out.into_any().unbind())
    }
}

/// Whether [`native_probe_snapshot`] is implemented on this platform.
///
/// Lets callers branch without provoking and catching an exception.
#[pyfunction]
pub(crate) fn native_probe_snapshot_supported() -> bool {
    cfg!(all(
        any(windows, target_os = "linux", target_os = "macos"),
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handle must survive until it is explicitly released.
    #[test]
    fn install_returns_a_handle_that_uninstalls_once() {
        // No daemon is running; install must still succeed, because an absent
        // daemon is a normal condition the worker retries through.
        let handle = native_probe_install(
            "test-class",
            None,
            None,
            None,
            Some("\\\\.\\pipe\\rp-probe-nonexistent".into()),
            None,
            true,
            false,
        )
        .expect("install must not fail merely because no daemon is running");

        assert!(
            native_probe_uninstall(handle),
            "the first uninstall releases the guard"
        );
        assert!(
            !native_probe_uninstall(handle),
            "a second uninstall must report that nothing was released"
        );
    }

    #[test]
    fn handles_are_distinct_and_independent() {
        let a = native_probe_install("a", None, None, None, None, None, false, true).unwrap();
        let b = native_probe_install("b", None, None, None, None, None, false, true).unwrap();
        assert_ne!(a, b, "handles must not alias");

        assert!(native_probe_uninstall(a));
        assert!(
            native_probe_uninstall(b),
            "releasing one handle must not release another"
        );
    }

    /// Nothing is armed without a daemon, so `is_armed` cannot be a constant.
    #[test]
    fn an_unknown_handle_is_not_armed() {
        assert!(!native_probe_is_armed(u64::MAX));
    }

    /// The point of this binding: a Python caller registers as Python.
    ///
    /// Asserted on the request that actually goes on the wire, not just on the
    /// config, so a `Config` field that stopped being read would still fail.
    #[test]
    fn a_python_caller_declares_the_python_runtime() {
        use running_process::probe::worker::build_register_request;
        use running_process_probe::probe_diag::v1::Runtime as ProtoRuntime;

        let config = build_config("app", None, None, None, None, None, false, true);
        assert_eq!(config.runtime, Runtime::Python);

        let request = build_register_request(&config).expect("build request");
        assert_eq!(
            request.runtime,
            ProtoRuntime::Python as i32,
            "a Python process must not register as native or unspecified"
        );
    }

    /// Env values stay deny-by-default unless a name is opted in.
    #[test]
    fn env_values_are_not_disclosed_by_default() {
        let bare = build_config("app", None, None, None, None, None, false, true);
        assert!(
            bare.disclosure.env_allowlist.is_empty(),
            "environments carry credentials; values must be opt-in"
        );
        assert!(!bare.disclosure.disclose_cwd);

        let opted = build_config(
            "app",
            None,
            None,
            None,
            None,
            Some(vec!["PATH".into()]),
            true,
            true,
        );
        assert_eq!(opted.disclosure.env_allowlist, vec!["PATH".to_string()]);
        assert!(opted.disclosure.disclose_cwd);
    }

    /// Optional arguments override the defaults derived from `app_class`.
    #[test]
    fn optional_fields_override_the_defaults() {
        let defaulted = build_config("myclass", None, None, None, None, None, false, true);
        assert_eq!(defaulted.app_class, "myclass");
        assert_eq!(
            defaulted.app_name, "myclass",
            "app_name defaults to the class"
        );
        assert_eq!(defaulted.instance, None);

        let explicit = build_config(
            "myclass",
            Some("friendly".into()),
            Some("9.9.9".into()),
            Some("worker-1".into()),
            None,
            None,
            false,
            true,
        );
        assert_eq!(explicit.app_name, "friendly");
        assert_eq!(explicit.app_version, "9.9.9");
        assert_eq!(explicit.instance.as_deref(), Some("worker-1"));
    }
}
