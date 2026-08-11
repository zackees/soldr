//! soldr-daemon integration with `running-process` `BackendHandle`.
//!
//! `running-process` commit 04f6387 added the active endpoint-response probe
//! required by zackees/running-process#232. soldr now uses that probe at the
//! old `daemon::lifecycle::is_live` boundary: the PID file is only trusted
//! after `BackendHandle::probe_with_service` verifies process identity and the
//! live daemon answers the broker-v1 BackendHandle nonce challenge on its IPC
//! endpoint.

use crate::cache_lib::daemon_pid_path;
use crate::core::SoldrPaths;
#[cfg(unix)]
use crate::daemon::client;
use crate::daemon::lifecycle::{pid_exe_stem_matches, pid_is_alive, read_pid_file};
use crate::daemon::protocol::PROTOCOL_VERSION;
use running_process::broker::backend_handle::{BackendHandle, BackendHandleError, DaemonProcess};
use running_process::broker::backend_lifecycle::identity::IdentityError;
use running_process::broker::backend_lifecycle::probe::{EndpointProbeError, ProbeError};
use running_process::broker::backend_lifecycle::verify_pid::VerifyPidError;
use running_process::broker::backend_sdk::{BackendEndpointMux, LegacyClassification};
use running_process::broker::host_identity;
use running_process::broker::protocol::Endpoint;
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const LEGACY_FRAME_HEADER_BYTES: usize = 8;

/// How long to keep retrying a liveness probe that never reached a verdict
/// (soldr#1893).
///
/// `running-process` gives the probe a hardcoded 500 ms deadline covering
/// connect + write + read, which a machine running a large parallel test suite
/// can exceed while the daemon is perfectly healthy. This budget allows a few
/// more attempts before we conclude the daemon is not there.
///
/// Only *inconclusive* outcomes consume it — a definitive "no daemon" returns
/// on the first attempt, so the common cold-start path is unaffected.
pub(crate) const PROBE_INCONCLUSIVE_RETRY_BUDGET: Duration = Duration::from_secs(2);

/// Pause between inconclusive probe attempts. Small relative to the 500 ms
/// probe deadline that dominates each attempt.
pub(crate) const PROBE_INCONCLUSIVE_RETRY_BACKOFF: Duration = Duration::from_millis(50);

/// Stable v2 broker `--program` namespace. Every Soldr root and version shares
/// this singleton broker; the requested service name partitions backend
/// routes by canonical root, Soldr version, and daemon image digest.
pub const SOLDR_DAEMON_SERVICE_NAME: &str = "soldr-daemon";
pub const SOLDR_DAEMON_SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Env override for the broker `--program` namespace. Test-only in practice:
/// production uses one stable per-user broker program while service names
/// partition default and explicit roots into distinct daemon routes.
///
/// The override is honored by both broker spawn and SESSION clients through
/// this single resolver, which keeps isolated tests on one bind namespace.
pub const SOLDR_BROKER_PROGRAM_ENV_VAR: &str = "SOLDR_BROKER_PROGRAM";
pub const SOLDR_BROKER_SERVICE_ENV_VAR: &str = "SOLDR_BROKER_SERVICE";
pub const SOLDR_DAEMON_IMAGE_SHA256_LABEL: &str = "soldr-image-sha256";

/// Resolve the singleton broker namespace: the explicit test override when
/// present, otherwise the stable `soldr-daemon` program name.
pub fn broker_program() -> String {
    std::env::var(SOLDR_BROKER_PROGRAM_ENV_VAR)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| SOLDR_DAEMON_SERVICE_NAME.to_string())
}

pub fn broker_service_name_for(paths: &SoldrPaths, daemon_binary: &Path) -> io::Result<String> {
    broker_route_identity(paths, daemon_binary).map(|identity| identity.service_name)
}

pub struct BrokerRouteIdentity {
    pub service_name: String,
    pub image_sha256: String,
}

pub fn broker_route_identity(
    paths: &SoldrPaths,
    daemon_binary: &Path,
) -> io::Result<BrokerRouteIdentity> {
    // The image-hash cache below creates this root anyway. Create it before
    // canonicalizing so a symlinked existing ancestor (notably macOS /var ->
    // /private/var) cannot make the first route use the lexical path and the
    // second route use a different canonical path.
    std::fs::create_dir_all(&paths.root)?;
    let normalized = std::fs::canonicalize(&paths.root).unwrap_or_else(|_| {
        if paths.root.is_absolute() {
            paths.root.clone()
        } else {
            std::env::current_dir()
                .unwrap_or_default()
                .join(&paths.root)
        }
    });
    let identity = normalized.to_string_lossy().into_owned();
    #[cfg(windows)]
    let identity = identity.replace('\\', "/").to_ascii_lowercase();
    // soldr#2442 / 0.9.0: blake3 (via zccache's shared hasher) with a
    // (path,size,mtime) cache, replacing the whole-file SHA-256 read that made
    // cold daemon-image placement slow. The digest is a hex string on both this
    // registration side and the broker's verification side, so the label
    // matches.
    let image_hash =
        super::image_hash::cached_blake3_hex(&paths.cache.join("image-hash"), daemon_binary)?;
    let mut hasher = Sha256::new();
    hasher.update(identity.as_bytes());
    hasher.update([0]);
    hasher.update(SOLDR_DAEMON_SERVICE_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(image_hash.as_bytes());
    let digest = hasher.finalize();
    Ok(BrokerRouteIdentity {
        service_name: format!("{SOLDR_DAEMON_SERVICE_NAME}-{}", hex::encode(&digest[..16])),
        image_sha256: image_hash,
    })
}

pub fn broker_service_name() -> io::Result<String> {
    if let Some(service) = std::env::var(SOLDR_BROKER_SERVICE_ENV_VAR)
        .ok()
        .filter(|value| !value.is_empty())
    {
        return Ok(service);
    }
    let paths = SoldrPaths::new().map_err(|err| io::Error::other(err.to_string()))?;
    let current = std::env::current_exe()?;
    let daemon = current
        .parent()
        .map(|parent| {
            parent.join(if cfg!(windows) {
                "soldr-daemon.exe"
            } else {
                "soldr-daemon"
            })
        })
        .unwrap_or_else(|| PathBuf::from("soldr-daemon"));
    broker_service_name_for(&paths, &daemon)
}
pub(crate) const RUNNING_PROCESS_BACKEND_HANDLE_STATUS: RunningProcessBackendHandleStatus =
    RunningProcessBackendHandleStatus {
        crate_name: "running-process",
        // Tracks the published crates.io release the soldr-side adoption is
        // anchored against. Bumped to 4.4.0 in #726 so the new `backend_sdk`
        // module (`BackendEndpointMux`, `FrameClient`, identity-file helpers)
        // is in scope; keep this in lockstep with `Cargo.toml`.
        dependency_source: "crates.io:running-process@4.4.0",
        required_symbol: "running_process::broker::backend_handle::BackendHandle",
        running_process_issue: "zackees/running-process#232",
        adoption_tracker_issue: "zackees/running-process#242",
        soldr_issue: "zackees/soldr#718",
        active_endpoint_probe: true,
        remaining_gate: "none; backend_sdk BackendEndpointMux adopted for the probe handler",
    };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunningProcessBackendHandleStatus {
    pub(crate) crate_name: &'static str,
    pub(crate) dependency_source: &'static str,
    pub(crate) required_symbol: &'static str,
    pub(crate) running_process_issue: &'static str,
    pub(crate) adoption_tracker_issue: &'static str,
    pub(crate) soldr_issue: &'static str,
    pub(crate) active_endpoint_probe: bool,
    pub(crate) remaining_gate: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SoldrDaemonBackendHandle {
    pub(crate) service_name: &'static str,
    pub(crate) service_version: &'static str,
    pub(crate) protocol_version: u32,
    pub(crate) pid: u32,
    pub(crate) exe_path: PathBuf,
    pub(crate) endpoint: PathBuf,
    pub(crate) pid_file: PathBuf,
    pub(crate) adoption_status: RunningProcessBackendHandleStatus,
}

impl SoldrDaemonBackendHandle {
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn is_alive(&self) -> bool {
        pid_is_alive(self.pid) && pid_exe_stem_matches(self.pid, SOLDR_DAEMON_SERVICE_NAME)
    }
}

/// Outcome of a single `BackendHandle` probe attempt.
///
/// The distinction that matters is [`ProbeOutcome::NotLive`] versus
/// [`ProbeOutcome::Inconclusive`]: the first is an answer, the second is the
/// absence of one. Collapsing them (soldr#1893) made a slow daemon
/// indistinguishable from a dead one.
enum ProbeOutcome {
    Live(Box<SoldrDaemonBackendHandle>),
    /// The daemon is genuinely absent, or is not one this build can adopt.
    NotLive,
    /// The probe never reached a verdict — it timed out or hit a transient OS
    /// fault. Says nothing about whether the daemon is alive.
    Inconclusive(BackendHandleError),
}

/// True when `err` means "the probe did not get an answer" rather than
/// "the answer was no".
///
/// `running-process` gives the connect timeout and an absent endpoint the same
/// [`EndpointProbeError::Connect`] variant, so the inner [`io::ErrorKind`] is
/// what separates them — `TimedOut` is transient, `NotFound` /
/// `ConnectionRefused` are definitive.
fn probe_error_is_transient(err: &BackendHandleError) -> bool {
    fn io_kind_is_transient(err: &io::Error) -> bool {
        matches!(
            err.kind(),
            io::ErrorKind::TimedOut
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::Interrupted
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
        )
    }

    let BackendHandleError::Probe(probe) = err else {
        return false;
    };
    match probe {
        ProbeError::EndpointResponse(endpoint) => match endpoint {
            // The whole-probe deadline (500 ms) expired. Under a loaded
            // machine this says nothing about the daemon.
            EndpointProbeError::Timeout => true,
            EndpointProbeError::Connect(e) | EndpointProbeError::Io(e) => io_kind_is_transient(e),
            EndpointProbeError::ConfigureNonblocking(_) | EndpointProbeError::Random(_) => true,
            _ => false,
        },
        // Reading another process's exe path/hash can fail transiently while
        // that process is still perfectly alive.
        ProbeError::VerifyPid(
            VerifyPidError::ExeHash { .. }
            | VerifyPidError::ExePath { .. }
            | VerifyPidError::Handle { .. },
        ) => true,
        _ => false,
    }
}

fn probe_soldr_daemon_once(paths: &SoldrPaths) -> ProbeOutcome {
    let Some((pid, exe_path)) = read_pid_file(paths) else {
        return ProbeOutcome::NotLive;
    };
    let Some(expected) = daemon_process_from_pid_file(paths, pid, exe_path) else {
        return ProbeOutcome::NotLive;
    };
    let handle = match BackendHandle::probe_with_service(
        SOLDR_DAEMON_SERVICE_NAME,
        SOLDR_DAEMON_SERVICE_VERSION,
        &expected.ipc_endpoint,
        &expected,
    ) {
        Ok(handle) => handle,
        Err(err) if probe_error_is_transient(&err) => return ProbeOutcome::Inconclusive(err),
        Err(_) => return ProbeOutcome::NotLive,
    };

    ProbeOutcome::Live(Box::new(SoldrDaemonBackendHandle {
        service_name: SOLDR_DAEMON_SERVICE_NAME,
        service_version: SOLDR_DAEMON_SERVICE_VERSION,
        protocol_version: PROTOCOL_VERSION,
        pid: handle.daemon_process.pid,
        exe_path: handle.daemon_process.exe_path,
        endpoint: PathBuf::from(handle.daemon_process.ipc_endpoint.path),
        pid_file: daemon_pid_path(paths),
        adoption_status: RUNNING_PROCESS_BACKEND_HANDLE_STATUS,
    }))
}

/// Probe the daemon named by the PID file, retrying only while the probe is
/// inconclusive (soldr#1893).
///
/// A definitive "not live" returns immediately, so the common
/// no-daemon-running path pays no extra latency. Only a timed-out or
/// transiently-faulted probe is retried, and a genuinely dead daemon never
/// becomes live — so the retry can remove false negatives but cannot mask a
/// true one.
pub(crate) fn probe_soldr_daemon(paths: &SoldrPaths) -> Option<SoldrDaemonBackendHandle> {
    let started = Instant::now();
    let mut attempts = 0_u32;
    loop {
        attempts += 1;
        match probe_soldr_daemon_once(paths) {
            ProbeOutcome::Live(handle) => {
                if attempts > 1 {
                    tracing::warn!(
                        pid = handle.pid,
                        endpoint = %handle.endpoint.display(),
                        attempts,
                        elapsed_ns = started.elapsed().as_nanos(),
                        "soldr-daemon liveness probe succeeded only after retry; \
                         the first attempt would have reported the daemon as not running"
                    );
                }
                return Some(*handle);
            }
            ProbeOutcome::NotLive => return None,
            ProbeOutcome::Inconclusive(err) => {
                let elapsed = started.elapsed();
                if elapsed >= PROBE_INCONCLUSIVE_RETRY_BUDGET {
                    tracing::warn!(
                        error = %err,
                        attempts,
                        elapsed_ns = elapsed.as_nanos(),
                        budget_ns = PROBE_INCONCLUSIVE_RETRY_BUDGET.as_nanos(),
                        "soldr-daemon liveness probe never reached a verdict within its retry \
                         budget; reporting the daemon as not running"
                    );
                    return None;
                }
                tracing::debug!(
                    error = %err,
                    attempts,
                    elapsed_ns = elapsed.as_nanos(),
                    "soldr-daemon liveness probe was inconclusive; retrying"
                );
                std::thread::sleep(PROBE_INCONCLUSIVE_RETRY_BACKOFF);
            }
        }
    }
}

/// Wait for a broker-placed daemon to publish its PID/image and answer on the
/// broker-assigned SESSION endpoint.
pub fn wait_for_broker_backend_handle(
    paths: &SoldrPaths,
    service_name: &str,
    service_version: &str,
    endpoint: &Endpoint,
    timeout: Duration,
) -> io::Result<BackendHandle> {
    wait_for_broker_backend_handle_while(
        paths,
        service_name,
        service_version,
        endpoint,
        timeout,
        || Ok(None),
    )
}

/// Variant used by the owning launcher. An actual child exit terminates the
/// wait immediately; a slow but live cold start keeps its one process for the
/// entire bounded acquisition window instead of being killed and resurrected.
pub fn wait_for_broker_backend_handle_while(
    paths: &SoldrPaths,
    service_name: &str,
    service_version: &str,
    endpoint: &Endpoint,
    timeout: Duration,
    mut child_status: impl FnMut() -> io::Result<Option<i32>>,
) -> io::Result<BackendHandle> {
    let deadline = Instant::now() + timeout;
    let mut last_error = "daemon has not published its PID yet".to_string();
    loop {
        if let Some(status) = child_status()? {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("broker-launched soldr-daemon exited before readiness ({status})"),
            ));
        }
        if let Some((pid, exe_path)) = read_pid_file(paths) {
            if let Some(mut daemon) = daemon_process_from_pid_file(paths, pid, exe_path) {
                daemon.ipc_endpoint = endpoint.clone();
                match BackendHandle::probe_with_service(
                    service_name.to_string(),
                    service_version.to_string(),
                    endpoint,
                    &daemon,
                ) {
                    Ok(handle) => return Ok(handle),
                    Err(err) => last_error = err.to_string(),
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "broker-launched soldr-daemon was not ready within {timeout:?}: {last_error}"
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn current_daemon_process(
    paths: &SoldrPaths,
    idle_timeout_secs: Option<u32>,
) -> Result<DaemonProcess, IdentityError> {
    DaemonProcess::current_process(soldr_daemon_endpoint(paths), idle_timeout_secs)
}

pub(crate) fn soldr_backend_endpoint_mux(
    daemon: DaemonProcess,
) -> BackendEndpointMux<fn(&[u8]) -> LegacyClassification> {
    BackendEndpointMux::new(daemon, &[], classify_soldr_legacy_wire)
}

fn classify_soldr_legacy_wire(buf: &[u8]) -> LegacyClassification {
    if buf.len() < LEGACY_FRAME_HEADER_BYTES {
        return LegacyClassification::NeedMoreBytes;
    }
    let version = u32::from_le_bytes(
        buf[4..LEGACY_FRAME_HEADER_BYTES]
            .try_into()
            .expect("slice is exactly 4 bytes"),
    );
    if version == PROTOCOL_VERSION {
        LegacyClassification::Legacy
    } else {
        LegacyClassification::NotLegacy
    }
}

fn daemon_process_from_pid_file(
    paths: &SoldrPaths,
    pid: u32,
    exe_path: PathBuf,
) -> Option<DaemonProcess> {
    Some(DaemonProcess {
        pid,
        exe_sha256: sha256_file(&exe_path).ok()?,
        exe_path,
        boot_id: host_identity::current().boot_id,
        ipc_endpoint: soldr_daemon_endpoint(paths),
        started_at_unix_ms: 0,
        idle_timeout_secs: None,
    })
}

fn soldr_daemon_endpoint(paths: &SoldrPaths) -> Endpoint {
    let namespace_id = host_identity::current().namespace_id;
    // Use running-process's smart constructors (issue #726): on
    // Windows `Endpoint::windows_pipe` enforces the bare-pipe-name
    // invariant (no leading `\\.\pipe\`, never empty), which the
    // prior manual `Endpoint { path }` literal could silently
    // violate and address the wrong pipe. `daemon_pipe_name` and
    // `default_sock_path` are both soldr-controlled producers, so
    // the smart-constructor errors only fire on a programming bug
    // — `expect` is correct.
    #[cfg(windows)]
    {
        // soldr#1808: the identity lookup is a genuine runtime failure (unlike
        // the smart-constructor errors this `expect` was written for), so it
        // gets its own message rather than being folded into that claim.
        let pipe_name =
            crate::cache_lib::daemon_pipe_name(paths).unwrap_or_else(|err| panic!("{err}"));
        Endpoint::windows_pipe(namespace_id, pipe_name)
            .expect("daemon_pipe_name returns a bare, non-empty pipe name")
    }
    #[cfg(unix)]
    {
        Endpoint::unix_socket(
            namespace_id,
            client::default_sock_path(paths)
                .to_string_lossy()
                .into_owned(),
        )
        .expect("default_sock_path returns a non-empty socket path")
    }
}

/// SHA-256 of a file's bytes. Retained specifically for `exe_sha256`, which is
/// a cross-repo daemon-identity contract: running-process computes the same
/// field via SHA-256 (`backend_lifecycle::identity`) and compares it in
/// `verify_daemon_process`, so this side must stay SHA-256 to match. The
/// *image-placement* hashing (the cold-start hotspot) moved to blake3 via
/// `super::image_hash`; migrating this identity field to blake3 would require a
/// coordinated running-process wire-format change (soldr#2442 follow-up).
fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    let bytes = std::fs::read(path)?;
    let digest = Sha256::digest(&bytes);
    let mut out = [0_u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_lib::soldr_daemon_dir;
    use running_process::broker::backend_sdk::MuxPoll;
    use tempfile::TempDir;

    fn write_pid_file(paths: &SoldrPaths, pid: u32, exe_path: &Path) {
        std::fs::create_dir_all(soldr_daemon_dir(paths)).expect("daemon dir");
        std::fs::write(
            daemon_pid_path(paths),
            format!("{pid}\n{}\n", exe_path.display()),
        )
        .expect("write pid file");
    }

    crate::timed_test!(broker_service_partition_covers_root_and_image_hash, {
        let temp = TempDir::new().expect("tempdir");
        let binary_a = temp.path().join("soldr-daemon-a");
        let binary_b = temp.path().join("soldr-daemon-b");
        std::fs::write(&binary_a, b"image-a").expect("binary a");
        std::fs::write(&binary_b, b"image-b").expect("binary b");
        let root_a = SoldrPaths::with_root(temp.path().join("root-a"));
        let root_b = SoldrPaths::with_root(temp.path().join("root-b"));
        assert!(!root_a.root.exists(), "fixture root starts absent");

        let route = broker_service_name_for(&root_a, &binary_a).expect("route");
        assert!(
            root_a.root.exists(),
            "route identity materializes its cache root"
        );
        assert_eq!(
            route,
            broker_service_name_for(&root_a, &binary_a).expect("stable route")
        );
        assert_ne!(
            route,
            broker_service_name_for(&root_b, &binary_a).expect("different root")
        );
        assert_ne!(
            route,
            broker_service_name_for(&root_a, &binary_b).expect("different image")
        );
        assert!(route.starts_with("soldr-daemon-"));
        assert_eq!(route.len(), "soldr-daemon-".len() + 32);
    });

    crate::timed_test!(dependency_status_documents_active_backend_handle_usage, {
        let status = RUNNING_PROCESS_BACKEND_HANDLE_STATUS;
        assert_eq!(status.crate_name, "running-process");
        // After #726 the dep ships as a published crates.io release rather
        // than the pre-publication git pin.
        assert!(status.dependency_source.starts_with("crates.io:"));
        assert!(status.dependency_source.contains("running-process@"));
        assert_eq!(
            status.required_symbol,
            "running_process::broker::backend_handle::BackendHandle"
        );
        assert_eq!(status.running_process_issue, "zackees/running-process#232");
        assert_eq!(status.adoption_tracker_issue, "zackees/running-process#242");
        assert_eq!(status.soldr_issue, "zackees/soldr#718");
        assert!(status.active_endpoint_probe);
        assert!(status.remaining_gate.contains("BackendEndpointMux adopted"));
    });

    crate::timed_test!(probe_missing_pid_file_reports_no_handle, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        assert!(probe_soldr_daemon(&paths).is_none());
    });

    crate::timed_test!(probe_stale_pid_file_reports_no_handle, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        write_pid_file(&paths, u32::MAX, Path::new("soldr-daemon"));

        assert!(probe_soldr_daemon(&paths).is_none());
    });

    crate::timed_test!(
        pid_file_identity_records_running_process_backend_handle_shape,
        {
            let temp = TempDir::new().expect("tempdir");
            let paths = SoldrPaths::with_root(temp.path().to_path_buf());
            let current_exe = std::env::current_exe().expect("current exe");

            let identity =
                daemon_process_from_pid_file(&paths, std::process::id(), current_exe.clone())
                    .expect("identity");

            assert_eq!(identity.pid, std::process::id());
            assert_eq!(identity.exe_path, current_exe);
            assert_eq!(identity.ipc_endpoint, soldr_daemon_endpoint(&paths));
            assert_eq!(identity.exe_sha256, sha256_file(&current_exe).unwrap());
            assert!(!identity.boot_id.is_empty());
        }
    );

    crate::timed_test!(backend_endpoint_mux_classifies_soldr_legacy_header, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let current_exe = std::env::current_exe().expect("current exe");
        let daemon =
            daemon_process_from_pid_file(&paths, std::process::id(), current_exe).expect("daemon");
        let mux = soldr_backend_endpoint_mux(daemon);
        let mut soldr_header = [0_u8; LEGACY_FRAME_HEADER_BYTES];
        soldr_header[..4].copy_from_slice(&1_u32.to_le_bytes());
        soldr_header[4..].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());

        assert!(matches!(mux.poll(&soldr_header), Ok(MuxPoll::Legacy)));
    });

    crate::timed_test!(soldr_legacy_detector_waits_for_full_header, {
        let mut partial = [0_u8; LEGACY_FRAME_HEADER_BYTES - 1];
        partial[..4].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(
            classify_soldr_legacy_wire(&partial),
            LegacyClassification::NeedMoreBytes,
        );
    });

    // soldr#1893: the whole point of the classifier is that a probe which
    // never got an answer must not be reported as "daemon is not running".

    fn endpoint_err(inner: EndpointProbeError) -> BackendHandleError {
        BackendHandleError::Probe(ProbeError::EndpointResponse(inner))
    }

    crate::timed_test!(probe_timeout_is_treated_as_inconclusive, {
        assert!(probe_error_is_transient(&endpoint_err(
            EndpointProbeError::Timeout
        )));
    });

    crate::timed_test!(
        connect_timeout_and_absent_endpoint_are_told_apart_by_io_kind,
        {
            // Both arrive as `Connect`; only the ErrorKind separates them, which
            // is the trap this classifier exists to avoid.
            assert!(
                probe_error_is_transient(&endpoint_err(EndpointProbeError::Connect(
                    io::Error::from(io::ErrorKind::TimedOut)
                ))),
                "a connect that timed out says nothing about the daemon"
            );
            for definitive in [io::ErrorKind::NotFound, io::ErrorKind::ConnectionRefused] {
                assert!(
                    !probe_error_is_transient(&endpoint_err(EndpointProbeError::Connect(
                        io::Error::from(definitive)
                    ))),
                    "{definitive:?} means the endpoint is genuinely absent"
                );
            }
        }
    );

    crate::timed_test!(identity_and_mismatch_failures_are_definitive, {
        assert!(!probe_error_is_transient(&BackendHandleError::Probe(
            ProbeError::EndpointMismatch
        )));
        assert!(!probe_error_is_transient(&endpoint_err(
            EndpointProbeError::IdentityMismatch { field: "pid" }
        )));
        assert!(!probe_error_is_transient(&endpoint_err(
            EndpointProbeError::UnsupportedFramingVersion {
                got: 9,
                expected: 1
            }
        )));
        assert!(!probe_error_is_transient(&BackendHandleError::Probe(
            ProbeError::VerifyPid(VerifyPidError::NotFound { pid: 4321 })
        )));
    });

    crate::timed_test!(reading_a_live_process_image_can_fail_transiently, {
        // The process may be alive and healthy while an exe-path read fails.
        assert!(probe_error_is_transient(&BackendHandleError::Probe(
            ProbeError::VerifyPid(VerifyPidError::ExePath {
                pid: 4321,
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            })
        )));
    });

    crate::timed_test!(retry_budget_allows_more_than_one_probe_attempt, {
        // A budget shorter than the 500 ms probe deadline would make the
        // retry unreachable in practice.
        assert!(
            PROBE_INCONCLUSIVE_RETRY_BUDGET > Duration::from_millis(500),
            "retry budget must exceed one probe deadline or it buys nothing"
        );
        assert!(PROBE_INCONCLUSIVE_RETRY_BACKOFF < PROBE_INCONCLUSIVE_RETRY_BUDGET);
    });
}
