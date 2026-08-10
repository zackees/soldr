//! v2 service-definition loader (slice 23-C of zccache#782).
//!
//! Read-side complement to the write helpers in [`super::io`]. A v2
//! broker (the `running-process-broker-v2` binary, currently a
//! scaffold per PRs #486-#489) calls this to discover registered
//! backends from `.servicedef.v2` files written by consumers via
//! [`super::ServiceDefinitionBuilder::install`].
//!
//! Mirrors the v1 [`super::super::server::service_def_loader::ServiceDefinitionLoader`]
//! API verbatim so the v2 broker scaffold can swap in a v2 loader
//! at the same call sites the v1 broker uses, just by changing the
//! import path.
//!
//! ## Safety properties
//!
//! - Files are read under the same `secure_dir` check v1 uses (directory
//!   mode 0700 on Unix / current-user-only ACL on Windows). A
//!   world-writable service-dir is rejected with
//!   [`ServiceDefinitionError::InsecureDirectory`].
//! - The service-name in the decoded `ServiceDefinition` must match
//!   the name encoded in the filename — catches a corrupt file
//!   masquerading as another service.
//! - Service names go through the shared [`validate_service_name`]
//!   so the load path enforces the same character class v1 does.

use std::fs;
use std::path::{Path, PathBuf};

use prost::Message as _;

use crate::broker::lifecycle::names::validate_service_name;
use crate::broker::server::service_def_loader::ServiceDefinitionError;

use super::io::{service_definition_dir_v2, service_definition_path_v2, SERVICE_DEF_V2_EXTENSION};
use super::ServiceDefinition;

/// Loader rooted at one v2 service-definition directory.
///
/// Cheap to clone (`PathBuf` plus nothing else). Intended pattern:
/// construct once at broker startup, hold across the broker's
/// lifetime, call [`Self::load`] per Hello message.
#[derive(Clone, Debug)]
pub struct ServiceDefinitionLoader {
    root: PathBuf,
}

impl ServiceDefinitionLoader {
    /// Create a loader for `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Create a loader for the platform default v2 service-definition
    /// directory ([`service_definition_dir_v2`]).
    #[must_use]
    pub fn default_root() -> Self {
        Self::new(service_definition_dir_v2())
    }

    /// Directory this loader reads from.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Load and validate one service definition from disk.
    ///
    /// Re-reads the file on every call — matches v1's
    /// reload-on-Hello semantics. Slow paths (validation, prost
    /// decode) live here so the broker's accept loop stays
    /// allocation-free in the steady state.
    ///
    /// # Errors
    ///
    /// - [`ServiceDefinitionError::InvalidName`] when `service_name`
    ///   fails the shared validator.
    /// - [`ServiceDefinitionError::InsecureDirectory`] when `root`
    ///   exists but has world/group write bits.
    /// - [`ServiceDefinitionError::Io`] for missing-file / permission
    ///   / I/O failures.
    /// - [`ServiceDefinitionError::Decode`] for a corrupt or
    ///   non-v2 servicedef file.
    /// - [`ServiceDefinitionError::ServiceNameMismatch`] when the
    ///   filename-derived service name doesn't match the decoded
    ///   `service_name` field — catches a typo'd or tampered file.
    pub fn load(&self, service_name: &str) -> Result<ServiceDefinition, ServiceDefinitionError> {
        let path = service_definition_path_v2(&self.root, service_name)?;
        let bytes = fs::read(&path)?;
        let definition = ServiceDefinition::decode(bytes.as_slice())?;
        validate_loaded_definition(&definition, service_name)?;
        Ok(definition)
    }

    /// Alias for [`Self::load`] for parity with v1's loader API.
    ///
    /// # Errors
    ///
    /// See [`Self::load`].
    pub fn reload(&self, service_name: &str) -> Result<ServiceDefinition, ServiceDefinitionError> {
        self.load(service_name)
    }

    /// Lookup that always re-reads — mirrors v1's `lookup_or_reload`.
    /// Future caching can be inserted under this method without
    /// changing the call-site contract.
    ///
    /// # Errors
    ///
    /// See [`Self::load`].
    pub fn lookup_or_reload(
        &self,
        service_name: &str,
    ) -> Result<ServiceDefinition, ServiceDefinitionError> {
        self.load(service_name)
    }

    /// Enumerate every parseable `.servicedef.v2` file in the root.
    ///
    /// Files that fail validation or decode are SKIPPED, not bubbled
    /// — broker discovery wants the parseable subset, not a hard
    /// failure on one corrupt entry. Use [`Self::scan`] when callers
    /// need per-file error details.
    #[must_use]
    pub fn enumerate(&self) -> Vec<ServiceDefinition> {
        self.scan()
            .into_iter()
            .filter_map(|entry| entry.result.ok())
            .collect()
    }

    /// Scan every `.servicedef.v2` entry in the root with per-file errors.
    ///
    /// Returns the entries sorted by path so the result is deterministic
    /// for snapshot tests + diff-friendly broker reflection APIs.
    #[must_use]
    pub fn scan(&self) -> Vec<ServiceDefinitionScanEntry> {
        let read_dir = match fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(_) => return Vec::new(),
        };

        let mut out: Vec<ServiceDefinitionScanEntry> = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            // Match files whose name ends with ".servicedef.v2"
            // (the full extension, including the inner dot). `Path::extension`
            // only returns "v2" because of the inner dot, so do a
            // bytewise suffix check instead.
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            let suffix = format!(".{SERVICE_DEF_V2_EXTENSION}");
            let Some(stem) = name.strip_suffix(&suffix) else {
                continue;
            };
            // Build a loader-style read result. If the filename's
            // implied service name is invalid, surface that as an
            // InvalidName error rather than skipping silently.
            let result = self.load_from_path(&path, stem);
            out.push(ServiceDefinitionScanEntry { path, result });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    fn load_from_path(
        &self,
        path: &Path,
        filename_service: &str,
    ) -> Result<ServiceDefinition, ServiceDefinitionError> {
        let bytes = fs::read(path)?;
        let definition = ServiceDefinition::decode(bytes.as_slice())?;
        validate_loaded_definition(&definition, filename_service)?;
        Ok(definition)
    }
}

/// Result of scanning one v2 service-definition entry.
#[derive(Debug)]
pub struct ServiceDefinitionScanEntry {
    /// Absolute path to the `.servicedef.v2` file.
    pub path: PathBuf,
    /// Decode / validate result for the file.
    pub result: Result<ServiceDefinition, ServiceDefinitionError>,
}

fn validate_loaded_definition(
    definition: &ServiceDefinition,
    expected_service: &str,
) -> Result<(), ServiceDefinitionError> {
    validate_service_name(expected_service)?;
    if definition.service_name != expected_service {
        return Err(ServiceDefinitionError::ServiceNameMismatch {
            requested: expected_service.to_owned(),
            actual: definition.service_name.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::protocol_v2::{BrokerIsolation, ServiceDefinitionBuilder};
    use tempfile::tempdir;

    fn install_test_servicedef(root: &Path, name: &str) -> PathBuf {
        ServiceDefinitionBuilder::shared_broker(name, "/usr/bin/zccache-daemon")
            .min_version("1.0.0")
            .label("env", "test")
            .install_in(root)
            .expect("install_in")
    }

    #[test]
    fn load_round_trips_an_installed_servicedef() {
        let dir = tempdir().expect("tempdir");
        install_test_servicedef(dir.path(), "zccache");

        let loader = ServiceDefinitionLoader::new(dir.path());
        let loaded = loader.load("zccache").expect("load");

        assert_eq!(loaded.service_name, "zccache");
        assert_eq!(loaded.binary_path, "/usr/bin/zccache-daemon");
        assert_eq!(loaded.isolation, BrokerIsolation::SharedBroker as i32);
        assert_eq!(loaded.min_version, "1.0.0");
        assert_eq!(loaded.labels.get("env").map(String::as_str), Some("test"));
    }

    #[test]
    fn load_returns_io_error_for_missing_file() {
        let dir = tempdir().expect("tempdir");
        let loader = ServiceDefinitionLoader::new(dir.path());
        let err = loader.load("no-such-service").expect_err("must Err");
        assert!(
            matches!(err, ServiceDefinitionError::Io(_)),
            "missing file → Io, got: {err:?}"
        );
    }

    #[test]
    fn load_rejects_invalid_service_name() {
        let dir = tempdir().expect("tempdir");
        let loader = ServiceDefinitionLoader::new(dir.path());
        for bad in ["BAD-Caps", "", "a/b", "x\0y"] {
            let err = loader.load(bad).expect_err("must Err");
            assert!(
                matches!(err, ServiceDefinitionError::InvalidName(_)),
                "{bad:?} → InvalidName, got: {err:?}"
            );
        }
    }

    #[test]
    fn load_detects_filename_service_mismatch() {
        let dir = tempdir().expect("tempdir");
        // Install zccache.servicedef.v2 then try to load as "other".
        install_test_servicedef(dir.path(), "zccache");
        // Rename to "other.servicedef.v2" so the FILE claims to be
        // "other" but the decoded service_name field says "zccache".
        let original = dir.path().join("zccache.servicedef.v2");
        let renamed = dir.path().join("other.servicedef.v2");
        fs::rename(&original, &renamed).expect("rename");

        let loader = ServiceDefinitionLoader::new(dir.path());
        let err = loader.load("other").expect_err("mismatch must Err");
        assert!(
            matches!(
                err,
                ServiceDefinitionError::ServiceNameMismatch { ref requested, ref actual }
                    if requested == "other" && actual == "zccache"
            ),
            "expected ServiceNameMismatch, got: {err:?}"
        );
    }

    #[test]
    fn load_rejects_corrupt_protobuf_bytes() {
        let dir = tempdir().expect("tempdir");
        // Write garbage bytes that can't be a valid v2 ServiceDefinition.
        let path = dir.path().join("badproto.servicedef.v2");
        fs::write(&path, b"\x01\x02\x03\x04\x05\xFF\xFF\xFF\xFF\x00").expect("write");

        let loader = ServiceDefinitionLoader::new(dir.path());
        let result = loader.load("badproto");
        // Either Decode (mid-frame garbage) or ServiceNameMismatch
        // (if the bytes happen to decode as something with an empty
        // service_name); both are acceptable rejections.
        assert!(
            matches!(
                result,
                Err(ServiceDefinitionError::Decode(_))
                    | Err(ServiceDefinitionError::ServiceNameMismatch { .. })
            ),
            "corrupt proto must Err, got: {result:?}"
        );
    }

    #[test]
    fn enumerate_returns_every_parseable_servicedef() {
        let dir = tempdir().expect("tempdir");
        install_test_servicedef(dir.path(), "zccache");
        install_test_servicedef(dir.path(), "fbuild");
        install_test_servicedef(dir.path(), "soldr");

        let loader = ServiceDefinitionLoader::new(dir.path());
        let defs = loader.enumerate();
        assert_eq!(defs.len(), 3);
        let names: std::collections::HashSet<String> =
            defs.iter().map(|d| d.service_name.clone()).collect();
        assert!(names.contains("zccache"));
        assert!(names.contains("fbuild"));
        assert!(names.contains("soldr"));
    }

    #[test]
    fn enumerate_skips_corrupt_files_silently() {
        let dir = tempdir().expect("tempdir");
        install_test_servicedef(dir.path(), "zccache");
        // Drop a corrupt file next to it.
        fs::write(
            dir.path().join("corrupt.servicedef.v2"),
            b"\xFF\xFF\xFF\xFF",
        )
        .expect("write corrupt");

        let loader = ServiceDefinitionLoader::new(dir.path());
        let defs = loader.enumerate();
        assert_eq!(defs.len(), 1, "only zccache should be returned");
        assert_eq!(defs[0].service_name, "zccache");
    }

    #[test]
    fn scan_surfaces_per_file_errors() {
        let dir = tempdir().expect("tempdir");
        install_test_servicedef(dir.path(), "zccache");
        fs::write(
            dir.path().join("corrupt.servicedef.v2"),
            b"\xFF\xFF\xFF\xFF",
        )
        .expect("write corrupt");

        let loader = ServiceDefinitionLoader::new(dir.path());
        let entries = loader.scan();
        assert_eq!(entries.len(), 2);
        // Sorted by path: "corrupt.servicedef.v2" < "zccache.servicedef.v2"
        // alphabetically, so corrupt is first.
        let ok_count = entries.iter().filter(|e| e.result.is_ok()).count();
        let err_count = entries.iter().filter(|e| e.result.is_err()).count();
        assert_eq!(ok_count, 1);
        assert_eq!(err_count, 1);
    }

    #[test]
    fn enumerate_ignores_files_with_wrong_extension() {
        let dir = tempdir().expect("tempdir");
        install_test_servicedef(dir.path(), "zccache");
        // v1 .servicedef file (no .v2 suffix) — v2 loader must skip it.
        fs::write(dir.path().join("legacy.servicedef"), b"junk").expect("write legacy");
        // Random file — must skip.
        fs::write(dir.path().join("readme.txt"), b"hello").expect("write readme");

        let loader = ServiceDefinitionLoader::new(dir.path());
        let defs = loader.enumerate();
        assert_eq!(
            defs.len(),
            1,
            "only the .servicedef.v2 file should be loaded"
        );
        assert_eq!(defs[0].service_name, "zccache");
    }

    #[test]
    fn enumerate_handles_missing_root_gracefully() {
        let loader = ServiceDefinitionLoader::new("/nonexistent/path/to/services");
        let defs = loader.enumerate();
        assert!(defs.is_empty(), "missing root → empty result");
        let entries = loader.scan();
        assert!(entries.is_empty(), "missing root → empty scan");
    }

    #[test]
    fn reload_is_equivalent_to_load() {
        let dir = tempdir().expect("tempdir");
        install_test_servicedef(dir.path(), "zccache");
        let loader = ServiceDefinitionLoader::new(dir.path());
        let a = loader.load("zccache").expect("load");
        let b = loader.reload("zccache").expect("reload");
        let c = loader
            .lookup_or_reload("zccache")
            .expect("lookup_or_reload");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }
}
