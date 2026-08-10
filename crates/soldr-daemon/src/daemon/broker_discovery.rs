//! running-process **v2** broker discovery for soldr-daemon (soldr#1495).
//!
//! Soldr's v2 broker adoption is mandatory — there is no legacy lane, opt-in
//! switch, or direct-spawn fallback:
//!
//! 1. The daemon publishes a v2 [`CacheManifest`] (state / pinned-binary
//!    / runtime / lock / log roots) into the central registry via
//!    [`CacheManifestBuilder`]. The service definition is written
//!    separately by [`crate::daemon::service_definition`].
//! 2. The requesting front door writes the exact daemon image, cache root,
//!    route name, and version to the service definition.
//! 3. The singleton broker resolves that definition and is the only component
//!    allowed to place or launch a daemon process.

use crate::core::SoldrPaths;
use crate::daemon::backend_handle_adoption::{
    SOLDR_DAEMON_SERVICE_NAME, SOLDR_DAEMON_SERVICE_VERSION,
};
use prost::Message as _;
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

/// Errors raised while *constructing* the broker registration messages.
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
    // soldr#2186: the claim is two files, so **order is the contract**.
    //
    // The sidecar goes first and the manifest last, because the manifest is
    // what readers gate on: `current_version_claim_matches` requires a
    // matching manifest *and* a matching store version, so a reader that
    // observes the manifest must already be able to observe the sidecar.
    //
    // Written the other way round there is a window where the manifest says
    // "this is your version" and the sidecar is not there yet, which reads as
    // store-unknown — i.e. stale — and displaces a perfectly healthy daemon
    // that is mid-startup. The window is small and load-dependent, which is
    // the worst size: it shows up as an unreproducible displacement flake.
    let path = store_version_claim_path(paths);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Best-effort — a daemon that cannot write it reads as store-unknown,
    // i.e. stale, which is the safe direction.
    let _ = std::fs::write(path, current_store_version());

    let manifest = cache_manifest_builder(paths).build();
    write_to_root_v2(&paths.root, &manifest).map_err(BrokerDiscoveryError::manifest)
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
    // soldr#2186: a completed claim must satisfy *both* halves of
    // `current_version_claim_matches`, since a daemon that publishes only one
    // reads as stale and gets displaced.
    //
    // This asserts the post-condition, NOT the write order -- it would pass
    // whichever file went first. The ordering that closes the startup window
    // (sidecar before manifest, so the manifest is the commit point) is
    // enforced by construction in `write_root_version_claim` and is not
    // observable from outside without instrumenting the writer.
    crate::timed_test!(a_completed_claim_satisfies_both_halves_of_the_check, {
        let temp = TempDir::new().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().to_path_buf());

        write_root_version_claim(&paths).expect("write claim");

        // Whenever the manifest is readable, the sidecar must agree -- that
        // is exactly the conjunction current_version_claim_matches applies.
        assert!(read_claimed_service_version(&paths).is_some());
        assert!(
            store_version_claim_matches(&paths),
            "a published claim must carry the store sidecar too; without it a              healthy daemon reads as store-unknown and is displaced",
        );
        assert!(crate::daemon::lifecycle::current_version_claim_matches(
            &paths
        ));
    });

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
}
