//! soldr-daemon integration with `running-process` `BackendHandle`.
//!
//! `running-process` commit 04f6387 added the active endpoint-response probe
//! required by zackees/running-process#232. soldr now uses that probe at the
//! `daemon::lifecycle::is_live` boundary: the route claim is only trusted after
//! `BackendHandle::probe_with_service` verifies process identity and the
//! live daemon answers the broker-v1 BackendHandle nonce challenge on its IPC
//! endpoint.

use crate::cache_lib::soldr_daemon_dir;
use crate::core::SoldrPaths;
use crate::daemon::client;
use crate::daemon::lifecycle::{pid_exe_stem_matches, pid_is_alive};
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

const BROKER_ROUTE_CLAIM_FILE: &str = "broker-route-claim.pb";

pub(crate) const CONTROL_FRAME_HEADER_BYTES: usize = 8;

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

/// Stable daemon service type. The requested route name additionally partitions
/// by canonical root, Soldr version, and daemon image digest.
pub const SOLDR_DAEMON_SERVICE_NAME: &str = "soldr-daemon";
pub const SOLDR_DAEMON_SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const SOLDR_BROKER_SERVICE_ENV_VAR: &str = "SOLDR_BROKER_SERVICE";
pub const SOLDR_DAEMON_IMAGE_HASH_LABEL: &str = "soldr-image-blake3";

pub fn broker_service_name_for(paths: &SoldrPaths, daemon_binary: &Path) -> io::Result<String> {
    broker_route_identity(paths, daemon_binary).map(|identity| identity.service_name)
}

pub struct BrokerRouteIdentity {
    pub service_name: String,
    pub image_hash: String,
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
    // Windows canonicalization yields `C:\...` forms; fold them to the
    // same slash/letter casing the broker-side identity uses so the hash
    // matches across the wire.
    let identity =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            identity.replace('\\', "/").to_ascii_lowercase()
        } else {
            identity
        };
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
        image_hash,
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
            parent.join(
                if crate::platform::host::facts::os()
                    == crate::platform::host::facts::HostOs::Windows
                {
                    "soldr-daemon.exe"
                } else {
                    "soldr-daemon"
                },
            )
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
    pub(crate) route_claim: PathBuf,
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
    let expected = match read_broker_route_claim(paths) {
        Ok(Some(claim)) => claim,
        Ok(None) => return ProbeOutcome::NotLive,
        Err(_) => {
            prune_broker_route_claim(paths);
            return ProbeOutcome::NotLive;
        }
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
        route_claim: broker_route_claim_path(paths),
        adoption_status: RUNNING_PROCESS_BACKEND_HANDLE_STATUS,
    }))
}

/// Probe the daemon named by the route claim, retrying only while the probe is
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
        |_| {},
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
    mut progress: impl FnMut(&str),
) -> io::Result<BackendHandle> {
    let deadline = Instant::now() + timeout;
    let mut next_progress = Instant::now() + Duration::from_secs(1);
    let mut last_error = "daemon has not published its protobuf route claim yet".to_string();
    loop {
        if let Some(status) = child_status()? {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("broker-launched soldr-daemon exited before readiness ({status})"),
            ));
        }
        match read_broker_route_claim(paths) {
            Ok(Some(daemon)) if daemon.ipc_endpoint != *endpoint => {
                last_error = format!(
                    "daemon route claim endpoint mismatch: claimed={}, expected={}",
                    daemon.ipc_endpoint.path, endpoint.path
                );
            }
            Ok(Some(daemon)) => {
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
            Ok(None) => {}
            Err(error) => last_error = format!("daemon route claim is unreadable: {error}"),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "broker-launched soldr-daemon was not ready within {timeout:?}: {last_error}"
                ),
            ));
        }
        if Instant::now() >= next_progress {
            progress(&last_error);
            next_progress = Instant::now() + Duration::from_secs(1);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Deterministic, root-local protobuf claim used only for broker restart
/// re-adoption. It is disposable discovery state, not authoritative routing
/// state; every reader must verify it with an exact `BackendHandle` probe.
pub fn broker_route_claim_path(paths: &SoldrPaths) -> PathBuf {
    soldr_daemon_dir(paths).join(BROKER_ROUTE_CLAIM_FILE)
}

pub fn publish_broker_route_claim(paths: &SoldrPaths, daemon: &DaemonProcess) -> io::Result<()> {
    use prost::Message as _;
    use std::io::Write as _;

    let directory = soldr_daemon_dir(paths);
    std::fs::create_dir_all(&directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    temporary.write_all(&daemon.to_proto().encode_to_vec())?;
    temporary.as_file().sync_all()?;
    let temporary = temporary
        .into_temp_path()
        .keep()
        .map_err(|error| error.error)?;
    let target = broker_route_claim_path(paths);
    replace_route_claim(&temporary, &target).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

fn replace_route_claim(source: &Path, target: &Path) -> io::Result<()> {
    crate::platform::fs::replace::atomic_replace(source, target)
}

pub fn read_broker_route_claim(paths: &SoldrPaths) -> io::Result<Option<DaemonProcess>> {
    use prost::Message as _;
    let bytes = match std::fs::read(broker_route_claim_path(paths)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let claim = running_process::broker::protocol::DaemonProcess::decode(bytes.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    DaemonProcess::try_from(claim)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn prune_broker_route_claim(paths: &SoldrPaths) {
    let _ = std::fs::remove_file(broker_route_claim_path(paths));
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
    BackendEndpointMux::new(daemon, &[], classify_soldr_control_wire)
}

fn classify_soldr_control_wire(buf: &[u8]) -> LegacyClassification {
    if buf.len() < CONTROL_FRAME_HEADER_BYTES {
        return LegacyClassification::NeedMoreBytes;
    }
    let version = u32::from_le_bytes(
        buf[4..CONTROL_FRAME_HEADER_BYTES]
            .try_into()
            .expect("slice is exactly 4 bytes"),
    );
    if version == PROTOCOL_VERSION {
        LegacyClassification::Legacy
    } else {
        LegacyClassification::NotLegacy
    }
}

fn soldr_daemon_endpoint(paths: &SoldrPaths) -> Endpoint {
    let namespace_id = host_identity::current().namespace_id;
    // Use running-process's smart constructors (issue #726): on
    // Windows `Endpoint::windows_pipe` enforces the bare-pipe-name
    // invariant (no leading `\\.\pipe\`, never empty), which the
    // prior manual `Endpoint { path }` literal could silently violate and
    // address the wrong pipe. `default_sock_path` is soldr-controlled, so
    // the smart-constructor errors only fire on a programming bug
    // — `expect` is correct.
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        let full = client::default_sock_path(paths)
            .to_string_lossy()
            .into_owned();
        let pipe_name = full.strip_prefix(r"\\.\pipe\").unwrap_or(&full);
        Endpoint::windows_pipe(namespace_id, pipe_name)
            .expect("resolved control endpoint returns a bare, non-empty pipe name")
    } else {
        Endpoint::unix_socket(
            namespace_id,
            client::default_sock_path(paths)
                .to_string_lossy()
                .into_owned(),
        )
        .expect("default_sock_path returns a non-empty socket path")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_lib::soldr_daemon_dir;
    use running_process::broker::backend_sdk::MuxPoll;
    use tempfile::TempDir;

    #[test]
    fn broker_service_partition_covers_root_and_image_hash() {
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
    }

    #[test]
    fn broker_route_claim_round_trips_and_replaces_atomically() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        let first = current_daemon_process(&paths, Some(3)).expect("first claim");
        publish_broker_route_claim(&paths, &first).expect("publish first claim");

        let mut second = first.clone();
        second.idle_timeout_secs = Some(9);
        publish_broker_route_claim(&paths, &second).expect("replace claim");

        let observed = read_broker_route_claim(&paths)
            .expect("read claim")
            .expect("claim exists");
        assert_eq!(observed.pid, second.pid);
        assert_eq!(observed.exe_path, second.exe_path);
        assert_eq!(observed.exe_sha256, second.exe_sha256);
        assert_eq!(observed.boot_id, second.boot_id);
        assert_eq!(observed.ipc_endpoint, second.ipc_endpoint);
        assert_eq!(observed.idle_timeout_secs, Some(9));

        let leftovers = std::fs::read_dir(soldr_daemon_dir(&paths))
            .expect("read daemon dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.path() != broker_route_claim_path(&paths))
            .count();
        assert_eq!(leftovers, 0, "atomic publish must not leave temp files");
    }

    #[test]
    fn corrupt_broker_route_claim_is_invalid_disposable_state() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        std::fs::create_dir_all(soldr_daemon_dir(&paths)).expect("daemon dir");
        std::fs::write(broker_route_claim_path(&paths), b"not protobuf").expect("corrupt claim");

        let error = read_broker_route_claim(&paths).expect_err("corruption must be reported");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        prune_broker_route_claim(&paths);
        assert!(!broker_route_claim_path(&paths).exists());
    }

    #[test]
    fn dependency_status_documents_active_backend_handle_usage() {
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
    }

    #[test]
    fn probe_missing_route_claim_reports_no_handle() {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        assert!(probe_soldr_daemon(&paths).is_none());
    }

    #[test]
    fn backend_endpoint_mux_classifies_soldr_legacy_header() {
        let current_exe = std::env::current_exe().expect("current exe");
        let endpoint = Endpoint::unix_socket("test", "/tmp/soldr-test.sock")
            .or_else(|_| Endpoint::windows_pipe("test", "soldr-test"))
            .expect("test endpoint");
        let daemon = DaemonProcess::current_process(endpoint, None).expect("daemon");
        assert_eq!(daemon.exe_path, current_exe);
        let mux = soldr_backend_endpoint_mux(daemon);
        let mut soldr_header = [0_u8; CONTROL_FRAME_HEADER_BYTES];
        soldr_header[..4].copy_from_slice(&1_u32.to_le_bytes());
        soldr_header[4..].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());

        assert!(matches!(mux.poll(&soldr_header), Ok(MuxPoll::Legacy)));
    }

    #[test]
    fn soldr_legacy_detector_waits_for_full_header() {
        let mut partial = [0_u8; CONTROL_FRAME_HEADER_BYTES - 1];
        partial[..4].copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(
            classify_soldr_control_wire(&partial),
            LegacyClassification::NeedMoreBytes,
        );
    }

    // soldr#1893: the whole point of the classifier is that a probe which
    // never got an answer must not be reported as "daemon is not running".

    fn endpoint_err(inner: EndpointProbeError) -> BackendHandleError {
        BackendHandleError::Probe(ProbeError::EndpointResponse(inner))
    }

    #[test]
    fn probe_timeout_is_treated_as_inconclusive() {
        assert!(probe_error_is_transient(&endpoint_err(
            EndpointProbeError::Timeout
        )));
    }

    #[test]
    fn connect_timeout_and_absent_endpoint_are_told_apart_by_io_kind() {
        // Both arrive as `Connect`; only the ErrorKind separates them, which
        // is the trap this classifier exists to avoid.
        assert!(
            probe_error_is_transient(&endpoint_err(EndpointProbeError::Connect(io::Error::from(
                io::ErrorKind::TimedOut
            )))),
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

    #[test]
    fn identity_and_mismatch_failures_are_definitive() {
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
    }

    #[test]
    fn reading_a_live_process_image_can_fail_transiently() {
        // The process may be alive and healthy while an exe-path read fails.
        assert!(probe_error_is_transient(&BackendHandleError::Probe(
            ProbeError::VerifyPid(VerifyPidError::ExePath {
                pid: 4321,
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            })
        )));
    }

    #[test]
    fn retry_budget_allows_more_than_one_probe_attempt() {
        // A budget shorter than the 500 ms probe deadline would make the
        // retry unreachable in practice.
        assert!(
            PROBE_INCONCLUSIVE_RETRY_BUDGET > Duration::from_millis(500),
            "retry budget must exceed one probe deadline or it buys nothing"
        );
        assert!(PROBE_INCONCLUSIVE_RETRY_BACKOFF < PROBE_INCONCLUSIVE_RETRY_BUDGET);
    }
}
