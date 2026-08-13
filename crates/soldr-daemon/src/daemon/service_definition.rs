//! `running-process` **v2** service-definition support for soldr-daemon.
//!
//! soldr adopts the running-process v2 broker (soldr#1495). This module
//! writes the soldr-daemon `.servicedef.v2` file the v2 broker reads to
//! discover the daemon, its `per_version_binary_dir` allow-list root,
//! and its version policy (`min_version` + `version_allow_list`). The v2
//! adoption is mandatory: there is no direct daemon-acquisition lane, opt-in
//! switch, or client-owned placement path.

use crate::daemon::backend_handle_adoption::{
    broker_route_identity, SOLDR_DAEMON_IMAGE_HASH_LABEL, SOLDR_DAEMON_SERVICE_VERSION,
};
use running_process::broker::protocol_v2::{
    service_definition_dir_v2, service_definition_path_v2, write_service_definition_v2,
    BrokerIsolation, ServiceDefinition, ServiceDefinitionBuilder,
};
use std::io;
use std::path::{Path, PathBuf};

pub const SOLDR_ROOT_SERVICE_LABEL: &str = "soldr-root";
pub const SOLDR_DAEMON_ENV_LABEL_PREFIX: &str = "soldr-env:";

/// Stable, user-scoped storage owned by the singleton broker rather than by
/// any daemon route.  Daemon images are content/version addressed below this
/// root and can therefore be shared by distinct `SOLDR_CACHE_DIR` partitions;
/// daemon state and sockets remain rooted in each route's [`SoldrPaths`].
pub fn broker_owned_paths() -> crate::core::SoldrPaths {
    let services = service_definition_dir_v2();
    let owner = services.parent().unwrap_or(&services);
    crate::core::SoldrPaths::with_root(owner.join("soldr-broker"))
}

/// Work intentionally left out of the v2 adoption slice, tracked under
/// soldr#1495 Workstream B: the broker-owned `UpgradeDaemon` graceful
/// handoff cannot land until running-process ships the singleton-mode
/// Phase 4b machinery (the v2 adopt path is a stub today).
pub const SOLDR_DAEMON_SERVICE_DEF_DEFERRED: &[&str] = &[
    "broker-owned UpgradeDaemon singleton handoff (upstream Phase 4b)",
    "broker-mediated backend routing once adopt returns a real endpoint",
];

#[derive(Debug, Clone)]
pub struct InstalledServiceDefinition {
    pub path: PathBuf,
    pub definition: ServiceDefinition,
}

pub(crate) fn sibling_daemon_binary(current: &Path) -> PathBuf {
    let stem =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
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

/// Build the soldr-daemon v2 `SHARED_BROKER` service definition. The
/// `per_version_binary_dir` allow-list root is the directory holding the
/// (canonicalized) daemon binary — matching soldr's version-rooted
/// runtime relocation tree — and the version policy pins exactly this
/// build's `CARGO_PKG_VERSION` so the broker refuses any other version's
/// Hello.
pub(crate) fn soldr_daemon_service_definition(
    daemon_binary: &Path,
) -> io::Result<ServiceDefinition> {
    let paths = crate::core::SoldrPaths::new().map_err(|err| io::Error::other(err.to_string()))?;
    soldr_daemon_service_definition_for_paths(&paths, daemon_binary)
}

pub(crate) fn soldr_daemon_service_definition_for_paths(
    paths: &crate::core::SoldrPaths,
    daemon_binary: &Path,
) -> io::Result<ServiceDefinition> {
    let binary = std::fs::canonicalize(daemon_binary)?;
    let binary_dir = binary.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "soldr-daemon binary path has no parent directory",
        )
    })?;

    let route = broker_route_identity(paths, &binary)?;
    let root = paths.root.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Soldr root is not valid Unicode and cannot be routed by the broker",
        )
    })?;
    let mut definition =
        ServiceDefinitionBuilder::shared_broker(route.service_name, binary.display().to_string())
            .per_version_binary_dir(binary_dir.display().to_string())
            .min_version(SOLDR_DAEMON_SERVICE_VERSION)
            .version_allow_list([SOLDR_DAEMON_SERVICE_VERSION])
            .label(SOLDR_ROOT_SERVICE_LABEL, root)
            .label(SOLDR_DAEMON_IMAGE_HASH_LABEL, route.image_hash)
            .label("vendor", "zackees")
            .label("package", "soldr")
            .label("running-process-tracker", "zackees/soldr#1495")
            .build();
    add_daemon_env_labels(
        &mut definition,
        crate::daemon::lifecycle::forwarded_soldr_env(),
    )?;

    debug_assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
    Ok(definition)
}

fn add_daemon_env_labels(
    definition: &mut ServiceDefinition,
    env: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> io::Result<()> {
    for (name, value) in env {
        let name = name.into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "daemon environment variable name is not valid Unicode",
            )
        })?;
        let value = value.into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("daemon environment value for {name} is not valid Unicode"),
            )
        })?;
        definition
            .labels
            .insert(format!("{SOLDR_DAEMON_ENV_LABEL_PREFIX}{name}"), value);
    }
    Ok(())
}

pub fn daemon_env_from_service_definition(
    definition: &ServiceDefinition,
) -> io::Result<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    daemon_env_from_labels(&definition.labels)
}

pub fn daemon_env_from_labels(
    labels: &std::collections::HashMap<String, String>,
) -> io::Result<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    let mut env = Vec::new();
    for (label, value) in labels {
        let Some(name) = label.strip_prefix(SOLDR_DAEMON_ENV_LABEL_PREFIX) else {
            continue;
        };
        if name.is_empty()
            || !crate::daemon::lifecycle::forwarded_env_name(std::ffi::OsStr::new(name))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid daemon environment label {label:?}"),
            ));
        }
        env.push((name.into(), value.into()));
    }
    Ok(env)
}

fn servicedef_io_error(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(err.to_string())
}

pub fn install_default_service_definition() -> io::Result<InstalledServiceDefinition> {
    install_service_definition(&default_daemon_binary()?)
}

pub fn install_service_definition(daemon_binary: &Path) -> io::Result<InstalledServiceDefinition> {
    install_service_definition_to_dir(service_definition_dir_v2(), daemon_binary)
}

pub fn install_service_definition_to_dir(
    service_root: impl AsRef<Path>,
    daemon_binary: &Path,
) -> io::Result<InstalledServiceDefinition> {
    let paths = crate::core::SoldrPaths::new().map_err(|err| io::Error::other(err.to_string()))?;
    install_service_definition_to_dir_for_paths(service_root, &paths, daemon_binary)
}

pub fn install_service_definition_to_dir_for_paths(
    service_root: impl AsRef<Path>,
    paths: &crate::core::SoldrPaths,
    daemon_binary: &Path,
) -> io::Result<InstalledServiceDefinition> {
    let service_root = service_root.as_ref();
    let definition = soldr_daemon_service_definition_for_paths(paths, daemon_binary)?;
    // `write_service_definition_v2` creates the (privately-permissioned)
    // dir, validates the service name, and writes the `.servicedef.v2`
    // protobuf.
    let path =
        write_service_definition_v2(service_root, &definition).map_err(servicedef_io_error)?;
    debug_assert_eq!(
        path,
        service_definition_path_v2(service_root, &definition.service_name)
            .expect("valid service name"),
    );
    Ok(InstalledServiceDefinition { path, definition })
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::protocol_v2::ServiceDefinitionLoader;
    use tempfile::TempDir;

    fn fake_daemon_binary(root: &Path) -> PathBuf {
        let binary = root.join(
            if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
                "soldr-daemon.exe"
            } else {
                "soldr-daemon"
            },
        );
        std::fs::write(&binary, b"stub").expect("fake daemon binary");
        binary
    }

    crate::timed_test!(service_definition_declares_soldr_daemon_shared_broker, {
        let temp = TempDir::new().expect("tempdir");
        let daemon = fake_daemon_binary(temp.path());

        let definition = soldr_daemon_service_definition(&daemon).expect("definition");

        assert!(definition.service_name.starts_with("soldr-daemon-"));
        assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
        assert_eq!(definition.min_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(definition.version_allow_list, [env!("CARGO_PKG_VERSION")]);
        assert_eq!(
            definition.labels.get("package").map(String::as_str),
            Some("soldr"),
        );
        assert!(definition.labels.contains_key(SOLDR_ROOT_SERVICE_LABEL));
    });

    crate::timed_test!(daemon_environment_is_owned_by_each_route_registration, {
        let mut definition = ServiceDefinitionBuilder::shared_broker("svc", "/bin/svc").build();
        add_daemon_env_labels(
            &mut definition,
            [
                ("SOLDR_JOBS".into(), "2".into()),
                ("ZCCACHE_STAGING_DIR".into(), "/tmp/staging".into()),
            ],
        )
        .expect("encode daemon env");

        let decoded = daemon_env_from_service_definition(&definition).expect("decode daemon env");
        assert!(decoded.contains(&("SOLDR_JOBS".into(), "2".into())));
        assert!(decoded.contains(&("ZCCACHE_STAGING_DIR".into(), "/tmp/staging".into())));
    });

    crate::timed_test!(daemon_environment_labels_reject_unapproved_names, {
        let mut definition = ServiceDefinitionBuilder::shared_broker("svc", "/bin/svc").build();
        definition.labels.insert(
            format!("{SOLDR_DAEMON_ENV_LABEL_PREFIX}PATH"),
            "/untrusted".into(),
        );
        let err = daemon_env_from_service_definition(&definition)
            .expect_err("PATH must not cross the broker registration boundary");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    });

    crate::timed_test!(
        install_service_definition_writes_v2_loader_compatible_protobuf,
        {
            let temp = TempDir::new().expect("tempdir");
            let service_root = temp.path().join("services");
            let daemon = fake_daemon_binary(temp.path());

            let installed = install_service_definition_to_dir(&service_root, &daemon)
                .expect("install service definition");

            // v2 files carry the `.servicedef.v2` extension.
            assert_eq!(
                installed.path,
                service_root.join(format!(
                    "{}.servicedef.v2",
                    installed.definition.service_name
                ))
            );
            let loaded = ServiceDefinitionLoader::new(&service_root)
                .load(&installed.definition.service_name)
                .expect("load service definition");
            assert_eq!(loaded, installed.definition);
        }
    );

    crate::timed_test!(deferred_items_document_upstream_gated_boundary, {
        // The v2 discovery + servicedef wiring is DONE (this slice); what
        // remains deferred is the upstream-gated broker-owned handoff.
        assert!(SOLDR_DAEMON_SERVICE_DEF_DEFERRED
            .iter()
            .any(|item| item.contains("UpgradeDaemon")));
        assert!(!SOLDR_DAEMON_SERVICE_DEF_DEFERRED
            .iter()
            .any(|item| item.contains("connect_to_backend wiring")));
    });
}
