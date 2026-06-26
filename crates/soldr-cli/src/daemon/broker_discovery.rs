//! Full v1 broker adoption for soldr-daemon (zackees/running-process#434).
//!
//! This module layers broker-mediated discovery *ahead of* soldr's existing
//! direct `BackendHandle` probe (`backend_handle_adoption.rs`). It follows the
//! frozen #433 broker API documented in running-process `docs/INTEGRATE-BROKER.md`
//! and the soldr-specific `docs/consumer-adoption-soldr.md`:
//!
//! 1. Register a `soldr-daemon` [`ServiceDefinition`] via [`ServiceDefinitionBuilder`]
//!    (`SHARED_BROKER` for per-user local; `EXPLICIT_INSTANCE` "ci-trusted" for CI
//!    trust grouping).
//! 2. Publish a [`CacheManifest`] (state / pinned-binary / runtime / lock / log
//!    roots) via [`CacheManifestBuilder`].
//! 3. Adopt the negotiated backend endpoint through [`BrokerSession::adopt`],
//!    constructing the v1 `Hello` (`service_name = "soldr-daemon"`,
//!    `client_min/max_protocol = 1`, `client_lib_name = "running-process"`,
//!    `wanted_version` = the soldr-daemon version).
//! 4. Classify a `HelloReply::Refused` through the typed [`RefusalKind`] enum
//!    instead of string-matching the broker's reason.
//!
//! `RUNNING_PROCESS_DISABLE=1` short-circuits before any broker contact:
//! [`BrokerSession::adopt`] returns [`AdoptError::BrokerDisabled`] and the caller
//! falls back to the direct soldr-daemon path
//! (`backend_handle_adoption::probe_soldr_daemon`). soldr keeps the existing
//! direct path and its tests fully active during the rollout window.

use crate::core::SoldrPaths;
use crate::daemon::backend_handle_adoption::{
    running_process_disabled, SOLDR_DAEMON_SERVICE_NAME, SOLDR_DAEMON_SERVICE_VERSION,
};
use running_process::broker::adopt::{AdoptError, BrokerSession};
use running_process::broker::builders::{CacheManifestBuilder, ServiceDefinitionBuilder};
use running_process::broker::client::{ConnectBackendRequest, RefusalKind};
use running_process::broker::doctor::default_broker_endpoint;
use running_process::broker::protocol::{CacheManifest, CacheRootKind, ServiceDefinition};
use std::path::{Path, PathBuf};

/// Minimum broker-mediated soldr-daemon version the service definition accepts,
/// per `docs/consumer-adoption-soldr.md`.
pub(crate) const SOLDR_DAEMON_MIN_VERSION: &str = "0.8.0";

/// `running-process` is the broker client library soldr speaks v1 through.
pub(crate) const RUNNING_PROCESS_CLIENT_LIB_NAME: &str = "running-process";

/// Trust-group label for the `EXPLICIT_INSTANCE` CI service definition.
pub(crate) const SOLDR_DAEMON_CI_INSTANCE: &str = "ci-trusted";

/// The v1 broker control-plane protocol range soldr requests. soldr pins
/// `client_min_protocol == client_max_protocol == 1` (the broker-client library
/// fills these in; recorded here for the conformance assertions and diagnostics).
pub(crate) const SOLDR_BROKER_PROTOCOL: u32 = 1;

/// How soldr-daemon discovery resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscoveryRoute {
    /// Broker negotiated a backend endpoint; the string is that endpoint.
    BrokerNegotiated { endpoint: String },
    /// `RUNNING_PROCESS_DISABLE=1` was set; the caller must take the direct path.
    DirectFallbackDisabled,
    /// The broker refused the Hello; classified for the caller to act on.
    Refused { kind: RefusalKind },
    /// The broker was unreachable or the dial failed (not a refusal). The caller
    /// should fall back to the direct soldr-daemon path.
    DirectFallbackUnavailable,
}

/// Errors raised while *constructing* the broker registration messages. Adoption
/// (dial/negotiation) failures are surfaced as [`DiscoveryRoute`] variants, not
/// errors, because the caller treats them as a fall-back signal, never a hard
/// failure of the build.
#[derive(Debug)]
pub(crate) enum BrokerDiscoveryError {
    /// Building / validating the `ServiceDefinition` failed.
    ServiceDefinition(String),
    /// Building / sealing the `CacheManifest` failed.
    CacheManifest(String),
    /// The default broker endpoint could not be derived for this user.
    Endpoint(String),
}

impl std::fmt::Display for BrokerDiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrokerDiscoveryError::ServiceDefinition(msg) => {
                write!(f, "soldr-daemon service definition invalid: {msg}")
            }
            BrokerDiscoveryError::CacheManifest(msg) => {
                write!(f, "soldr-daemon cache manifest invalid: {msg}")
            }
            BrokerDiscoveryError::Endpoint(msg) => {
                write!(f, "cannot derive broker endpoint: {msg}")
            }
        }
    }
}

impl std::error::Error for BrokerDiscoveryError {}

/// Build the soldr-daemon `SHARED_BROKER` service definition. This is the
/// per-user local form used for normal developer machines.
pub(crate) fn soldr_daemon_service_definition(
    daemon_binary: &Path,
) -> Result<ServiceDefinition, BrokerDiscoveryError> {
    shared_broker_builder(daemon_binary)
        .build()
        .map_err(|err| BrokerDiscoveryError::ServiceDefinition(err.to_string()))
}

/// Build the soldr-daemon `EXPLICIT_INSTANCE` "ci-trusted" service definition
/// used to trust-group CI pools.
pub(crate) fn soldr_daemon_ci_service_definition(
    daemon_binary: &Path,
) -> Result<ServiceDefinition, BrokerDiscoveryError> {
    ci_instance_builder(daemon_binary)
        .build()
        .map_err(|err| BrokerDiscoveryError::ServiceDefinition(err.to_string()))
}

fn shared_broker_builder(daemon_binary: &Path) -> ServiceDefinitionBuilder {
    common_labels(
        ServiceDefinitionBuilder::shared_broker(
            SOLDR_DAEMON_SERVICE_NAME,
            daemon_binary.display().to_string(),
        )
        .min_version(SOLDR_DAEMON_MIN_VERSION)
        .allow_version(SOLDR_DAEMON_SERVICE_VERSION)
        .per_version_binary_dir(binary_dir(daemon_binary)),
    )
}

fn ci_instance_builder(daemon_binary: &Path) -> ServiceDefinitionBuilder {
    common_labels(
        ServiceDefinitionBuilder::explicit_instance(
            SOLDR_DAEMON_SERVICE_NAME,
            daemon_binary.display().to_string(),
            SOLDR_DAEMON_CI_INSTANCE,
        )
        .min_version(SOLDR_DAEMON_MIN_VERSION)
        .allow_version(SOLDR_DAEMON_SERVICE_VERSION)
        .per_version_binary_dir(binary_dir(daemon_binary)),
    )
}

fn common_labels(builder: ServiceDefinitionBuilder) -> ServiceDefinitionBuilder {
    builder
        .label("vendor", "zackees")
        .label("consumer", "soldr")
        .label("running-process-tracker", "zackees/running-process#434")
}

fn binary_dir(daemon_binary: &Path) -> String {
    daemon_binary
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Build the soldr-daemon cache manifest declaring the state, pinned-binary,
/// runtime, lock, and log roots (per `docs/consumer-adoption-soldr.md` step 7).
///
/// `build` here only seals the `self_sha256` digest — it does not write to the
/// central registry. Use [`publish_cache_manifest`] / [`publish_cache_manifest_in`]
/// for the persistent path.
pub(crate) fn soldr_daemon_cache_manifest(
    paths: &SoldrPaths,
) -> Result<CacheManifest, BrokerDiscoveryError> {
    cache_manifest_builder(paths)
        .build()
        .map_err(|err| BrokerDiscoveryError::CacheManifest(err.to_string()))
}

fn cache_manifest_builder(paths: &SoldrPaths) -> CacheManifestBuilder {
    let roots = SoldrCacheRoots::for_paths(paths);
    CacheManifestBuilder::new(SOLDR_DAEMON_SERVICE_NAME, SOLDR_DAEMON_SERVICE_VERSION)
        .broker_instance("shared")
        // state: the redb state/data DBs live directly under the soldr root.
        .root(CacheRootKind::CacheData, roots.state.display().to_string())
        // pinned binary: machine-level pinned install (issue #426), preserved
        // across uninstall — recorded as a runtime/binary root.
        .root(
            CacheRootKind::CacheRuntime,
            roots.pinned_binary.display().to_string(),
        )
        // runtime: relocated daemon binaries (ensure_daemon_relocated dest).
        .root(
            CacheRootKind::CacheRuntime,
            roots.runtime.display().to_string(),
        )
        // lock: PID file, IPC socket/pipe, spawn lock.
        .root(CacheRootKind::CacheLocks, roots.lock.display().to_string())
        // log: lifecycle JSONL + daemon stderr log.
        .root(CacheRootKind::CacheLogs, roots.log.display().to_string())
}

/// Publish the soldr-daemon cache manifest into the central registry.
pub(crate) fn publish_cache_manifest(paths: &SoldrPaths) -> Result<PathBuf, BrokerDiscoveryError> {
    cache_manifest_builder(paths)
        .publish()
        .map_err(|err| BrokerDiscoveryError::CacheManifest(err.to_string()))
}

/// Publish the manifest into an explicit registry dir (tests, custom layouts).
pub(crate) fn publish_cache_manifest_in(
    paths: &SoldrPaths,
    registry_dir: &Path,
) -> Result<PathBuf, BrokerDiscoveryError> {
    cache_manifest_builder(paths)
        .publish_in(registry_dir)
        .map_err(|err| BrokerDiscoveryError::CacheManifest(err.to_string()))
}

/// The concrete on-disk roots soldr records in its cache manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SoldrCacheRoots {
    pub(crate) state: PathBuf,
    pub(crate) pinned_binary: PathBuf,
    pub(crate) runtime: PathBuf,
    pub(crate) lock: PathBuf,
    pub(crate) log: PathBuf,
}

impl SoldrCacheRoots {
    pub(crate) fn for_paths(paths: &SoldrPaths) -> Self {
        let daemon_dir = crate::cache_lib::soldr_daemon_dir(paths);
        Self {
            state: paths.root.clone(),
            pinned_binary: paths.pinned_bin.clone(),
            runtime: paths.root.join("runtime").join("soldr-daemon"),
            lock: daemon_dir.clone(),
            log: daemon_dir.join("logs"),
        }
    }
}

/// Construct the v1 broker `Hello` request for soldr-daemon adoption.
///
/// `client_lib_name` is `"running-process"`; `wanted_version` and the client's
/// own `self_version` are the soldr-daemon `CARGO_PKG_VERSION`. The broker-client
/// library pins `client_min_protocol == client_max_protocol == 1`.
fn connect_request<'a>(broker_endpoint: &'a str) -> ConnectBackendRequest<'a> {
    ConnectBackendRequest::new(
        broker_endpoint,
        SOLDR_DAEMON_SERVICE_NAME,
        SOLDR_DAEMON_SERVICE_VERSION,
        SOLDR_DAEMON_SERVICE_VERSION,
    )
}

/// Discover soldr-daemon through the broker.
///
/// Resolution order (matches `docs/consumer-adoption-soldr.md`):
/// 1. `RUNNING_PROCESS_DISABLE=1` → [`DiscoveryRoute::DirectFallbackDisabled`]
///    (the caller takes the direct soldr-daemon path).
/// 2. derive the default broker endpoint; on failure the caller falls back to
///    the direct path.
/// 3. [`BrokerSession::adopt`] — on success returns the negotiated endpoint; a
///    typed `Refused` becomes [`DiscoveryRoute::Refused`]; any transport/dial
///    failure becomes [`DiscoveryRoute::DirectFallbackUnavailable`].
///
/// The returned [`BrokerSession`] (on the success path) is ready to issue framed
/// requests against the negotiated backend.
pub(crate) fn discover_via_broker(
) -> Result<(DiscoveryRoute, Option<BrokerSession>), BrokerDiscoveryError> {
    discover_via_broker_with_disabled(running_process_disabled())
}

/// Inner discovery split so tests inject the `RUNNING_PROCESS_DISABLE` decision
/// without mutating the process-global env var (which races sibling tests).
/// Mirrors `lifecycle::is_live_with_running_process_disabled`.
pub(crate) fn discover_via_broker_with_disabled(
    running_process_disabled: bool,
) -> Result<(DiscoveryRoute, Option<BrokerSession>), BrokerDiscoveryError> {
    if running_process_disabled {
        return Ok((DiscoveryRoute::DirectFallbackDisabled, None));
    }

    let endpoint = default_broker_endpoint().map_err(BrokerDiscoveryError::Endpoint)?;

    match BrokerSession::adopt(connect_request(&endpoint)) {
        Ok(session) => {
            let negotiated = session.endpoint().to_string();
            Ok((
                DiscoveryRoute::BrokerNegotiated {
                    endpoint: negotiated,
                },
                Some(session),
            ))
        }
        // Defensive: adopt already checked the env, but keep the mapping explicit
        // so the disable contract is unambiguous at this call site too.
        Err(AdoptError::BrokerDisabled) => Ok((DiscoveryRoute::DirectFallbackDisabled, None)),
        Err(AdoptError::DisableEnv(err)) => Err(BrokerDiscoveryError::Endpoint(err.to_string())),
        Err(AdoptError::Connect(err)) => match err.refusal_kind() {
            Some(kind) => Ok((DiscoveryRoute::Refused { kind }, None)),
            // Not a refusal — the broker was unreachable or the dial failed.
            // Fall back to the direct soldr-daemon path.
            None => Ok((DiscoveryRoute::DirectFallbackUnavailable, None)),
        },
        // Catch-all for variants that only exist under richer
        // running-process feature sets (e.g. `AdoptError::AsyncJoin`,
        // surfaced when zccache pulls in `client-async` through its
        // embedded service). Treat unknown adopt failures as
        // "broker unavailable" so direct discovery takes over rather
        // than propagating an opaque error. The lint allow keeps the
        // arm green in feature configurations where the enum is
        // already exhaustively matched above.
        #[allow(unreachable_patterns)]
        Err(_) => Ok((DiscoveryRoute::DirectFallbackUnavailable, None)),
    }
}

/// Lifecycle bridge: resolve the live soldr-daemon PID via the broker.
///
/// Called *ahead of* the direct `BackendHandle` probe in
/// [`crate::daemon::lifecycle::is_live`]. When broker discovery negotiates a
/// backend ([`DiscoveryRoute::BrokerNegotiated`]), the daemon is confirmed live
/// and we return the locally-recorded PID for it (the broker verifies the
/// endpoint; the PID file supplies the process id the rest of soldr's lifecycle
/// machinery expects).
///
/// Every other route — `RUNNING_PROCESS_DISABLE=1`, a typed `Refused`, an
/// unreachable broker, or a construction error — returns `None` so the caller
/// falls through to the direct soldr-daemon path. A broker problem must never
/// be a hard failure: a missing broker degrades to the pre-#434 behavior.
pub(crate) fn soldr_daemon_pid_via_broker(paths: &SoldrPaths) -> Option<u32> {
    soldr_daemon_pid_via_broker_with_disabled(paths, running_process_disabled())
}

pub(crate) fn soldr_daemon_pid_via_broker_with_disabled(
    paths: &SoldrPaths,
    running_process_disabled: bool,
) -> Option<u32> {
    match discover_via_broker_with_disabled(running_process_disabled) {
        Ok((DiscoveryRoute::BrokerNegotiated { .. }, _session)) => {
            // The broker confirmed a verified backend. Report the locally
            // recorded daemon PID so the rest of the lifecycle path is
            // unchanged. If the PID file is gone the direct probe will also
            // miss and the caller spawns a fresh daemon.
            crate::daemon::backend_handle_adoption::probe_soldr_daemon(paths)
                .map(|handle| handle.pid())
        }
        // Disabled / refused / unreachable / construction error: defer to the
        // direct path. Refusals are logged for diagnostics but never fatal.
        Ok((DiscoveryRoute::Refused { kind }, _)) => {
            log_refusal(kind);
            None
        }
        Ok(_) => None,
        Err(err) => {
            tracing::debug!(target: "soldr::broker", "broker discovery unavailable: {err}");
            None
        }
    }
}

/// Emit a typed diagnostic for a broker refusal. Mirrors the `RefusalKind`
/// branch table in `docs/INTEGRATE-BROKER.md` step 5.
fn log_refusal(kind: RefusalKind) {
    match kind {
        RefusalKind::VersionUnsupported => {
            tracing::warn!(target: "soldr::broker", "broker refused soldr-daemon: version unsupported (upgrade running-process)");
        }
        RefusalKind::VersionBlocked => {
            tracing::warn!(target: "soldr::broker", "broker refused soldr-daemon: this daemon version is blocked");
        }
        RefusalKind::ServiceUnknown => {
            tracing::warn!(target: "soldr::broker", "broker refused soldr-daemon: service unknown (install the .servicedef)");
        }
        RefusalKind::RateLimited => {
            tracing::debug!(target: "soldr::broker", "broker rate-limited soldr-daemon discovery; using direct path");
        }
        RefusalKind::ShuttingDown => {
            tracing::debug!(target: "soldr::broker", "broker shutting down; using direct soldr-daemon path");
        }
        RefusalKind::Other(code) => {
            tracing::debug!(target: "soldr::broker", "broker refused soldr-daemon: {code:?}; using direct path");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::protocol::BrokerIsolation;
    use running_process::broker::server::ServiceDefinitionLoader;
    use tempfile::TempDir;

    fn fake_daemon_binary(root: &Path) -> PathBuf {
        let binary = root.join(if cfg!(windows) {
            "soldr-daemon.exe"
        } else {
            "soldr-daemon"
        });
        std::fs::write(&binary, b"stub").expect("fake daemon binary");
        binary
    }

    crate::timed_test!(shared_broker_service_definition_targets_soldr_daemon, {
        let temp = TempDir::new().expect("tempdir");
        let daemon = fake_daemon_binary(temp.path());

        let def = soldr_daemon_service_definition(&daemon).expect("definition");

        assert_eq!(def.service_name, SOLDR_DAEMON_SERVICE_NAME);
        assert_eq!(def.isolation, BrokerIsolation::SharedBroker as i32);
        assert!(def.explicit_instance.is_empty());
        assert_eq!(def.min_version, SOLDR_DAEMON_MIN_VERSION);
        assert!(def
            .version_allow_list
            .contains(&SOLDR_DAEMON_SERVICE_VERSION.to_string()));
        assert_eq!(
            def.labels.get("consumer").map(String::as_str),
            Some("soldr")
        );
    });

    crate::timed_test!(ci_service_definition_uses_explicit_instance_trust_group, {
        let temp = TempDir::new().expect("tempdir");
        let daemon = fake_daemon_binary(temp.path());

        let def = soldr_daemon_ci_service_definition(&daemon).expect("definition");

        assert_eq!(def.isolation, BrokerIsolation::ExplicitInstance as i32);
        assert_eq!(def.explicit_instance, SOLDR_DAEMON_CI_INSTANCE);
    });

    crate::timed_test!(service_definition_install_round_trips_through_loader, {
        let temp = TempDir::new().expect("tempdir");
        let service_root = temp.path().join("services");
        let daemon = fake_daemon_binary(temp.path());

        let written = shared_broker_builder(&daemon)
            .install_in(&service_root)
            .expect("install service definition");
        assert_eq!(written, service_root.join("soldr-daemon.servicedef"));

        let loaded = ServiceDefinitionLoader::new(&service_root)
            .load(SOLDR_DAEMON_SERVICE_NAME)
            .expect("load service definition");
        assert_eq!(loaded.service_name, SOLDR_DAEMON_SERVICE_NAME);
        assert_eq!(loaded.min_version, SOLDR_DAEMON_MIN_VERSION);
    });

    crate::timed_test!(cache_manifest_records_all_five_roots, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        let manifest = soldr_daemon_cache_manifest(&paths).expect("manifest");

        assert_eq!(manifest.service_name, SOLDR_DAEMON_SERVICE_NAME);
        assert_eq!(manifest.broker_instance, "shared");
        // state + pinned-binary + runtime + lock + log == 5 roots.
        assert_eq!(manifest.roots.len(), 5);
        let kinds: Vec<i32> = manifest.roots.iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&(CacheRootKind::CacheData as i32)));
        assert!(kinds.contains(&(CacheRootKind::CacheRuntime as i32)));
        assert!(kinds.contains(&(CacheRootKind::CacheLocks as i32)));
        assert!(kinds.contains(&(CacheRootKind::CacheLogs as i32)));
        // self_sha256 is sealed by build().
        assert!(!manifest.self_sha256.is_empty());
    });

    crate::timed_test!(cache_manifest_publishes_and_round_trips, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let registry = temp.path().join("manifests");

        let written = publish_cache_manifest_in(&paths, &registry).expect("publish");
        assert!(written.exists(), "manifest file written to registry dir");

        let bytes = std::fs::read(&written).expect("read manifest back");
        assert!(!bytes.is_empty());
    });

    crate::timed_test!(roots_map_to_distinct_soldr_directories, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let roots = SoldrCacheRoots::for_paths(&paths);

        assert_eq!(roots.state, paths.root);
        assert_eq!(roots.pinned_binary, paths.pinned_bin);
        assert!(roots.runtime.starts_with(&paths.root));
        assert!(roots.lock.starts_with(&paths.root));
        assert!(roots.log.starts_with(&roots.lock));
    });

    crate::timed_test!(disable_env_takes_direct_fallback_route, {
        // Inject the disabled decision rather than mutating the global env var,
        // so this never races sibling tests that read RUNNING_PROCESS_DISABLE.
        let (route, session) = discover_via_broker_with_disabled(true).expect("discovery");
        assert_eq!(route, DiscoveryRoute::DirectFallbackDisabled);
        assert!(session.is_none());
    });

    crate::timed_test!(refusal_kinds_classify_via_error_code, {
        use running_process::broker::protocol::ErrorCode;
        // Verify the typed RefusalKind surface soldr branches on maps the v1
        // broker error codes the guide names (VersionUnsupported / VersionBlocked
        // / ServiceUnknown) — exercised without a live broker.
        assert_eq!(
            RefusalKind::from_code(ErrorCode::ErrorVersionUnsupported),
            RefusalKind::VersionUnsupported
        );
        assert_eq!(
            RefusalKind::from_code(ErrorCode::ErrorVersionBlocked),
            RefusalKind::VersionBlocked
        );
        assert_eq!(
            RefusalKind::from_code(ErrorCode::ErrorServiceUnknown),
            RefusalKind::ServiceUnknown
        );
        // A code newer than this build understands lands in Other, never a panic.
        log_refusal(RefusalKind::VersionUnsupported);
        log_refusal(RefusalKind::ServiceUnknown);
    });

    crate::timed_test!(broker_bridge_returns_none_when_disabled, {
        // With the escape hatch engaged, the bridge must report None so the
        // caller falls to the direct soldr-daemon path — and must never panic.
        // Injected flag avoids racing the global env var.
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        assert!(soldr_daemon_pid_via_broker_with_disabled(&paths, true).is_none());
    });

    crate::timed_test!(connect_request_uses_soldr_daemon_v1_hello_fields, {
        let request = connect_request("broker.sock");
        assert_eq!(request.service_name, SOLDR_DAEMON_SERVICE_NAME);
        assert_eq!(request.wanted_version, SOLDR_DAEMON_SERVICE_VERSION);
        assert_eq!(request.self_version, SOLDR_DAEMON_SERVICE_VERSION);
        // The broker-client library fills client_lib_name == "running-process".
        assert_eq!(request.client_lib_name, RUNNING_PROCESS_CLIENT_LIB_NAME);
    });
}
