#![cfg(feature = "client")]

use std::fs;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use prost::Message;
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    ensure_service_definition_dir, service_definition_path, write_service_definition,
    ServiceDefinitionError, ServiceDefinitionLoader,
};
use running_process::broker::server::{service_definition_dir, SERVICE_DEF_DIR_ENV};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn absolute_paths() -> (String, String) {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    (
        exe.to_string_lossy().into_owned(),
        dir.to_string_lossy().into_owned(),
    )
}

fn service_definition() -> ServiceDefinition {
    let (binary_path, per_version_binary_dir) = absolute_paths();
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path,
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir,
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn write_definition_for(root: &Path, service_name: &str, definition: &ServiceDefinition) {
    let path = service_definition_path(root, service_name).unwrap();
    fs::write(path, definition.encode_to_vec()).unwrap();
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn remove(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, original }
    }

    fn set_path(key: &'static str, value: &Path) -> Self {
        let original = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn loader_reads_valid_service_definition() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();
    write_definition_for(&root, "zccache", &service_definition());

    let loaded = ServiceDefinitionLoader::new(&root).load("zccache").unwrap();

    assert_eq!(loaded.service_name, "zccache");
    assert_eq!(loaded.min_version, "1.10.0");
    assert_eq!(loaded.version_allow_list, vec!["1.11.20"]);
}

#[test]
fn write_service_definition_creates_private_directory_and_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");

    let path = write_service_definition(&root, &service_definition()).unwrap();
    let loaded = ServiceDefinitionLoader::new(&root).load("zccache").unwrap();

    assert_eq!(path, service_definition_path(&root, "zccache").unwrap());
    assert_eq!(loaded.service_name, "zccache");
    assert_eq!(loaded.min_version, "1.10.0");
}

#[cfg(windows)]
#[test]
fn windows_default_service_definition_dir_uses_roaming_appdata_services() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _override = EnvVarGuard::remove(SERVICE_DEF_DIR_ENV);
    let appdata = PathBuf::from(std::env::var_os("APPDATA").expect("APPDATA must be set"));

    assert_eq!(
        service_definition_dir(),
        appdata.join("running-process").join("services")
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_default_service_definition_dir_uses_application_support() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _override = EnvVarGuard::remove(SERVICE_DEF_DIR_ENV);
    let home = dirs::home_dir().expect("home dir must resolve on macOS runners");

    assert_eq!(
        service_definition_dir(),
        home.join("Library")
            .join("Application Support")
            .join("running-process")
            .join("services")
    );
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn linux_default_service_definition_dir_uses_xdg_config_home() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _override = EnvVarGuard::remove(SERVICE_DEF_DIR_ENV);
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = EnvVarGuard::set_path("XDG_CONFIG_HOME", tmp.path());

    assert_eq!(
        service_definition_dir(),
        tmp.path().join("running-process").join("services")
    );
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn linux_default_service_definition_dir_falls_back_to_home_config() {
    let _lock = ENV_LOCK.lock().unwrap();
    let _override = EnvVarGuard::remove(SERVICE_DEF_DIR_ENV);
    let _xdg = EnvVarGuard::remove("XDG_CONFIG_HOME");
    let home = dirs::home_dir().expect("home dir must resolve on Linux runners");

    assert_eq!(
        service_definition_dir(),
        home.join(".config")
            .join("running-process")
            .join("services")
    );
}

#[test]
fn service_definition_dir_override_is_private_and_loadable() {
    let _lock = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("overridden-services");
    let _override = EnvVarGuard::set_path(SERVICE_DEF_DIR_ENV, &root);

    let default_root = service_definition_dir();
    let path = write_service_definition(&default_root, &service_definition()).unwrap();

    let loaded = ServiceDefinitionLoader::default_root()
        .lookup_or_reload("zccache")
        .unwrap();

    assert_eq!(default_root, root);
    assert_eq!(path, root.join("zccache.servicedef"));
    assert_eq!(loaded.service_name, "zccache");
    assert_eq!(loaded.min_version, "1.10.0");
}

#[test]
fn lookup_or_reload_rereads_changed_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();

    let mut definition = service_definition();
    write_definition_for(&root, "zccache", &definition);
    assert_eq!(
        ServiceDefinitionLoader::new(&root)
            .lookup_or_reload("zccache")
            .unwrap()
            .min_version,
        "1.10.0"
    );

    definition.min_version = "1.11.0".into();
    write_definition_for(&root, "zccache", &definition);
    assert_eq!(
        ServiceDefinitionLoader::new(&root)
            .lookup_or_reload("zccache")
            .unwrap()
            .min_version,
        "1.11.0"
    );
}

#[test]
fn loader_rejects_service_name_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();

    let mut definition = service_definition();
    definition.service_name = "clud".into();
    write_definition_for(&root, "zccache", &definition);

    let err = ServiceDefinitionLoader::new(&root)
        .load("zccache")
        .unwrap_err();
    match err {
        ServiceDefinitionError::ServiceNameMismatch { requested, actual } => {
            assert_eq!(requested, "zccache");
            assert_eq!(actual, "clud");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn loader_rejects_invalid_version_allow_list() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();

    let mut definition = service_definition();
    definition.version_allow_list = vec!["v1.11.20".into()];
    write_definition_for(&root, "zccache", &definition);

    let err = ServiceDefinitionLoader::new(&root)
        .load("zccache")
        .unwrap_err();
    assert!(matches!(err, ServiceDefinitionError::InvalidName(_)));
}

#[test]
fn loader_rejects_invalid_explicit_instance_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    ensure_service_definition_dir(&root).unwrap();

    let mut definition = service_definition();
    definition.isolation = BrokerIsolation::ExplicitInstance as i32;
    definition.explicit_instance.clear();
    write_definition_for(&root, "zccache", &definition);

    let err = ServiceDefinitionLoader::new(&root)
        .load("zccache")
        .unwrap_err();
    assert!(matches!(
        err,
        ServiceDefinitionError::InvalidIsolation { .. }
    ));
}

#[cfg(unix)]
#[test]
fn loader_rejects_group_or_other_accessible_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("services");
    fs::create_dir_all(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
    write_definition_for(&root, "zccache", &service_definition());

    let err = ServiceDefinitionLoader::new(&root)
        .load("zccache")
        .unwrap_err();
    match err {
        ServiceDefinitionError::InsecureDirectory(path) => assert_eq!(path, root),
        other => panic!("unexpected error: {other:?}"),
    }
}
