//! Service-definition file loading for the v1 broker.
//!
//! The loader intentionally re-reads from disk for each `lookup_or_reload`
//! call. That gives Phase 4's Hello path reload-on-Hello semantics without
//! coupling the validation rules to the later async accept loop.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use prost::Message;

use crate::broker::lifecycle::names::{validate_service_name, validate_version, PipePathError};
use crate::broker::protocol::{BrokerIsolation, ServiceDefinition};
use crate::broker::secure_dir;

/// Service-definition file extension.
pub const SERVICE_DEF_EXTENSION: &str = "servicedef";

/// Environment override for tests and development.
pub const SERVICE_DEF_DIR_ENV: &str = "RUNNING_PROCESS_SERVICE_DEF_DIR";

/// Loader rooted at one service-definition directory.
#[derive(Clone, Debug)]
pub struct ServiceDefinitionLoader {
    root: PathBuf,
}

impl ServiceDefinitionLoader {
    /// Create a loader for `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Create a loader for the platform default service-definition directory.
    pub fn default_root() -> Self {
        Self::new(service_definition_dir())
    }

    /// Directory this loader reads from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Load and validate one service definition from disk.
    pub fn load(&self, service_name: &str) -> Result<ServiceDefinition, ServiceDefinitionError> {
        ensure_loadable_service_definition_dir(&self.root)?;
        let path = service_definition_path(&self.root, service_name)?;
        let bytes = fs::read(&path)?;
        let definition = ServiceDefinition::decode(bytes.as_slice())?;
        validate_service_definition_for_service(&definition, service_name)?;
        Ok(definition)
    }

    /// Reload one service definition from disk.
    pub fn reload(&self, service_name: &str) -> Result<ServiceDefinition, ServiceDefinitionError> {
        self.load(service_name)
    }

    /// Lookup that always re-reads the service-definition file.
    pub fn lookup_or_reload(
        &self,
        service_name: &str,
    ) -> Result<ServiceDefinition, ServiceDefinitionError> {
        self.load(service_name)
    }
}

/// Return the platform service-definition directory.
pub fn service_definition_dir() -> PathBuf {
    if let Some(path) = std::env::var_os(SERVICE_DEF_DIR_ENV) {
        return PathBuf::from(path);
    }

    #[cfg(windows)]
    {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("running-process")
            .join("services")
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Library")
            .join("Application Support")
            .join("running-process")
            .join("services")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(config_home)
                .join("running-process")
                .join("services")
        } else {
            dirs::home_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join(".config")
                .join("running-process")
                .join("services")
        }
    }
}

/// Ensure a service-definition directory exists with private permissions.
pub fn ensure_service_definition_dir(path: &Path) -> Result<(), ServiceDefinitionError> {
    secure_dir::ensure_private_dir(path)?;
    ensure_loadable_service_definition_dir(path)
}

/// Validate and write one `.servicedef` file into `root`.
///
/// Consumer installers and development tools should use this helper instead of
/// duplicating protobuf serialization and service-definition path logic.
pub fn write_service_definition(
    root: &Path,
    definition: &ServiceDefinition,
) -> Result<PathBuf, ServiceDefinitionError> {
    ensure_service_definition_dir(root)?;
    validate_service_definition_for_service(definition, &definition.service_name)?;
    let path = service_definition_path(root, &definition.service_name)?;
    fs::write(&path, definition.encode_to_vec())?;
    Ok(path)
}

/// Compute the file path for one service definition.
pub fn service_definition_path(
    root: &Path,
    service_name: &str,
) -> Result<PathBuf, ServiceDefinitionError> {
    validate_service_name(service_name)?;
    Ok(root.join(format!("{service_name}.{SERVICE_DEF_EXTENSION}")))
}

/// Validate one decoded service definition against the requested service.
pub fn validate_service_definition_for_service(
    definition: &ServiceDefinition,
    expected_service: &str,
) -> Result<(), ServiceDefinitionError> {
    validate_service_name(expected_service)?;
    validate_service_name(&definition.service_name)?;
    if definition.service_name != expected_service {
        return Err(ServiceDefinitionError::ServiceNameMismatch {
            requested: expected_service.into(),
            actual: definition.service_name.clone(),
        });
    }
    validate_absolute_path("binary_path", &definition.binary_path)?;
    if !definition.per_version_binary_dir.is_empty() {
        validate_absolute_path("per_version_binary_dir", &definition.per_version_binary_dir)?;
    }
    if !definition.min_version.is_empty() {
        validate_version(&definition.min_version)?;
    }
    for version in &definition.version_allow_list {
        validate_version(version)?;
    }

    match BrokerIsolation::try_from(definition.isolation) {
        Ok(BrokerIsolation::PrivateBroker) | Ok(BrokerIsolation::SharedBroker) => {
            if !definition.explicit_instance.is_empty() {
                return Err(ServiceDefinitionError::InvalidIsolation {
                    reason: "explicit_instance must be empty unless isolation is EXPLICIT_INSTANCE",
                });
            }
        }
        Ok(BrokerIsolation::ExplicitInstance) => {
            if definition.explicit_instance.is_empty() {
                return Err(ServiceDefinitionError::InvalidIsolation {
                    reason: "EXPLICIT_INSTANCE requires explicit_instance",
                });
            }
            validate_service_name(&definition.explicit_instance)?;
        }
        Err(_) => {
            return Err(ServiceDefinitionError::InvalidIsolation {
                reason: "unknown BrokerIsolation value",
            });
        }
    }

    Ok(())
}

/// Errors returned while loading service-definition files.
#[derive(Debug, thiserror::Error)]
pub enum ServiceDefinitionError {
    /// Filesystem operation failed.
    #[error("service-definition I/O failed: {0}")]
    Io(#[from] io::Error),
    /// Protobuf decode failed.
    #[error("service-definition protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),
    /// Name or version validation failed.
    #[error(transparent)]
    InvalidName(#[from] PipePathError),
    /// Directory permissions are too broad.
    #[error("service-definition directory has insecure permissions: {0}")]
    InsecureDirectory(PathBuf),
    /// File content did not match the requested service.
    #[error("service-definition requested {requested:?} but file declares {actual:?}")]
    ServiceNameMismatch {
        /// Service name requested by the Hello path.
        requested: String,
        /// Service name decoded from disk.
        actual: String,
    },
    /// A path field was empty or relative.
    #[error("service-definition {field} is invalid: {path:?} ({reason})")]
    InvalidPath {
        /// Field name.
        field: &'static str,
        /// Field value.
        path: String,
        /// Why it failed validation.
        reason: &'static str,
    },
    /// Isolation fields were inconsistent.
    #[error("service-definition isolation is invalid: {reason}")]
    InvalidIsolation {
        /// Why it failed validation.
        reason: &'static str,
    },
}

fn ensure_loadable_service_definition_dir(path: &Path) -> Result<(), ServiceDefinitionError> {
    if !secure_dir::private_dir_permissions_are_private(path)? {
        return Err(ServiceDefinitionError::InsecureDirectory(
            path.to_path_buf(),
        ));
    }
    Ok(())
}

fn validate_absolute_path(field: &'static str, value: &str) -> Result<(), ServiceDefinitionError> {
    if value.is_empty() {
        return Err(ServiceDefinitionError::InvalidPath {
            field,
            path: value.into(),
            reason: "must not be empty",
        });
    }
    if !Path::new(value).is_absolute() {
        return Err(ServiceDefinitionError::InvalidPath {
            field,
            path: value.into(),
            reason: "must be absolute",
        });
    }
    Ok(())
}
