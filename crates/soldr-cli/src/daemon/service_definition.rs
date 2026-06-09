//! Minimal `running-process` service-definition support for soldr-daemon.
//!
//! This is the package/discovery slice for zackees/soldr#718. It makes
//! soldr-daemon discoverable by the shared broker without switching soldr's
//! own hot path to broker-mediated connects yet.

use crate::daemon::backend_handle_adoption::{
    SOLDR_DAEMON_SERVICE_NAME, SOLDR_DAEMON_SERVICE_VERSION,
};
use prost14::Message as _;
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    ensure_service_definition_dir, service_definition_dir, service_definition_path,
    validate_service_definition_for_service,
};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) const SOLDR_DAEMON_SERVICE_DEF_DEFERRED: &[&str] = &[
    "package/postinstall servicedef auto-install coverage",
    "broker-client connect_to_backend wiring",
    "default-on rollout and escape-hatch removal",
];

#[derive(Debug, Clone)]
pub(crate) struct InstalledServiceDefinition {
    pub(crate) path: PathBuf,
    pub(crate) definition: ServiceDefinition,
}

pub(crate) fn sibling_daemon_binary(current: &Path) -> PathBuf {
    let stem = if cfg!(windows) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    current
        .parent()
        .map(|p| p.join(stem))
        .unwrap_or_else(|| PathBuf::from(stem))
}

pub(crate) fn default_daemon_binary() -> io::Result<PathBuf> {
    let current = std::env::current_exe()?;
    Ok(sibling_daemon_binary(&current))
}

pub(crate) fn soldr_daemon_service_definition(
    daemon_binary: &Path,
) -> io::Result<ServiceDefinition> {
    let binary = std::fs::canonicalize(daemon_binary)?;
    let binary_dir = binary.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "soldr-daemon binary path has no parent directory",
        )
    })?;

    let mut labels = HashMap::new();
    labels.insert("vendor".to_string(), "zackees".to_string());
    labels.insert("package".to_string(), "soldr".to_string());
    labels.insert(
        "running-process-tracker".to_string(),
        "zackees/soldr#718".to_string(),
    );

    let definition = ServiceDefinition {
        service_name: SOLDR_DAEMON_SERVICE_NAME.to_string(),
        binary_path: binary.display().to_string(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: binary_dir.display().to_string(),
        min_version: SOLDR_DAEMON_SERVICE_VERSION.to_string(),
        version_allow_list: vec![SOLDR_DAEMON_SERVICE_VERSION.to_string()],
        labels,
    };
    validate_service_definition_for_service(&definition, SOLDR_DAEMON_SERVICE_NAME)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    Ok(definition)
}

pub(crate) fn install_default_service_definition() -> io::Result<InstalledServiceDefinition> {
    install_service_definition(&default_daemon_binary()?)
}

pub(crate) fn install_service_definition(
    daemon_binary: &Path,
) -> io::Result<InstalledServiceDefinition> {
    install_service_definition_to_dir(service_definition_dir(), daemon_binary)
}

pub(crate) fn install_service_definition_to_dir(
    service_root: impl AsRef<Path>,
    daemon_binary: &Path,
) -> io::Result<InstalledServiceDefinition> {
    let service_root = service_root.as_ref();
    ensure_service_definition_dir(service_root).map_err(|err| io::Error::other(err.to_string()))?;
    let definition = soldr_daemon_service_definition(daemon_binary)?;
    let path = service_definition_path(service_root, SOLDR_DAEMON_SERVICE_NAME)
        .map_err(|err| io::Error::other(err.to_string()))?;

    std::fs::write(&path, definition.encode_to_vec())?;
    Ok(InstalledServiceDefinition { path, definition })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn service_definition_declares_soldr_daemon_shared_broker() {
        let temp = TempDir::new().expect("tempdir");
        let daemon = fake_daemon_binary(temp.path());

        let definition = soldr_daemon_service_definition(&daemon).expect("definition");

        assert_eq!(definition.service_name, "soldr-daemon");
        assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
        assert_eq!(definition.min_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(definition.version_allow_list, [env!("CARGO_PKG_VERSION")]);
        assert_eq!(
            definition
                .labels
                .get("running-process-tracker")
                .map(String::as_str),
            Some("zackees/soldr#718"),
        );
    }

    #[test]
    fn install_service_definition_writes_loader_compatible_protobuf() {
        let temp = TempDir::new().expect("tempdir");
        let service_root = temp.path().join("services");
        let daemon = fake_daemon_binary(temp.path());

        let installed = install_service_definition_to_dir(&service_root, &daemon)
            .expect("install service definition");

        assert_eq!(installed.path, service_root.join("soldr-daemon.servicedef"));
        let loaded = ServiceDefinitionLoader::new(&service_root)
            .load("soldr-daemon")
            .expect("load service definition");
        assert_eq!(loaded, installed.definition);
    }

    #[test]
    fn deferred_items_document_minimal_slice_boundary() {
        assert!(SOLDR_DAEMON_SERVICE_DEF_DEFERRED
            .iter()
            .any(|item| item.contains("connect_to_backend")));
        assert!(!SOLDR_DAEMON_SERVICE_DEF_DEFERRED
            .iter()
            .any(|item| item.contains("RUNNING_PROCESS_DISABLE")));
        assert!(SOLDR_DAEMON_SERVICE_DEF_DEFERRED
            .iter()
            .any(|item| item.contains("escape-hatch")));
    }
}
