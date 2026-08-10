//! v2 service-definition I/O helpers (slice 22b of zccache#782).
//!
//! Convenience layer for writing and locating `.servicedef.v2` files —
//! the v2 broker reads these instead of v1's `.servicedef`. Coexists
//! with the v1 helpers in [`super::super::server::service_def_loader`]
//! during the v1→v2 rollout; both extensions can live in the same
//! per-OS service-definition directory.
//!
//! Mirrors the v1 surface (`ServiceDefinitionBuilder`,
//! `service_definition_dir`, `write_service_definition`) so v1→v2
//! migration on the consumer side is mechanical — every v1 call has
//! a v2 counterpart with the same argument shape.
//!
//! Reads (loader) intentionally live elsewhere: the v2 broker owns the
//! load path; this module is the *write* + *layout* side for
//! consumer-side installers (e.g. zccache's `service_definition.rs`).

use std::path::{Path, PathBuf};

use prost::Message as _;

use crate::broker::lifecycle::names::validate_service_name;
use crate::broker::secure_dir;
use crate::broker::server::service_def_loader::{
    ensure_service_definition_dir, service_definition_dir, ServiceDefinitionError,
};

use super::{BrokerIsolation, ServiceDefinition};

/// v2 service-definition file extension. Distinct from v1's `servicedef`
/// so a v1 broker never accidentally tries to decode a v2 file (and
/// vice versa).
pub const SERVICE_DEF_V2_EXTENSION: &str = "servicedef.v2";

/// Return the v2 service-definition directory.
///
/// Same per-OS path as v1's [`service_definition_dir`] — both
/// extensions cohabit the directory during rollout. The broker
/// chooses which to load based on its v1/v2 mode.
#[must_use]
pub fn service_definition_dir_v2() -> PathBuf {
    service_definition_dir()
}

/// Compute the v2 file path for one service definition.
///
/// # Errors
///
/// Returns [`ServiceDefinitionError::InvalidName`] when the service name
/// fails [`validate_service_name`] (same rules as v1).
pub fn service_definition_path_v2(
    root: &Path,
    service_name: &str,
) -> Result<PathBuf, ServiceDefinitionError> {
    validate_service_name(service_name)?;
    Ok(root.join(format!("{service_name}.{SERVICE_DEF_V2_EXTENSION}")))
}

/// Validate and write one `.servicedef.v2` file into `root`.
///
/// Consumer installers (e.g. zccache's daemon startup) should use this
/// helper instead of re-implementing the proto encode + path layout.
///
/// # Errors
///
/// - [`ServiceDefinitionError::Io`] for filesystem failures (mkdir, write).
/// - [`ServiceDefinitionError::InvalidName`] when `definition.service_name`
///   fails validation.
/// - [`ServiceDefinitionError::InsecureDirectory`] when `root` exists
///   but has world/group write bits set.
pub fn write_service_definition_v2(
    root: &Path,
    definition: &ServiceDefinition,
) -> Result<PathBuf, ServiceDefinitionError> {
    ensure_service_definition_dir(root)?;
    let path = service_definition_path_v2(root, &definition.service_name)?;
    std::fs::write(&path, definition.encode_to_vec())?;
    Ok(path)
}

/// Builder for [`ServiceDefinition`].
///
/// Mirrors the v1 [`super::super::builders::ServiceDefinitionBuilder`]
/// API verbatim so consumers can swap `use` paths and keep the same
/// call sites. v2 ships the same launcher + isolation field set as v1
/// (slice 22 of zccache#782) plus the v2-only HTTP capability slot.
#[derive(Debug, Clone)]
pub struct ServiceDefinitionBuilder {
    definition: ServiceDefinition,
}

impl ServiceDefinitionBuilder {
    /// Start a builder for a service that opts in to the per-user
    /// shared broker (the common case for first-party tools).
    ///
    /// `service_name` must satisfy [`validate_service_name`] (already
    /// enforced by [`write_service_definition_v2`] at install time;
    /// the builder itself is permissive so a caller can finish
    /// constructing then validate once).
    #[must_use]
    pub fn shared_broker(service_name: impl Into<String>, binary_path: impl Into<String>) -> Self {
        Self {
            definition: ServiceDefinition {
                service_name: service_name.into(),
                binary_path: binary_path.into(),
                isolation: BrokerIsolation::SharedBroker as i32,
                ..Default::default()
            },
        }
    }

    /// Start a builder for a service that uses a private per-service
    /// broker (the default — safest for third-party consumers).
    #[must_use]
    pub fn private_broker(service_name: impl Into<String>, binary_path: impl Into<String>) -> Self {
        Self {
            definition: ServiceDefinition {
                service_name: service_name.into(),
                binary_path: binary_path.into(),
                isolation: BrokerIsolation::PrivateBroker as i32,
                ..Default::default()
            },
        }
    }

    /// Start a builder pinned to a named broker instance (e.g.
    /// `"ci-trusted"` / `"ci-untrusted"`).
    #[must_use]
    pub fn explicit_instance(
        service_name: impl Into<String>,
        binary_path: impl Into<String>,
        instance: impl Into<String>,
    ) -> Self {
        Self {
            definition: ServiceDefinition {
                service_name: service_name.into(),
                binary_path: binary_path.into(),
                isolation: BrokerIsolation::ExplicitInstance as i32,
                explicit_instance: instance.into(),
                ..Default::default()
            },
        }
    }

    /// Pin the canonicalized binary-directory allow-list root.
    #[must_use]
    pub fn per_version_binary_dir(mut self, dir: impl Into<String>) -> Self {
        self.definition.per_version_binary_dir = dir.into();
        self
    }

    /// Set the semver floor; broker refuses Hello below this.
    #[must_use]
    pub fn min_version(mut self, version: impl Into<String>) -> Self {
        self.definition.min_version = version.into();
        self
    }

    /// Pin to a strict allow-list of versions.
    #[must_use]
    pub fn version_allow_list<I, S>(mut self, versions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.definition.version_allow_list = versions.into_iter().map(Into::into).collect();
        self
    }

    /// Add a key/value label.
    #[must_use]
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.definition.labels.insert(key.into(), value.into());
        self
    }

    /// Finalize into a [`ServiceDefinition`]. Does not validate; call
    /// [`write_service_definition_v2`] to validate + install.
    #[must_use]
    pub fn build(self) -> ServiceDefinition {
        self.definition
    }

    /// Install into a specific service-definition directory. Equivalent
    /// to `write_service_definition_v2(root, &self.build())`.
    ///
    /// # Errors
    ///
    /// See [`write_service_definition_v2`].
    pub fn install_in(self, root: &Path) -> Result<PathBuf, ServiceDefinitionError> {
        write_service_definition_v2(root, &self.build())
    }

    /// Install into the platform's default v2 service-definition
    /// directory ([`service_definition_dir_v2`]).
    ///
    /// # Errors
    ///
    /// See [`write_service_definition_v2`].
    pub fn install(self) -> Result<PathBuf, ServiceDefinitionError> {
        let root = service_definition_dir_v2();
        // Mirror the v1 install path: ensure the dir exists *and* is
        // privately-permissioned before writing. `ensure_service_definition_dir`
        // is called transitively by `write_service_definition_v2`, but
        // calling `ensure_private_dir` here too matches v1's belt-and-
        // suspenders pattern for the default-root path.
        secure_dir::ensure_private_dir(&root)?;
        self.install_in(&root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn extension_is_servicedef_v2() {
        assert_eq!(SERVICE_DEF_V2_EXTENSION, "servicedef.v2");
    }

    #[test]
    fn service_definition_path_v2_uses_v2_extension() {
        let root = Path::new("/svc");
        let path = service_definition_path_v2(root, "zccache").unwrap();
        assert_eq!(
            path.to_str().unwrap().replace('\\', "/"),
            "/svc/zccache.servicedef.v2"
        );
    }

    #[test]
    fn service_definition_path_v2_rejects_invalid_name() {
        let root = Path::new("/svc");
        assert!(service_definition_path_v2(root, "ZCCACHE").is_err());
        assert!(service_definition_path_v2(root, "").is_err());
        assert!(service_definition_path_v2(root, "a/b").is_err());
    }

    #[test]
    fn shared_broker_builder_sets_expected_fields() {
        let def = ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache").build();
        assert_eq!(def.service_name, "zccache");
        assert_eq!(def.binary_path, "/usr/bin/zccache");
        assert_eq!(def.isolation, BrokerIsolation::SharedBroker as i32);
        assert!(def.explicit_instance.is_empty());
    }

    #[test]
    fn private_broker_builder_sets_expected_fields() {
        let def = ServiceDefinitionBuilder::private_broker("svc", "/bin/x").build();
        assert_eq!(def.isolation, BrokerIsolation::PrivateBroker as i32);
    }

    #[test]
    fn explicit_instance_builder_sets_expected_fields() {
        let def =
            ServiceDefinitionBuilder::explicit_instance("svc", "/bin/x", "ci-trusted").build();
        assert_eq!(def.isolation, BrokerIsolation::ExplicitInstance as i32);
        assert_eq!(def.explicit_instance, "ci-trusted");
    }

    #[test]
    fn builder_chain_propagates_optional_fields() {
        let def = ServiceDefinitionBuilder::shared_broker("svc", "/bin/x")
            .per_version_binary_dir("/usr/local/bin")
            .min_version("1.2.3")
            .version_allow_list(["1.2.3", "1.3.0"])
            .label("env", "prod")
            .label("region", "us-west")
            .build();
        assert_eq!(def.per_version_binary_dir, "/usr/local/bin");
        assert_eq!(def.min_version, "1.2.3");
        assert_eq!(def.version_allow_list, vec!["1.2.3", "1.3.0"]);
        assert_eq!(def.labels.get("env"), Some(&"prod".to_owned()));
        assert_eq!(def.labels.get("region"), Some(&"us-west".to_owned()));
    }

    #[test]
    fn install_in_writes_and_decodes_round_trip() {
        let dir = tempdir().expect("tempdir");
        let path = ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache")
            .min_version("1.0.0")
            .label("env", "prod")
            .install_in(dir.path())
            .expect("install_in");

        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some("zccache.servicedef.v2")
        );

        let bytes = std::fs::read(&path).expect("read file");
        let decoded = ServiceDefinition::decode(bytes.as_slice()).expect("decode");
        assert_eq!(decoded.service_name, "zccache");
        assert_eq!(decoded.binary_path, "/usr/bin/zccache");
        assert_eq!(decoded.isolation, BrokerIsolation::SharedBroker as i32);
        assert_eq!(decoded.min_version, "1.0.0");
        assert_eq!(decoded.labels.get("env"), Some(&"prod".to_owned()));
    }

    #[test]
    fn write_service_definition_v2_rejects_invalid_name() {
        let dir = tempdir().expect("tempdir");
        let bad = ServiceDefinition {
            service_name: "BAD-Caps".to_owned(),
            ..Default::default()
        };
        let err = write_service_definition_v2(dir.path(), &bad).expect_err("must reject");
        let _ = err;
    }

    #[test]
    fn write_service_definition_v2_creates_parent_dir() {
        let dir = tempdir().expect("tempdir");
        let nested = dir.path().join("nested");
        let path = ServiceDefinitionBuilder::shared_broker("svc", "/bin/x")
            .install_in(&nested)
            .expect("install_in into nested");
        assert!(path.exists());
        assert!(nested.exists());
    }

    /// Round-trip: write via the builder, read the bytes back as a
    /// raw [`ServiceDefinition`], assert every builder field survived.
    /// Pins the contract from a different angle than the standalone
    /// proto round-trip tests in `mod.rs`.
    #[test]
    fn builder_install_round_trip_preserves_every_field() {
        let dir = tempdir().expect("tempdir");
        let path = ServiceDefinitionBuilder::explicit_instance("svc", "/bin/x", "ci-trusted")
            .per_version_binary_dir("/usr/local/bin")
            .min_version("1.0.0")
            .version_allow_list(["1.0.0", "1.1.0"])
            .label("env", "prod")
            .label("rollout", "blue")
            .install_in(dir.path())
            .expect("install_in");

        let bytes = std::fs::read(&path).expect("read");
        let decoded = ServiceDefinition::decode(bytes.as_slice()).expect("decode");
        assert_eq!(decoded.service_name, "svc");
        assert_eq!(decoded.binary_path, "/bin/x");
        assert_eq!(decoded.isolation, BrokerIsolation::ExplicitInstance as i32);
        assert_eq!(decoded.explicit_instance, "ci-trusted");
        assert_eq!(decoded.per_version_binary_dir, "/usr/local/bin");
        assert_eq!(decoded.min_version, "1.0.0");
        assert_eq!(decoded.version_allow_list, vec!["1.0.0", "1.1.0"]);
        assert_eq!(decoded.labels.len(), 2);
        assert_eq!(decoded.labels.get("env"), Some(&"prod".to_owned()));
        assert_eq!(decoded.labels.get("rollout"), Some(&"blue".to_owned()));
    }
}
