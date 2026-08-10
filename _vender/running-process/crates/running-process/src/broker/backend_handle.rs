//! Public handle for a verified backend daemon.
//!
//! `BackendHandle` is the shared probe-and-verify abstraction for broker-managed
//! daemons and direct-daemon consumers. A cache manifest records where a daemon
//! is listening and which process identity it claimed when the manifest was
//! written. Probing turns that persisted identity into an owned handle only
//! after the endpoint tuple, active IPC response, current boot ID, process
//! liveness, executable path, and executable digest still match.
//!
//! Consumers should use this module at the boundary where they would otherwise
//! trust a manifest, PID file, socket path, or named-pipe path from disk.
//!
//! ```
//! use running_process::broker::backend_handle::BackendHandle;
//! use running_process::broker::protocol::CacheManifest;
//!
//! fn existing_backend(manifest: &CacheManifest) -> Option<BackendHandle> {
//!     let handle = BackendHandle::probe_manifest(manifest)?;
//!     handle.is_alive().then_some(handle)
//! }
//! ```
//!
//! Direct-daemon consumers that just spawned a backend can persist
//! [`DaemonProcess`] and later probe it without duplicating the liveness and
//! executable-hash checks:
//!
//! ```no_run
//! use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
//! use running_process::broker::protocol::Endpoint;
//!
//! # fn example() -> running_process::broker::backend_handle::Result<()> {
//! let endpoint = Endpoint {
//!     namespace_id: "local-dev".to_owned(),
//!     path: "running-process-example.sock".to_owned(),
//! };
//! let daemon = DaemonProcess::current_process(endpoint.clone(), Some(300))?;
//!
//! let handle =
//!     BackendHandle::probe_with_service("soldr", "1.2.3", &endpoint, &daemon)?;
//! assert_eq!(handle.service_name, "soldr");
//! # Ok(())
//! # }
//! ```

use std::io;
use std::time::{Duration, Instant};

use crate::broker::backend_lifecycle::identity::IdentityError;
use crate::broker::backend_lifecycle::probe::{self, ProbeError};
use crate::broker::backend_lifecycle::verify_pid::{self, ProcessHandle, VerifyPidError};
use crate::broker::protocol::{CacheManifest, Endpoint};

pub use crate::broker::backend_lifecycle::DaemonProcess;

/// Result type returned by backend-handle operations.
pub type Result<T> = std::result::Result<T, BackendHandleError>;

/// A verified handle to a running backend daemon.
///
/// The handle carries the daemon identity needed to defend against stale
/// manifests and PID recycling before consumers connect to the IPC endpoint.
///
/// A handle is created only through one of the `probe*` constructors. The
/// constructor performs all identity checks first; successful callers may then
/// use [`Self::is_alive`] for a cheap liveness check or [`Self::connect`] to
/// open a fresh local-socket connection.
pub struct BackendHandle {
    /// Logical service name from the manifest or direct probe caller.
    pub service_name: String,
    /// Service version from the manifest or direct probe caller.
    pub service_version: String,
    /// Verified daemon process identity.
    pub daemon_process: DaemonProcess,
    #[cfg(unix)]
    pub(crate) pid_handle: Option<ProcessHandle>,
    #[cfg(windows)]
    pub(crate) process_handle: Option<ProcessHandle>,
}

impl BackendHandle {
    /// Connect to an existing backend by endpoint and verify process identity.
    ///
    /// This probe verifies the endpoint identity tuple, requires the endpoint
    /// to answer the nonce-based IPC identity probe, then verifies current boot
    /// ID, process liveness, executable path, and executable SHA-256. It
    /// returns `None` for stale manifests, dead PIDs, mismatched daemon
    /// binaries, or endpoints that do not answer as the expected backend.
    ///
    /// Use this when the caller already has service metadata elsewhere and only
    /// needs to know whether the daemon identity is still valid.
    ///
    /// **BLOCKING.** Performs synchronous IPC up to
    /// [`probe::DEFAULT_ENDPOINT_PROBE_TIMEOUT`]
    /// (500 ms). From a tokio task, call from `spawn_blocking` or
    /// switch to `Self::probe_async` (requires the `client-async`
    /// feature).
    ///
    /// ```no_run
    /// use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
    /// use running_process::broker::protocol::Endpoint;
    ///
    /// # fn example(endpoint: Endpoint, expected: DaemonProcess) {
    /// if let Some(handle) = BackendHandle::probe(&endpoint, &expected) {
    ///     assert!(handle.is_alive());
    /// }
    /// # }
    /// ```
    pub fn probe(endpoint: &Endpoint, expected: &DaemonProcess) -> Option<Self> {
        Self::probe_with_service("", "", endpoint, expected).ok()
    }

    /// Async counterpart of [`Self::probe`] (#414).
    ///
    /// Performs the same identity checks but all I/O runs on the
    /// current tokio runtime, so tokio daemons (zccache, soldr, clud)
    /// can call this directly instead of wrapping in `spawn_blocking`.
    ///
    /// Available when the `client-async` cargo feature is enabled.
    #[cfg(feature = "client-async")]
    pub async fn probe_async(endpoint: &Endpoint, expected: &DaemonProcess) -> Option<Self> {
        Self::probe_with_service_async("", "", endpoint, expected)
            .await
            .ok()
    }

    /// Probe an existing backend and attach service metadata to the handle.
    ///
    /// This is the preferred constructor for direct-daemon consumers because it
    /// preserves the logical service tuple alongside the verified process
    /// identity.
    ///
    /// **BLOCKING.** Performs synchronous IPC up to
    /// [`probe::DEFAULT_ENDPOINT_PROBE_TIMEOUT`]
    /// (500 ms). From a tokio task, call from `spawn_blocking` or use
    /// `Self::probe_with_service_async` (requires the
    /// `client-async` feature) instead — calling this directly from
    /// an async context will block the runtime worker thread.
    ///
    /// ```no_run
    /// use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
    /// use running_process::broker::protocol::Endpoint;
    ///
    /// # fn example(endpoint: Endpoint, expected: DaemonProcess)
    /// #     -> running_process::broker::backend_handle::Result<BackendHandle>
    /// # {
    /// BackendHandle::probe_with_service("zccache", "0.8.0", &endpoint, &expected)
    /// # }
    /// ```
    pub fn probe_with_service(
        service_name: impl Into<String>,
        service_version: impl Into<String>,
        endpoint: &Endpoint,
        expected: &DaemonProcess,
    ) -> Result<Self> {
        let process_handle = probe::probe_endpoint(endpoint, expected)?;
        Ok(Self::from_verified(
            service_name.into(),
            service_version.into(),
            expected.clone(),
            process_handle,
        ))
    }

    /// Async counterpart of [`Self::probe_with_service`] (#414).
    ///
    /// Performs the same identity checks (endpoint tuple, PID, exe
    /// path, exe SHA-256, boot ID, and the live nonce probe) but all
    /// I/O runs on the current tokio runtime. This is the preferred
    /// entry point for tokio daemons (zccache, soldr, clud) — calling
    /// the blocking [`Self::probe_with_service`] from an async
    /// context blocks the runtime worker thread.
    ///
    /// Available when the `client-async` cargo feature is enabled.
    ///
    /// ```no_run
    /// # #[cfg(feature = "client-async")]
    /// # async fn example(
    /// #     endpoint: running_process::broker::protocol::Endpoint,
    /// #     expected: running_process::broker::backend_handle::DaemonProcess,
    /// # ) -> running_process::broker::backend_handle::Result<()> {
    /// use running_process::broker::backend_handle::BackendHandle;
    ///
    /// let handle = BackendHandle::probe_with_service_async(
    ///     "zccache", "0.8.0", &endpoint, &expected,
    /// ).await?;
    /// assert!(handle.is_alive());
    /// # Ok(()) }
    /// ```
    #[cfg(feature = "client-async")]
    pub async fn probe_with_service_async(
        service_name: impl Into<String>,
        service_version: impl Into<String>,
        endpoint: &Endpoint,
        expected: &DaemonProcess,
    ) -> Result<Self> {
        let process_handle =
            crate::broker::backend_lifecycle::probe_async::probe_endpoint_async(endpoint, expected)
                .await?;
        Ok(Self::from_verified(
            service_name.into(),
            service_version.into(),
            expected.clone(),
            process_handle,
        ))
    }

    /// Probe the `current_daemon` recorded in a cache manifest.
    ///
    /// Returns `None` when the manifest has no daemon entry or when the daemon
    /// entry no longer matches a live process on the current boot.
    ///
    /// ```
    /// use running_process::broker::backend_handle::BackendHandle;
    /// use running_process::broker::protocol::CacheManifest;
    ///
    /// # fn example(manifest: &CacheManifest) {
    /// match BackendHandle::probe_manifest(manifest) {
    ///     Some(handle) if handle.is_alive() => {
    ///         // Reuse the verified backend.
    ///     }
    ///     _ => {
    ///         // Spawn or discover a replacement backend.
    ///     }
    /// }
    /// # }
    /// ```
    pub fn probe_manifest(manifest: &CacheManifest) -> Option<Self> {
        Self::try_from_manifest(manifest).ok().flatten()
    }

    /// Fallible variant of [`Self::probe_manifest`] that preserves parse errors.
    ///
    /// Use this in maintenance tools and diagnostics where malformed manifest
    /// identities should be reported separately from a normal cache miss.
    pub fn try_from_manifest(manifest: &CacheManifest) -> Result<Option<Self>> {
        let Some(daemon_process) = DaemonProcess::from_manifest_current_daemon(manifest)? else {
            return Ok(None);
        };
        let handle = Self::probe_with_service(
            manifest.service_name.clone(),
            manifest.service_version.clone(),
            &daemon_process.ipc_endpoint,
            &daemon_process,
        )?;
        Ok(Some(handle))
    }

    /// Check liveness without opening a new IPC connection.
    ///
    /// On platforms with an owned process-handle primitive, this checks the
    /// handle captured during probing. Otherwise it falls back to opening the
    /// process ID again.
    pub fn is_alive(&self) -> bool {
        self.platform_handle()
            .map(|handle| handle.is_alive())
            .unwrap_or_else(|| verify_pid::process_is_alive(self.daemon_process.pid))
    }

    /// Open a fresh IPC connection to this backend.
    ///
    /// The process identity is verified when the handle is created. Callers that
    /// cache handles for a long time should call [`Self::is_alive`] or reprobe
    /// from the latest manifest before opening a connection.
    ///
    /// ```no_run
    /// use running_process::broker::backend_handle::BackendHandle;
    ///
    /// async fn connect_to_verified_backend(
    ///     handle: &BackendHandle,
    /// ) -> running_process::broker::backend_handle::Result<()> {
    ///     let connection = handle.connect().await?;
    ///     let _stream = connection.into_inner();
    ///     Ok(())
    /// }
    /// ```
    pub async fn connect(&self) -> Result<Connection> {
        Connection::connect(&self.daemon_process.ipc_endpoint).map_err(BackendHandleError::Connect)
    }

    /// Duplicate a broker-owned pipe handle into this verified backend process.
    ///
    /// This is the Windows bridge between `BackendHandle` identity verification
    /// and the optional Phase 6 `DuplicateHandle` transport. The caller still
    /// owns delivery of the paired handoff token to the backend and must wait
    /// for backend acknowledgement before reporting handoff success.
    #[cfg(windows)]
    pub fn try_duplicate_windows_handoff_handle(
        &self,
        pipe_handle: crate::broker::server::handoff::WindowsHandleValue,
        handoff_token: crate::broker::server::handoff::HandoffToken,
    ) -> crate::broker::server::handoff::DuplicateHandleResult {
        let attempt = crate::broker::server::handoff::DuplicateHandleAttempt::new(
            pipe_handle,
            self.daemon_process.pid,
            handoff_token,
        );
        crate::broker::server::handoff::try_duplicate_handle(&attempt)
    }

    /// Send a graceful shutdown signal and wait until the process exits.
    ///
    /// On Windows this foundation returns `GracefulTerminateUnsupported` until
    /// the broker shutdown request protocol lands.
    ///
    /// Dropping the handle without calling this method leaves the backend
    /// running.
    pub async fn shutdown(self, timeout: Duration) -> Result<()> {
        verify_pid::signal_terminate(self.daemon_process.pid)?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.is_alive() {
                // The broker asked for this daemon to stop and watched it
                // stop, so the endpoint it was serving is now a dead name.
                // Nothing else will remove it: a daemon that is signalled
                // does not run its own cleanup, and under broker-owned bind
                // the adopted listener carries no reclaim guard at all.
                //
                // Stale sockets are not inert here — #519 recorded them
                // masking real failures as `EADDRINUSE` on bind or
                // `ECONNREFUSED` on connect.
                remove_endpoint_socket(&self.daemon_process.ipc_endpoint);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(BackendHandleError::ShutdownTimeout {
            pid: self.daemon_process.pid,
        })
    }

    /// Force-kill the daemon process.
    ///
    /// This is the last-resort teardown path for a daemon that ignored graceful
    /// shutdown or whose IPC protocol is unavailable.
    pub fn force_kill(self) -> Result<()> {
        verify_pid::force_kill_pid(self.daemon_process.pid)?;
        Ok(())
    }

    fn from_verified(
        service_name: String,
        service_version: String,
        daemon_process: DaemonProcess,
        process_handle: ProcessHandle,
    ) -> Self {
        #[cfg(unix)]
        {
            Self {
                service_name,
                service_version,
                daemon_process,
                pid_handle: Some(process_handle),
            }
        }

        #[cfg(windows)]
        {
            Self {
                service_name,
                service_version,
                daemon_process,
                process_handle: Some(process_handle),
            }
        }
    }

    fn platform_handle(&self) -> Option<&ProcessHandle> {
        #[cfg(unix)]
        {
            self.pid_handle.as_ref()
        }

        #[cfg(windows)]
        {
            self.process_handle.as_ref()
        }
    }
}

/// A fresh IPC connection to a verified backend daemon.
///
/// `Connection` is intentionally thin: `BackendHandle` owns identity and
/// liveness, while this type owns a single local-socket stream opened from the
/// verified endpoint.
pub struct Connection {
    stream: interprocess::local_socket::Stream,
}

impl Connection {
    /// Connect to a backend endpoint using the platform local-socket name type.
    pub fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        if endpoint.path.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "backend endpoint path is empty",
            ));
        }
        let name = endpoint_name(&endpoint.path)?;

        use interprocess::local_socket::traits::Stream as _;
        let stream = interprocess::local_socket::Stream::connect(name)?;
        Ok(Self { stream })
    }

    /// Return the underlying `interprocess` stream.
    pub fn into_inner(self) -> interprocess::local_socket::Stream {
        self.stream
    }
}

/// Errors returned by `BackendHandle`.
#[derive(Debug, thiserror::Error)]
pub enum BackendHandleError {
    /// Daemon identity normalization failed.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// Endpoint/process probing failed.
    #[error(transparent)]
    Probe(#[from] ProbeError),
    /// Opening an IPC connection failed.
    #[error("backend IPC connection failed: {0}")]
    Connect(io::Error),
    /// Process verification or signalling failed.
    #[error(transparent)]
    VerifyPid(#[from] VerifyPidError),
    /// Graceful shutdown timed out.
    #[error("backend shutdown timed out for pid {pid}")]
    ShutdownTimeout {
        /// Process ID that did not exit before the timeout.
        pid: u32,
    },
}

fn endpoint_name(path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    use interprocess::local_socket::prelude::*;

    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        path.to_fs_name::<GenericFilePath>()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        path.to_ns_name::<GenericNamespaced>()
    }
}

/// Remove the socket file backing `endpoint`, if there is one.
///
/// Absence is success: a daemon that exited cleanly on its own may already
/// have reclaimed the name, and racing it is not an error.
///
/// Deliberately not called from [`BackendHandle::force_kill`]. That path
/// signals and returns without confirming the process is gone, so removing
/// the name there could unlink the socket of a daemon that is still serving —
/// turning a failed kill into an unreachable-but-live backend, which is worse
/// than a stale file.
#[cfg(unix)]
fn remove_endpoint_socket(endpoint: &Endpoint) {
    let _ = std::fs::remove_file(&endpoint.path);
}

/// A Windows named pipe has no directory entry — it disappears with its last
/// handle — so there is nothing to remove.
#[cfg(not(unix))]
fn remove_endpoint_socket(_endpoint: &Endpoint) {}

#[cfg(all(test, unix))]
mod endpoint_socket_tests {
    use super::*;

    fn endpoint_at(path: &std::path::Path) -> Endpoint {
        Endpoint {
            namespace_id: "shared".into(),
            path: path.display().to_string(),
        }
    }

    #[test]
    fn the_socket_file_is_removed() {
        // The property `shutdown` depends on: once the daemon is confirmed
        // gone, its endpoint name goes too. Stale sockets are not inert —
        // #519 recorded them masking real failures as EADDRINUSE on bind and
        // ECONNREFUSED on connect.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("endpoint.sock");
        std::fs::write(&path, b"").expect("create the stand-in socket");
        assert!(path.exists(), "precondition: the file exists");

        remove_endpoint_socket(&endpoint_at(&path));

        assert!(!path.exists(), "the endpoint name outlived its daemon");
    }

    #[test]
    fn an_already_removed_socket_is_not_an_error() {
        // A daemon that exited cleanly may have reclaimed the name first.
        // Racing it is normal, not a failure — and this function has no way
        // to report one, so the test exists to pin that it does not panic.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("never-existed.sock");
        assert!(!path.exists(), "precondition: nothing to remove");

        remove_endpoint_socket(&endpoint_at(&path));
    }

    #[test]
    fn a_directory_at_the_endpoint_path_is_left_alone() {
        // `remove_file` will not remove a directory, which is the behaviour
        // wanted: an endpoint path that is somehow a directory is a broken
        // assumption elsewhere, and quietly deleting a tree to satisfy
        // cleanup would turn that into data loss.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("surprise-directory");
        std::fs::create_dir(&path).expect("create the directory");

        remove_endpoint_socket(&endpoint_at(&path));

        assert!(path.is_dir(), "cleanup removed a directory");
    }
}
