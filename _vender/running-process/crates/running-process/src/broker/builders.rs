//! Ergonomic builders for the two registration messages a consumer must
//! produce to join the broker: [`ServiceDefinition`] and [`CacheManifest`]
//! (#433 R2).
//!
//! The wire types are prost-generated structs with ~10-16 fields each, most of
//! which a consumer leaves at their defaults. Hand-constructing them means
//! spelling out every field (and re-deriving the boilerplate the broker already
//! owns: media type, schema version, host identity, timestamps, self-digest).
//! These builders set the required fields, default the rest, validate on
//! `build`, and optionally persist via the existing central-registry helpers.
//!
//! ```no_run
//! use running_process::broker::builders::{CacheManifestBuilder, ServiceDefinitionBuilder};
//! use running_process::broker::protocol::CacheRootKind;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Register the service the broker spawns/negotiates.
//! ServiceDefinitionBuilder::shared_broker("zccache", "/usr/local/bin/zccache")
//!     .min_version("1.10.0")
//!     .allow_version("1.11.20")
//!     .install()?;
//!
//! // Publish the daemon's cache manifest into the central registry.
//! CacheManifestBuilder::new("zccache", "1.11.20")
//!     .broker_instance("shared")
//!     .root(CacheRootKind::CacheData, "/var/cache/zccache")
//!     .publish()?;
//! # Ok(()) }
//! ```

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::broker::host_identity;
use crate::broker::manifest::{
    manifest_with_self_sha256, write_to_central, write_to_central_in_dir, ManifestError,
    CACHE_MANIFEST_MEDIA_TYPE, SUPPORTED_MANIFEST_SCHEMA_VERSION,
};
use crate::broker::protocol::{
    BrokerIsolation, CacheManifest, CacheRoot, CacheRootKind, ServiceDefinition,
};
use crate::broker::server::service_def_loader::{
    service_definition_dir, validate_service_definition_for_service, write_service_definition,
    ServiceDefinitionError,
};

/// Broker envelope version stamped onto every manifest this builder produces.
const BROKER_ENVELOPE_VERSION: &str = "v1";

/// Fluent builder for a [`ServiceDefinition`].
///
/// Construct via [`shared_broker`](Self::shared_broker) (per-user local) or
/// [`explicit_instance`](Self::explicit_instance) (trust-grouped CI), chain the
/// optional setters, then [`build`](Self::build) to validate or
/// [`install`](Self::install) to validate and write the `.servicedef`.
#[derive(Clone, Debug)]
pub struct ServiceDefinitionBuilder {
    definition: ServiceDefinition,
}

impl ServiceDefinitionBuilder {
    /// Begin a `SHARED_BROKER` (per-user local) service definition.
    ///
    /// `binary_path` must be an absolute path — the broker validates it on
    /// [`build`](Self::build).
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

    /// Begin an `EXPLICIT_INSTANCE` (trust-grouped) service definition.
    ///
    /// `instance` is the trust-group label; it must be a valid service-name
    /// token.
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

    /// Set the minimum acceptable backend version.
    pub fn min_version(mut self, version: impl Into<String>) -> Self {
        self.definition.min_version = version.into();
        self
    }

    /// Append one version to the allow-list.
    pub fn allow_version(mut self, version: impl Into<String>) -> Self {
        self.definition.version_allow_list.push(version.into());
        self
    }

    /// Set the absolute directory holding per-version backend binaries.
    pub fn per_version_binary_dir(mut self, dir: impl Into<String>) -> Self {
        self.definition.per_version_binary_dir = dir.into();
        self
    }

    /// Attach one diagnostic label.
    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.definition.labels.insert(key.into(), value.into());
        self
    }

    /// Validate and return the [`ServiceDefinition`] without persisting it.
    pub fn build(self) -> Result<ServiceDefinition, ServiceDefinitionError> {
        validate_service_definition_for_service(&self.definition, &self.definition.service_name)?;
        Ok(self.definition)
    }

    /// Validate and write the `.servicedef` into the default
    /// service-definition directory.
    pub fn install(self) -> Result<PathBuf, ServiceDefinitionError> {
        self.install_in(&service_definition_dir())
    }

    /// Validate and write the `.servicedef` into an explicit root (tests,
    /// custom layouts).
    pub fn install_in(self, root: &Path) -> Result<PathBuf, ServiceDefinitionError> {
        let definition = self.build()?;
        write_service_definition(root, &definition)
    }
}

/// Fluent builder for a [`CacheManifest`].
///
/// [`new`](Self::new) stamps the boilerplate the broker owns — media type,
/// schema version, host identity, created/last-active timestamps — leaving the
/// consumer to declare only what is theirs: the cache roots and broker
/// instance. [`build`](Self::build) seals the `self_sha256` digest;
/// [`publish`](Self::publish) writes it into the central registry.
#[derive(Clone, Debug)]
pub struct CacheManifestBuilder {
    manifest: CacheManifest,
}

impl CacheManifestBuilder {
    /// Begin a manifest for `service_name` at `service_version`.
    pub fn new(service_name: impl Into<String>, service_version: impl Into<String>) -> Self {
        let now = now_unix_ms();
        Self {
            manifest: CacheManifest {
                manifest_schema_version: SUPPORTED_MANIFEST_SCHEMA_VERSION,
                media_type: CACHE_MANIFEST_MEDIA_TYPE.to_string(),
                host: Some(host_identity::current()),
                service_name: service_name.into(),
                service_version: service_version.into(),
                broker_envelope_version: BROKER_ENVELOPE_VERSION.to_string(),
                created_at_unix_ms: now,
                last_active_unix_ms: now,
                ..Default::default()
            },
        }
    }

    /// Set the broker instance label (e.g. `"shared"` or an explicit-instance
    /// trust group).
    pub fn broker_instance(mut self, instance: impl Into<String>) -> Self {
        self.manifest.broker_instance = instance.into();
        self
    }

    /// Set the manifest bundle id.
    pub fn bundle_id(mut self, bundle_id: impl Into<String>) -> Self {
        self.manifest.bundle_id = bundle_id.into();
        self
    }

    /// Append one cache root of the given kind at `path`.
    pub fn root(mut self, kind: CacheRootKind, path: impl Into<String>) -> Self {
        self.manifest.roots.push(CacheRoot {
            path: path.into(),
            kind: kind as i32,
            ..Default::default()
        });
        self
    }

    /// Seal the manifest by computing its `self_sha256` digest and return it
    /// without persisting.
    pub fn build(self) -> Result<CacheManifest, ManifestError> {
        manifest_with_self_sha256(&self.manifest)
    }

    /// Seal and write the manifest atomically into the central registry,
    /// returning the written path.
    pub fn publish(self) -> Result<PathBuf, ManifestError> {
        let manifest = self.build()?;
        write_to_central(&manifest.service_name, &manifest.service_version, &manifest)
    }

    /// Seal and write into an explicit registry dir (tests, custom layouts).
    pub fn publish_in(self, registry_dir: &Path) -> Result<PathBuf, ManifestError> {
        let manifest = self.build()?;
        write_to_central_in_dir(
            registry_dir,
            &manifest.service_name,
            &manifest.service_version,
            &manifest,
        )
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
