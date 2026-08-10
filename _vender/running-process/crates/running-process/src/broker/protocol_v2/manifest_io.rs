//! v2 cache-manifest I/O helpers (slice 23-A of zccache#782).
//!
//! Mirrors the v1 [`super::super::manifest`] surface that consumers
//! (zccache, fbuild, soldr) use today — `CacheManifestBuilder`,
//! `write_to_root`, `write_to_central[_in_dir]`, `central_registry_dir`
//! — against the v2 [`CacheManifest`] / [`CacheRoot`] / [`CacheRootKind`]
//! types added in [`super::super::protocol_v2`].
//!
//! Per the v1↔v2 coexistence design (#470), the two write paths use
//! distinct file extensions so a single registry directory can carry
//! both formats:
//!
//! | format | per-cache-root file | central registry file |
//! |---|---|---|
//! | v1 | `.running-process-manifest.pb` | `<svc>-<ver>.pb` |
//! | v2 | `.running-process-manifest.v2.pb` | `<svc>-<ver>.v2.pb` |
//!
//! Slice 23-A ships only the WRITE side + extension constants. A
//! verifying loader (signature check, host-identity match,
//! self_sha256) lands when a v2 broker actually consumes these files
//! — separate slice in [`zackees/running-process#523`].

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use prost::Message as _;

use super::super::manifest::ManifestError;
use super::super::secure_dir;
use super::{CacheManifest, CacheRoot, CacheRootKind};

/// v2 file name written inside `<cache_root>/`. Distinct from
/// v1's `.running-process-manifest.pb` so a v1 broker never decodes
/// a v2 file by accident (and vice versa).
pub const ROOT_MANIFEST_FILE_V2: &str = ".running-process-manifest.v2.pb";

/// v2 file extension used for entries in the central manifest registry.
/// Mirrors v1's `pb` extension with a `v2.pb` distinguisher.
pub const CENTRAL_MANIFEST_EXTENSION_V2: &str = "v2.pb";

/// Constant carried in every v2 manifest's
/// [`CacheManifest::broker_envelope_version`] field. Pins the schema
/// generation from the proto side independently of the file name.
pub const BROKER_ENVELOPE_VERSION_V2: &str = "v2";

/// Return the platform central-registry directory (same path as v1).
///
/// `RUNNING_PROCESS_MANIFEST_DIR` is honored as a test override —
/// callers MUST NOT set this in production. Production callers leave
/// it unset and rely on the per-OS default.
///
/// Per v1↔v2 coexistence, v2 manifest files coexist with v1's in the
/// same directory; the file extension (`.v2.pb` vs `.pb`) is what
/// keeps them distinct.
#[must_use]
pub fn central_registry_dir_v2() -> PathBuf {
    super::super::manifest::central_registry_dir()
}

/// Builder for [`CacheManifest`]. Mirrors v1's
/// [`super::super::builders::CacheManifestBuilder`] API verbatim so
/// the consumer-side migration is a literal s/v1::/v2::/ swap.
#[derive(Debug, Clone)]
pub struct CacheManifestBuilder {
    manifest: CacheManifest,
}

impl CacheManifestBuilder {
    /// Begin a v2 manifest for `service_name` at `service_version`.
    ///
    /// Pre-populates [`CacheManifest::broker_envelope_version`] = `"v2"`
    /// plus the unix-ms `created_at` / `last_active` timestamps so the
    /// callee only has to chain `.root(...)` calls.
    #[must_use]
    pub fn new(service_name: impl Into<String>, service_version: impl Into<String>) -> Self {
        let now = now_unix_ms();
        Self {
            manifest: CacheManifest {
                service_name: service_name.into(),
                service_version: service_version.into(),
                broker_envelope_version: BROKER_ENVELOPE_VERSION_V2.to_owned(),
                created_at_unix_ms: now,
                last_active_unix_ms: now,
                ..Default::default()
            },
        }
    }

    /// Append one cache root of the given kind at `path`.
    #[must_use]
    pub fn root(mut self, kind: CacheRootKind, path: impl Into<String>) -> Self {
        self.manifest.roots.push(CacheRoot {
            kind: kind as i32,
            path: path.into(),
        });
        self
    }

    /// Set the broker instance label (e.g. `"shared"` or an
    /// explicit-instance trust group).
    #[must_use]
    pub fn broker_instance(mut self, instance: impl Into<String>) -> Self {
        self.manifest.broker_instance = instance.into();
        self
    }

    /// Set the manifest bundle id (deploy hint for multi-service bundles).
    #[must_use]
    pub fn bundle_id(mut self, bundle_id: impl Into<String>) -> Self {
        self.manifest.bundle_id = bundle_id.into();
        self
    }

    /// Finalize into a [`CacheManifest`] without writing anywhere.
    #[must_use]
    pub fn build(self) -> CacheManifest {
        self.manifest
    }

    /// Build + write into the platform's central registry, returning
    /// the written path.
    ///
    /// # Errors
    ///
    /// See [`write_to_central_v2`].
    pub fn publish(self) -> Result<PathBuf, ManifestError> {
        let manifest = self.build();
        write_to_central_v2(&manifest.service_name, &manifest.service_version, &manifest)
    }

    /// Testable variant of [`Self::publish`] with an explicit registry
    /// directory.
    ///
    /// # Errors
    ///
    /// See [`write_to_central_in_dir_v2`].
    pub fn publish_in(self, registry_dir: &Path) -> Result<PathBuf, ManifestError> {
        let manifest = self.build();
        write_to_central_in_dir_v2(
            registry_dir,
            &manifest.service_name,
            &manifest.service_version,
            &manifest,
        )
    }
}

/// Write `<cache_root>/.running-process-manifest.v2.pb` atomically.
///
/// # Errors
///
/// - [`ManifestError::Io`] on filesystem failures.
/// - [`ManifestError::InsecureRegistry`] (re-used for the cache root)
///   when the directory exists but has insecure permissions.
pub fn write_to_root_v2(cache_root: &Path, manifest: &CacheManifest) -> Result<(), ManifestError> {
    fs::create_dir_all(cache_root)?;
    secure_dir::ensure_private_dir(cache_root)?;
    let target = cache_root.join(ROOT_MANIFEST_FILE_V2);
    write_manifest_file_v2(&target, manifest)
}

/// Write `<central_registry>/{service}-{version}.v2.pb` atomically.
///
/// # Errors
///
/// See [`write_to_root_v2`] (filesystem + permissions) plus
/// [`ManifestError::InvalidName`] when `service_name` / `version`
/// fail validation.
pub fn write_to_central_v2(
    service_name: &str,
    version: &str,
    manifest: &CacheManifest,
) -> Result<PathBuf, ManifestError> {
    let dir = central_registry_dir_v2();
    write_to_central_in_dir_v2(&dir, service_name, version, manifest)
}

/// Testable variant of [`write_to_central_v2`] with an explicit
/// registry directory (tests, custom layouts).
///
/// # Errors
///
/// See [`write_to_central_v2`].
pub fn write_to_central_in_dir_v2(
    registry_dir: &Path,
    service_name: &str,
    version: &str,
    manifest: &CacheManifest,
) -> Result<PathBuf, ManifestError> {
    super::super::manifest::ensure_central_registry_dir(registry_dir)?;
    let target = central_manifest_path_v2(registry_dir, service_name, version)?;
    write_manifest_file_v2(&target, manifest)?;
    Ok(target)
}

/// Compute the v2 central-registry file path for one (service, version)
/// pair. Mirrors v1's `central_manifest_path` with the `.v2.pb` suffix.
///
/// # Errors
///
/// Surfaces [`ManifestError::InvalidName`] when the service name
/// fails validation (delegates to the shared v1 validator since the
/// name rules are cross-version-stable per #228).
pub fn central_manifest_path_v2(
    registry_dir: &Path,
    service_name: &str,
    version: &str,
) -> Result<PathBuf, ManifestError> {
    // Delegate to the shared validator to keep the rules identical to v1.
    super::super::manifest::central_manifest_path(registry_dir, service_name, version).map(
        |v1_path| {
            // v1 returns `<registry>/<svc>-<ver>.pb`. Re-stem to `.v2.pb`
            // by stripping the `.pb` and appending `.v2.pb`.
            let stem = v1_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            registry_dir.join(format!("{stem}.{CENTRAL_MANIFEST_EXTENSION_V2}"))
        },
    )
}

fn write_manifest_file_v2(target: &Path, manifest: &CacheManifest) -> Result<(), ManifestError> {
    let bytes = manifest.encode_to_vec();
    super::super::manifest::write_atomic(target, &bytes)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn root_manifest_filename_is_v2() {
        assert_eq!(ROOT_MANIFEST_FILE_V2, ".running-process-manifest.v2.pb");
    }

    #[test]
    fn central_extension_is_v2_pb() {
        assert_eq!(CENTRAL_MANIFEST_EXTENSION_V2, "v2.pb");
    }

    #[test]
    fn envelope_version_is_v2() {
        assert_eq!(BROKER_ENVELOPE_VERSION_V2, "v2");
    }

    #[test]
    fn builder_new_populates_required_fields() {
        let manifest = CacheManifestBuilder::new("svc", "1.0.0").build();
        assert_eq!(manifest.service_name, "svc");
        assert_eq!(manifest.service_version, "1.0.0");
        assert_eq!(manifest.broker_envelope_version, "v2");
        assert!(manifest.created_at_unix_ms > 0);
        assert_eq!(manifest.created_at_unix_ms, manifest.last_active_unix_ms);
        assert!(manifest.roots.is_empty());
    }

    #[test]
    fn builder_root_appends_in_order() {
        let manifest = CacheManifestBuilder::new("svc", "1.0.0")
            .root(CacheRootKind::CacheData, "/var/cache/svc")
            .root(CacheRootKind::CacheIndex, "/var/cache/svc/index")
            .root(CacheRootKind::CacheLogs, "/var/log/svc")
            .root(CacheRootKind::CacheLocks, "/var/cache/svc/locks")
            .build();
        assert_eq!(manifest.roots.len(), 4);
        assert_eq!(manifest.roots[0].kind, CacheRootKind::CacheData as i32);
        assert_eq!(manifest.roots[0].path, "/var/cache/svc");
        assert_eq!(manifest.roots[1].kind, CacheRootKind::CacheIndex as i32);
        assert_eq!(manifest.roots[2].kind, CacheRootKind::CacheLogs as i32);
        assert_eq!(manifest.roots[3].kind, CacheRootKind::CacheLocks as i32);
    }

    /// v2 wire values mirror v1's exactly so consumers that bridge the
    /// two generations (zccache, fbuild) can `as i32`-cast across
    /// without translation. Pins every variant so a future renumber
    /// forces an explicit migration of every consumer instead of
    /// silently misclassifying.
    #[test]
    fn cache_root_kind_wire_values_mirror_v1() {
        assert_eq!(CacheRootKind::Unspecified as i32, 0);
        assert_eq!(CacheRootKind::CacheData as i32, 1);
        assert_eq!(CacheRootKind::CacheLogs as i32, 2);
        assert_eq!(CacheRootKind::CacheLocks as i32, 3);
        assert_eq!(CacheRootKind::CacheRuntime as i32, 4);
        assert_eq!(CacheRootKind::CacheTmp as i32, 5);
        assert_eq!(CacheRootKind::CacheConfig as i32, 6);
        assert_eq!(CacheRootKind::CacheIndex as i32, 7);
        assert_eq!(CacheRootKind::CacheJournal as i32, 8);
        assert_eq!(CacheRootKind::CacheSecrets as i32, 9);
    }

    #[test]
    fn builder_broker_instance_and_bundle_id_round_trip() {
        let manifest = CacheManifestBuilder::new("svc", "1.0.0")
            .broker_instance("ci-trusted")
            .bundle_id("bundle-42")
            .build();
        assert_eq!(manifest.broker_instance, "ci-trusted");
        assert_eq!(manifest.bundle_id, "bundle-42");
    }

    #[test]
    fn write_to_root_v2_writes_to_canonical_filename() {
        let dir = tempdir().expect("tempdir");
        let manifest = CacheManifestBuilder::new("svc", "1.0.0")
            .root(CacheRootKind::CacheData, "/path/to/data")
            .build();
        write_to_root_v2(dir.path(), &manifest).expect("write_to_root_v2");

        let written = dir.path().join(ROOT_MANIFEST_FILE_V2);
        assert!(written.exists(), "v2 manifest file must exist");

        let bytes = fs::read(&written).expect("read");
        let decoded = CacheManifest::decode(bytes.as_slice()).expect("decode");
        assert_eq!(decoded.service_name, "svc");
        assert_eq!(decoded.roots.len(), 1);
        assert_eq!(decoded.roots[0].path, "/path/to/data");
    }

    #[test]
    fn publish_in_writes_to_central_with_v2_extension() {
        let dir = tempdir().expect("tempdir");
        let path = CacheManifestBuilder::new("svc", "1.2.3")
            .root(CacheRootKind::CacheData, "/path")
            .publish_in(dir.path())
            .expect("publish_in");

        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some("svc-1.2.3.v2.pb")
        );
        let bytes = fs::read(&path).expect("read");
        let decoded = CacheManifest::decode(bytes.as_slice()).expect("decode");
        assert_eq!(decoded.service_name, "svc");
        assert_eq!(decoded.service_version, "1.2.3");
    }

    #[test]
    fn publish_in_rejects_invalid_service_name() {
        let dir = tempdir().expect("tempdir");
        let manifest = CacheManifest {
            service_name: "BAD-Caps".to_owned(),
            service_version: "1.0.0".to_owned(),
            ..Default::default()
        };
        let err = write_to_central_in_dir_v2(dir.path(), "BAD-Caps", "1.0.0", &manifest)
            .expect_err("must reject");
        let _ = err;
    }

    /// Round-trip: write via builder, read raw bytes, assert every
    /// builder-set field survives. Pins the contract from a different
    /// angle than the standalone proto round-trip tests in `mod.rs`.
    #[test]
    fn builder_publish_round_trip_preserves_every_field() {
        let dir = tempdir().expect("tempdir");
        let path = CacheManifestBuilder::new("zccache", "1.12.9")
            .root(CacheRootKind::CacheData, "/var/cache/zccache/data")
            .root(CacheRootKind::CacheIndex, "/var/cache/zccache/index")
            .root(CacheRootKind::CacheLogs, "/var/log/zccache")
            .broker_instance("shared")
            .bundle_id("zccache-bundle-v1")
            .publish_in(dir.path())
            .expect("publish_in");

        let bytes = fs::read(&path).expect("read");
        let decoded = CacheManifest::decode(bytes.as_slice()).expect("decode");
        assert_eq!(decoded.service_name, "zccache");
        assert_eq!(decoded.service_version, "1.12.9");
        assert_eq!(decoded.broker_envelope_version, "v2");
        assert_eq!(decoded.roots.len(), 3);
        assert_eq!(decoded.broker_instance, "shared");
        assert_eq!(decoded.bundle_id, "zccache-bundle-v1");
        assert!(decoded.created_at_unix_ms > 0);
    }

    /// Coexistence: a v1 file (`.pb`) and a v2 file (`.v2.pb`) for the
    /// same (service, version) can live in the same registry dir
    /// without colliding.
    #[test]
    fn v2_central_filename_does_not_collide_with_v1() {
        let dir = tempdir().expect("tempdir");
        let v1_path = dir.path().join("zccache-1.12.9.pb");
        let v2_path = central_manifest_path_v2(dir.path(), "zccache", "1.12.9").unwrap();
        assert_ne!(v1_path, v2_path);
        assert_eq!(
            v2_path.file_name().and_then(|s| s.to_str()),
            Some("zccache-1.12.9.v2.pb")
        );
    }
}
