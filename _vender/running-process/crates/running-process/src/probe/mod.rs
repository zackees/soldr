//! Probe client facade (#633) — enroll this process with `rpprobed`.
//!
//! # No global constructors
//!
//! There is no `#[ctor]`, no `lazy_static` auto-init, and nothing in
//! `.init_array`. Registration happens only when the application calls
//! [`install`]. A build with the `probe` feature enabled but `install` never
//! called is completely inert, and a build without the feature does not
//! contain this module at all.
//!
//! # This talks to the daemon, it does not contain one
//!
//! The facade depends on `running-process-probe` solely for the
//! `probe_diag.v1` schema, with that crate's default features off — its
//! injection vehicles are gated behind `embed-helper`, which stays off. So
//! enabling `probe` adds no injection symbols to this crate, preserving the
//! #539 static-analysis invariant. It never depends on the probe *daemon*
//! crate; everything goes over IPC.
//!
//! ```no_run
//! # #[cfg(feature = "probe")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use running_process::probe;
//!
//! let _guard = probe::install(probe::Config::new("my-app"))?;
//! // ... application runs; the guard unregisters on drop.
//! # Ok(())
//! # }
//! ```

mod capture;
pub mod client;
pub mod worker;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use running_process_probe::crash::{self, spool::CrashMetadata};
pub use running_process_probe::crash::{spool::SPOOL_DIR_ENV, CrashPolicy};
use running_process_probe::probe_diag::v1::{ProcessKey, Runtime as ProtoRuntime};

/// How often the worker heartbeats. Matches the daemon's expectation; its
/// grace is three intervals, so a single missed beat is survivable.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// What this process permits the daemon to do.
#[derive(Clone, Debug)]
pub struct AllowPolicy {
    /// Whether all probe operations are permitted. Defaults to `true` —
    /// enrolling is itself the opt-in, so a registrant that took that step
    /// gets the full operation set unless it says otherwise.
    pub allow_all_ops: bool,
}

impl Default for AllowPolicy {
    fn default() -> Self {
        Self {
            allow_all_ops: true,
        }
    }
}

/// What this process discloses on query surfaces.
///
/// `env_allowlist` is empty by default: environment *values* are deny-by-
/// default because they routinely carry credentials. Opt individual names in
/// with [`Config::allow_env_value`].
#[derive(Clone, Debug, Default)]
pub struct Disclosure {
    /// Env var names whose values may be disclosed.
    pub env_allowlist: Vec<String>,
    /// Whether the working directory may be disclosed.
    pub disclose_cwd: bool,
}

/// Which language runtime this process is.
///
/// The daemon uses this to decide what a captured stack *means*. A native
/// process yields machine frames and nothing else; a Python process runs an
/// interpreter above those frames, so its stacks are mixed-mode and need the
/// interpreter half attached before they read as the program the operator
/// wrote. Declaring it at registration is what lets the daemon know which
/// treatment applies without inspecting the process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Runtime {
    /// Machine frames only.
    #[default]
    Native,
    /// CPython: machine frames plus interpreter frames above them.
    Python,
}

impl Runtime {
    /// The wire value for this runtime.
    fn to_proto(self) -> ProtoRuntime {
        match self {
            Self::Native => ProtoRuntime::Native,
            Self::Python => ProtoRuntime::Python,
        }
    }
}

/// Configuration for [`install`].
#[derive(Clone, Debug)]
pub struct Config {
    /// Coarse grouping for cross-instance queries (e.g. `"clud"`).
    pub app_class: String,
    /// Human-readable application name.
    pub app_name: String,
    /// Application version.
    pub app_version: String,
    /// Optional instance discriminator.
    pub instance: Option<String>,
    /// What the daemon may do.
    pub allow_policy: AllowPolicy,
    /// What is disclosed on query surfaces.
    pub disclosure: Disclosure,
    /// Override the discovered control-socket path (tests, unusual layouts).
    pub socket_override: Option<PathBuf>,
    /// Heartbeat cadence.
    pub heartbeat_interval: Duration,
    /// Language runtime to report. Defaults to [`Runtime::Native`]; the Python
    /// client sets [`Runtime::Python`].
    pub runtime: Runtime,
    /// Native crash interception. Defaults to [`CrashPolicy::On`].
    pub crash_policy: CrashPolicy,
    /// Optional process-level symbol manifest consumed only by the isolated
    /// symbolization worker.
    pub symbol_manifest_path: Option<PathBuf>,
    /// Explicit symbol files or directories offered to the isolated worker.
    ///
    /// Every candidate is identity-checked; a same-named file from another
    /// build is rejected.
    pub symbol_paths: Vec<PathBuf>,
}

impl Config {
    /// Config for `app_class`, with permissive ops and deny-by-default env
    /// values.
    pub fn new(app_class: impl Into<String>) -> Self {
        let app_class = app_class.into();
        Self {
            app_name: app_class.clone(),
            app_class,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            instance: None,
            allow_policy: AllowPolicy::default(),
            disclosure: Disclosure::default(),
            socket_override: None,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            runtime: Runtime::Native,
            crash_policy: CrashPolicy::On,
            symbol_manifest_path: None,
            symbol_paths: Vec::new(),
        }
    }

    /// Permit the daemon to disclose the value of environment variable `name`.
    pub fn allow_env_value(mut self, name: impl Into<String>) -> Self {
        self.disclosure.env_allowlist.push(name.into());
        self
    }

    /// Set the application version.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.app_version = version.into();
        self
    }

    /// Set the instance discriminator.
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Declare the language runtime this process is.
    pub fn with_runtime(mut self, runtime: Runtime) -> Self {
        self.runtime = runtime;
        self
    }

    /// Select native crash interception policy.
    pub fn crash_policy(mut self, policy: CrashPolicy) -> Self {
        self.crash_policy = policy;
        self
    }

    /// Declare a process-level symbol manifest.
    pub fn with_symbol_manifest(mut self, path: impl Into<PathBuf>) -> Self {
        self.symbol_manifest_path = Some(path.into());
        self
    }

    /// Offer a symbol file or directory to the isolated worker.
    pub fn with_symbol_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.symbol_paths.push(path.into());
        self
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new("unknown")
    }
}

/// Why [`install`] failed.
///
/// Only *local* failures appear here. An unreachable daemon is not one of
/// them: `install` returns successfully and the worker keeps retrying, so a
/// missing daemon never breaks the application.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// The current executable could not be identified.
    #[error("cannot determine current executable: {0}")]
    CurrentExe(#[source] std::io::Error),
    /// The worker thread could not be spawned.
    #[error("cannot spawn probe worker thread: {0}")]
    Spawn(#[source] std::io::Error),
    /// Crash interception could not be prepared locally.
    #[error("cannot arm crash capture: {0}")]
    Crash(#[source] crash::InstallError),
}

/// Handle returned by [`install`]. Dropping it deregisters.
///
/// Deregistration is best-effort. The daemon's real liveness signal is the
/// connection closing, which happens whether or not this runs — so a crash is
/// detected just as reliably as a clean exit.
pub struct Guard {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    key: Arc<Mutex<Option<ProcessKey>>>,
    crash: crash::CrashGuard,
}

impl Guard {
    /// The armed identity, once the worker has registered.
    ///
    /// `None` before the first successful registration, or while
    /// disconnected.
    pub fn armed_key(&self) -> Option<ProcessKey> {
        self.key.lock().ok().and_then(|k| k.clone())
    }

    /// Whether the worker currently holds an armed registration.
    pub fn is_armed(&self) -> bool {
        self.armed_key().is_some()
    }

    /// Whether native crash interception is armed.
    pub fn crash_handler_armed(&self) -> bool {
        self.crash.is_armed()
    }

    /// Whether at least one bounded all-thread pre-crash sample is ready.
    pub fn crash_sample_ready(&self) -> bool {
        self.crash.sample_ready()
    }

    /// Thread count in the latest bounded pre-crash sample.
    pub fn crash_sample_thread_count(&self) -> usize {
        self.crash.sample_thread_count()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // The worker polls `stop` between bounded operations, so this
            // returns promptly. It is never an unbounded wait on the daemon.
            let _ = handle.join();
        }
    }
}

/// Enroll this process with the probe daemon.
///
/// Daemon I/O — discovery, connect, register, heartbeat, reconnect — happens
/// on a background thread, so an absent or wedged daemon cannot slow startup.
/// Native crash arming is deliberately synchronous: before this returns it
/// creates and opens the owner-private local spool, attaches the platform
/// handler, and starts the bounded sampler. That local readiness boundary is
/// what makes a crash immediately after `install()` reportable.
pub fn install(config: Config) -> Result<Guard, InstallError> {
    let creation_time_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut request = worker::build_register_request(&config).map_err(InstallError::CurrentExe)?;
    if let Some(key) = request.key.as_mut() {
        key.start_time = Some(creation_time_ms);
    }
    let crash = crash::install(
        config.crash_policy,
        CrashMetadata {
            app_class: config.app_class.clone(),
            app_name: config.app_name.clone(),
            app_version: config.app_version.clone(),
            instance_name: config.instance.clone().unwrap_or_default(),
            creation_time_ms,
            cwd,
        },
    )
    .map_err(InstallError::Crash)?;

    let stop = Arc::new(AtomicBool::new(false));
    let key = Arc::new(Mutex::new(None));

    let handle = worker::spawn(request, config, Arc::clone(&stop), Arc::clone(&key))
        .map_err(InstallError::Spawn)?;

    Ok(Guard {
        stop,
        handle: Some(handle),
        key,
        crash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_values_are_deny_by_default() {
        let cfg = Config::new("app");
        assert!(
            cfg.disclosure.env_allowlist.is_empty(),
            "env values must be deny-by-default"
        );
    }

    #[test]
    fn env_values_are_opt_in_by_name() {
        let cfg = Config::new("app").allow_env_value("PATH");
        assert_eq!(cfg.disclosure.env_allowlist, vec!["PATH".to_string()]);
    }

    #[test]
    fn ops_are_permitted_by_default() {
        assert!(Config::new("app").allow_policy.allow_all_ops);
        assert_eq!(Config::new("app").crash_policy, CrashPolicy::On);
    }

    #[test]
    fn builder_sets_version_and_instance() {
        let cfg = Config::new("app")
            .with_version("9.9")
            .with_instance("i-1")
            .with_symbol_manifest("app.symbols.json")
            .with_symbol_path("symbols");
        assert_eq!(cfg.app_version, "9.9");
        assert_eq!(cfg.instance.as_deref(), Some("i-1"));
        assert_eq!(
            cfg.symbol_manifest_path.as_deref(),
            Some(std::path::Path::new("app.symbols.json"))
        );
        assert_eq!(cfg.symbol_paths, vec![PathBuf::from("symbols")]);
    }

    /// The headline requirement: install must not block on a missing daemon.
    #[test]
    fn install_returns_immediately_with_no_daemon_present() {
        let mut cfg = Config::new("probe-install-timing");
        // Point at a path nothing is listening on.
        cfg.socket_override = Some(PathBuf::from(if cfg!(windows) {
            r"\\.\pipe\rp-probe-absent-633"
        } else {
            "/tmp/rp-probe-absent-633.sock"
        }));

        let start = std::time::Instant::now();
        let guard = install(cfg).expect("install must succeed even with no daemon");
        let elapsed = start.elapsed();

        // Budget deliberately loose. What this asserts is that `install` does
        // no blocking I/O on the calling thread — a daemon connect that waited
        // on a timeout would take seconds, and this catches that with room to
        // spare.
        //
        // It was 50ms, which failed under a full `--workspace` run (47 test
        // binaries in parallel) and passed in isolation, both nextest retries
        // included. At that tightness the number measures how promptly the OS
        // scheduled this thread, not the property under test, so a loaded CI
        // runner turns a correct implementation red. A flaky assertion about
        // the right thing is worse than a loose one: it trains people to
        // re-run rather than read.
        assert!(
            elapsed < Duration::from_millis(500),
            "install took {elapsed:?}; it must not perform I/O on the calling thread"
        );
        // Not armed — but that is the worker's problem, not the caller's.
        assert!(!guard.is_armed());
        drop(guard);
    }

    /// Dropping must not hang when the daemon was never reachable.
    #[test]
    fn guard_drop_returns_promptly_without_a_daemon() {
        let mut cfg = Config::new("probe-drop-timing");
        cfg.socket_override = Some(PathBuf::from(if cfg!(windows) {
            r"\\.\pipe\rp-probe-absent-633b"
        } else {
            "/tmp/rp-probe-absent-633b.sock"
        }));
        let guard = install(cfg).unwrap();

        let start = std::time::Instant::now();
        drop(guard);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "Guard::drop must not block on an absent daemon"
        );
    }
}
