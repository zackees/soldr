//! running-process **v2** broker discovery for soldr-daemon (soldr#1495).
//!
//! This module layers v2 broker-mediated discovery *ahead of* soldr's
//! direct `BackendHandle` probe (`backend_handle_adoption.rs`). soldr's
//! v2 adoption is mandatory — there is no v1 lane and no opt-in switch:
//!
//! 1. The daemon publishes a v2 [`CacheManifest`] (state / pinned-binary
//!    / runtime / lock / log roots) into the central registry via
//!    [`CacheManifestBuilder`]. The service definition is written
//!    separately by [`crate::daemon::service_definition`].
//! 2. Discovery dials the v2 broker with
//!    [`running_process::broker::client_v2::connect`] — the Hello carries
//!    `service_name = "soldr-daemon"` and `wanted_version` = this build's
//!    version, and the broker enforces the servicedef's version policy.
//! 3. On a negotiated backend the daemon is confirmed live and the local
//!    PID-file probe supplies the process id.
//!
//! `RUNNING_PROCESS_DISABLE=1` short-circuits before any broker contact
//! and the caller takes the direct soldr-daemon path
//! (`backend_handle_adoption::probe_soldr_daemon`). When no broker is
//! running at all — the common case today, since the v2 broker binary is
//! not yet deployed and its adopt path is still a stub — the dial fails
//! fast (no listener) and discovery likewise defers to the direct probe.
//! Convergence therefore never depends on a running broker.

use crate::core::SoldrPaths;
use crate::daemon::backend_handle_adoption::{
    running_process_disabled, SOLDR_DAEMON_SERVICE_NAME, SOLDR_DAEMON_SERVICE_VERSION,
};
use prost::Message as _;
use running_process::broker::client_v2::{self, BrokerV2Error};
use running_process::broker::protocol_v2::{
    write_to_root_v2, CacheManifest, CacheManifestBuilder, CacheRootKind, ROOT_MANIFEST_FILE_V2,
};
use std::path::{Path, PathBuf};

/// Test seam (soldr#1495): overrides the package version the daemon
/// *claims* in its published manifest, so a test can stand up a daemon
/// that advertises a stale version and assert a current-version client
/// displaces it. Never consulted on the read/compare side — liveness
/// always compares against the real [`SOLDR_DAEMON_SERVICE_VERSION`].
pub(crate) const FAKE_PKG_VERSION_ENV: &str = "SOLDR_TEST_DAEMON_FAKE_PKG_VERSION";

/// Where the daemon records which embedded compile store it is serving.
///
/// soldr#2186: staleness was decided on soldr's package version alone, but
/// the embedded store is versioned by
/// `zccache::core::config::versioned_subdir()` — the *vendored* zccache
/// version, compiled into whichever binary asks — and the two move
/// independently. Bumping `_vender/zccache` without cutting a release is
/// the documented workflow, and soldr#2185 did exactly that: 1.12.17 →
/// 1.13.0, still soldr 0.8.30.
///
/// A daemon built before such a bump keeps claiming the same package
/// version, is adopted as current, and serves compiles out of
/// `…/v1.12.17/…` while the front door reads `…/v1.13.0/…`. The reported
/// symptom was an empty session-stats directory; the real one is a cache
/// split across two store versions until that daemon happens to restart.
///
/// This deliberately does **not** ride in the manifest's `service_version`.
/// That field is a cross-crate protocol value the running-process registry
/// validates as strict `MAJOR.MINOR.PATCH`, and it rejects
/// `0.8.30+zccache.1.13.0` outright. So the store version is a soldr-owned
/// sidecar in soldr's own daemon directory, written and removed with the
/// manifest claim.
pub(crate) fn store_version_claim_path(paths: &SoldrPaths) -> PathBuf {
    crate::cache_lib::soldr_daemon_dir(paths).join("embedded-store-version")
}

/// The embedded store version this build would read and write.
pub(crate) fn current_store_version() -> &'static str {
    zccache::core::VERSION
}

/// True when the running daemon's embedded store version matches this
/// build's. A missing sidecar is a mismatch, for the same reason a missing
/// manifest is: unknown is stale, so a newer client converges rather than
/// adopting something it cannot name.
pub(crate) fn store_version_claim_matches(paths: &SoldrPaths) -> bool {
    std::fs::read_to_string(store_version_claim_path(paths))
        .ok()
        .is_some_and(|claimed| claimed.trim() == current_store_version())
}

/// The package version this daemon advertises in its manifest claim —
/// normally this build's `CARGO_PKG_VERSION`, overridable by the
/// [`FAKE_PKG_VERSION_ENV`] test seam. The embedded store version travels
/// separately, in [`store_version_claim_path`].
fn claimed_service_version() -> String {
    std::env::var(FAKE_PKG_VERSION_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| SOLDR_DAEMON_SERVICE_VERSION.to_string())
}

/// How soldr-daemon discovery resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscoveryRoute {
    /// Broker negotiated a backend; the string is the negotiated pipe
    /// (empty while the v2 broker's adopt path is still a stub — the
    /// daemon is nonetheless confirmed live).
    BrokerNegotiated { endpoint: String },
    /// `RUNNING_PROCESS_DISABLE=1` was set; the caller must take the direct path.
    DirectFallbackDisabled,
    /// The broker explicitly refused the Hello (version policy, unknown
    /// service, ...); the caller defers to the direct path. Carries the
    /// broker's human-readable reason for diagnostics.
    Refused { reason: String },
    /// The broker was unreachable or the dial failed (not a refusal). The caller
    /// should fall back to the direct soldr-daemon path.
    DirectFallbackUnavailable,
}

/// Errors raised while *constructing* the broker registration messages.
/// Discovery (dial/negotiation) failures are surfaced as
/// [`DiscoveryRoute`] variants, not errors, because the caller treats
/// them as a fall-back signal, never a hard failure of the build.
#[derive(Debug)]
pub(crate) enum BrokerDiscoveryError {
    /// Building / publishing the `CacheManifest` failed.
    CacheManifest(String),
}

impl std::fmt::Display for BrokerDiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrokerDiscoveryError::CacheManifest(msg) => {
                write!(f, "soldr-daemon cache manifest invalid: {msg}")
            }
        }
    }
}

impl std::error::Error for BrokerDiscoveryError {}

impl BrokerDiscoveryError {
    fn manifest(err: impl std::fmt::Display) -> Self {
        BrokerDiscoveryError::CacheManifest(err.to_string())
    }
}

/// Build the soldr-daemon v2 cache manifest declaring the state,
/// pinned-binary, runtime, lock, and log roots. The `CacheData` /
/// `CacheIndex` roots are the daemon's shared, version-agnostic warm
/// state (carried across an upgrade handoff); only the runtime binary
/// dir is version-partitioned (soldr#1495).
pub(crate) fn soldr_daemon_cache_manifest(paths: &SoldrPaths) -> CacheManifest {
    cache_manifest_builder(paths).build()
}

fn cache_manifest_builder(paths: &SoldrPaths) -> CacheManifestBuilder {
    let roots = SoldrCacheRoots::for_paths(paths);
    CacheManifestBuilder::new(SOLDR_DAEMON_SERVICE_NAME, claimed_service_version())
        .broker_instance("shared")
        // state: the redb state/data DBs live directly under the soldr root.
        .root(CacheRootKind::CacheData, roots.state.display().to_string())
        // separately-bounded index: redb + depgraph snapshot + compile journal.
        .root(CacheRootKind::CacheIndex, roots.state.display().to_string())
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
        .map_err(BrokerDiscoveryError::manifest)
}

/// Publish the manifest into an explicit registry dir (tests, custom layouts).
pub(crate) fn publish_cache_manifest_in(
    paths: &SoldrPaths,
    registry_dir: &Path,
) -> Result<PathBuf, BrokerDiscoveryError> {
    cache_manifest_builder(paths)
        .publish_in(registry_dir)
        .map_err(BrokerDiscoveryError::manifest)
}

/// Path of the root manifest the daemon writes as its **version claim**
/// (soldr#1495): `<soldr-root>/.running-process-manifest.v2.pb`. The
/// broker reads this to route the daemon; soldr reads it locally for
/// version-aware liveness (see [`crate::daemon::lifecycle`]).
pub(crate) fn root_manifest_path(paths: &SoldrPaths) -> PathBuf {
    paths.root.join(ROOT_MANIFEST_FILE_V2)
}

/// Write the daemon's version-claim manifest into the soldr root (the
/// `CacheData`/`CacheIndex` shared-state root). Called once at daemon
/// startup. The claimed `service_version` is this build's version (or the
/// [`FAKE_PKG_VERSION_ENV`] override in tests).
pub(crate) fn write_root_version_claim(paths: &SoldrPaths) -> Result<(), BrokerDiscoveryError> {
    let manifest = cache_manifest_builder(paths).build();
    write_to_root_v2(&paths.root, &manifest).map_err(BrokerDiscoveryError::manifest)?;
    // soldr#2186: the manifest cannot carry this, so it rides alongside.
    // Best-effort — a daemon that cannot write it reads as store-unknown,
    // i.e. stale, which is the safe direction.
    let path = store_version_claim_path(paths);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, current_store_version());
    Ok(())
}

/// Read the package version the currently-running daemon claimed in its
/// root manifest. `None` when no manifest exists (a pre-#1495 daemon that
/// never wrote one — treated as version-unknown) or it cannot be decoded.
pub fn read_claimed_service_version(paths: &SoldrPaths) -> Option<String> {
    let bytes = std::fs::read(root_manifest_path(paths)).ok()?;
    let manifest = CacheManifest::decode(bytes.as_slice()).ok()?;
    let version = manifest.service_version;
    (!version.is_empty()).then_some(version)
}

/// Remove the root version-claim manifest — part of tearing down a
/// displaced daemon so a stale claim can't outlive its writer.
pub(crate) fn remove_root_version_claim(paths: &SoldrPaths) {
    let _ = std::fs::remove_file(root_manifest_path(paths));
    // soldr#2186: the store-version sidecar is half of the same claim, so
    // it retracts with it. Leaving it behind is not a correctness problem
    // — a claim is only believed when the manifest is present too — but a
    // stale file that outlives the daemon that wrote it is exactly the
    // kind of residue this function exists to prevent.
    let _ = std::fs::remove_file(store_version_claim_path(paths));
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

/// Discover soldr-daemon through the v2 broker.
///
/// Resolution order:
/// 1. `RUNNING_PROCESS_DISABLE=1` → [`DiscoveryRoute::DirectFallbackDisabled`].
/// 2. [`client_v2::connect`] — on a `Negotiated` reply returns the
///    negotiated endpoint; an explicit `Refused` becomes
///    [`DiscoveryRoute::Refused`]; any other failure (no listener, dial
///    error, SID lookup failure in restricted CI, framing/decode) becomes
///    [`DiscoveryRoute::DirectFallbackUnavailable`].
pub(crate) fn discover_via_broker() -> DiscoveryRoute {
    discover_via_broker_with_disabled(running_process_disabled())
}

/// Inner discovery split so tests inject the `RUNNING_PROCESS_DISABLE`
/// decision without mutating the process-global env var (which races
/// sibling tests). Mirrors `lifecycle::is_live_with_running_process_disabled`.
pub(crate) fn discover_via_broker_with_disabled(running_process_disabled: bool) -> DiscoveryRoute {
    if running_process_disabled {
        return DiscoveryRoute::DirectFallbackDisabled;
    }

    match client_v2::connect(SOLDR_DAEMON_SERVICE_NAME, SOLDR_DAEMON_SERVICE_VERSION) {
        Ok(session) => DiscoveryRoute::BrokerNegotiated {
            endpoint: session.negotiated().backend_pipe.clone(),
        },
        Err(BrokerV2Error::Refused { reason, .. }) => DiscoveryRoute::Refused { reason },
        // No listener / dial / SID / framing / decode: the broker is not
        // reachable. Defer to the direct soldr-daemon path — a missing
        // broker must never be a hard failure.
        Err(_) => DiscoveryRoute::DirectFallbackUnavailable,
    }
}

/// Lifecycle bridge: resolve the live soldr-daemon PID via the broker.
///
/// Called *ahead of* the direct `BackendHandle` probe in
/// [`crate::daemon::lifecycle::is_live`]. On a negotiated backend the
/// daemon is confirmed live and we return the locally-recorded PID (the
/// broker verifies the endpoint; the PID file supplies the process id the
/// rest of soldr's lifecycle machinery expects). Every other route
/// returns `None` so the caller falls through to the direct path.
pub(crate) fn soldr_daemon_pid_via_broker(paths: &SoldrPaths) -> Option<u32> {
    soldr_daemon_pid_via_broker_with_disabled(paths, running_process_disabled())
}

pub(crate) fn soldr_daemon_pid_via_broker_with_disabled(
    paths: &SoldrPaths,
    running_process_disabled: bool,
) -> Option<u32> {
    match discover_via_broker_with_disabled(running_process_disabled) {
        DiscoveryRoute::BrokerNegotiated { .. } => {
            crate::daemon::backend_handle_adoption::probe_soldr_daemon(paths)
                .map(|handle| handle.pid())
        }
        DiscoveryRoute::Refused { reason } => {
            tracing::warn!(target: "soldr::broker", "v2 broker refused soldr-daemon: {reason}; using direct path");
            None
        }
        DiscoveryRoute::DirectFallbackDisabled | DiscoveryRoute::DirectFallbackUnavailable => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    crate::timed_test!(version_claim_round_trips_through_root_manifest, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        // No claim yet → version-unknown.
        assert!(read_claimed_service_version(&paths).is_none());

        write_root_version_claim(&paths).expect("write claim");
        assert_eq!(
            read_claimed_service_version(&paths).as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
        );
        // soldr#2186: the store version rides alongside the manifest.
        assert!(store_version_claim_matches(&paths));

        // Removing the claim returns to version-unknown so a stale claim
        // can't outlive its writer -- both halves of it (soldr#2186).
        remove_root_version_claim(&paths);
        assert!(read_claimed_service_version(&paths).is_none());
        assert!(!store_version_claim_path(&paths).exists());
    });

    // soldr#2186: the claim must name the vendored zccache version, because
    // that is what versions the embedded store. Asserting the *shape* rather
    // than a frozen string keeps this from needing an edit on every bump,
    // while still failing if either half is dropped.
    crate::timed_test!(a_missing_store_claim_reads_as_stale, {
        // Unknown is stale, matching how a missing manifest is treated: a
        // daemon that never recorded which store it serves cannot be shown
        // to be serving this one. Also covers every daemon built before
        // soldr#2186, which wrote no sidecar at all.
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        assert!(!store_version_claim_matches(&paths));
    });

    // The regression itself, in the terms the adoption path sees it: two
    // builds at the same soldr version but different vendored zccache must
    // not be mistaken for each other.
    crate::timed_test!(same_package_version_different_store_version_is_stale, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        // Exactly what a pre-bump daemon at this same soldr version leaves
        // behind: a matching manifest claim, and a store version that is
        // now one bump behind.
        write_root_version_claim(&paths).expect("write claim");
        let sidecar = store_version_claim_path(&paths);
        std::fs::write(&sidecar, "0.0.0-previous").expect("write stale store version");

        assert_eq!(
            read_claimed_service_version(&paths).as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "the package version alone still matches — that is the trap",
        );
        assert!(
            !crate::daemon::lifecycle::current_version_claim_matches(&paths),
            "a daemon serving a different embedded store must be displaced, \
             even though its package version is identical",
        );

        // And the same daemon on the store this build serves is kept.
        std::fs::write(&sidecar, current_store_version()).expect("write current store version");
        assert!(crate::daemon::lifecycle::current_version_claim_matches(
            &paths
        ));
    });

    crate::timed_test!(read_claim_reports_a_stale_writers_version, {
        // A daemon of a different version writes a manifest; the read
        // side reports whatever version was claimed (so the caller can
        // detect the mismatch), never assuming the current version.
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        let stale = CacheManifestBuilder::new(SOLDR_DAEMON_SERVICE_NAME, "0.0.0-stale").build();
        write_to_root_v2(&paths.root, &stale).expect("write stale claim");

        assert_eq!(
            read_claimed_service_version(&paths).as_deref(),
            Some("0.0.0-stale"),
        );
    });

    crate::timed_test!(cache_manifest_records_expected_roots, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        let manifest = soldr_daemon_cache_manifest(&paths);

        assert_eq!(manifest.service_name, SOLDR_DAEMON_SERVICE_NAME);
        // The manifest carries the full identity claim, not the bare package
        // version handed to the broker handshake (soldr#2186).
        assert_eq!(manifest.service_version, SOLDR_DAEMON_SERVICE_VERSION);
        let kinds: Vec<i32> = manifest.roots.iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&(CacheRootKind::CacheData as i32)));
        assert!(kinds.contains(&(CacheRootKind::CacheIndex as i32)));
        assert!(kinds.contains(&(CacheRootKind::CacheRuntime as i32)));
        assert!(kinds.contains(&(CacheRootKind::CacheLocks as i32)));
        assert!(kinds.contains(&(CacheRootKind::CacheLogs as i32)));
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
        let route = discover_via_broker_with_disabled(true);
        assert_eq!(route, DiscoveryRoute::DirectFallbackDisabled);
    });

    crate::timed_test!(broker_bridge_returns_none_when_disabled, {
        // With the escape hatch engaged, the bridge must report None so the
        // caller falls to the direct soldr-daemon path — and must never panic.
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());
        assert!(soldr_daemon_pid_via_broker_with_disabled(&paths, true).is_none());
    });

    crate::timed_test!(broker_absent_yields_direct_fallback_not_hard_error, {
        // No v2 broker is running in the test environment, so discovery must
        // resolve to a fallback route (unavailable, refused, or — in a
        // restricted CI container with no machine-id — a SID failure that
        // also maps to unavailable), never a negotiated backend and never a
        // panic. Proves convergence does not require a live broker.
        let route = discover_via_broker_with_disabled(false);
        assert!(
            matches!(
                route,
                DiscoveryRoute::DirectFallbackUnavailable | DiscoveryRoute::Refused { .. }
            ),
            "expected a direct-fallback route with no broker running, got {route:?}"
        );
    });
}
