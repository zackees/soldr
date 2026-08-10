use prost::Message;
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    ensure_service_definition_dir, service_definition_path,
    validate_service_definition_for_service, ServiceDefinitionError, ServiceDefinitionLoader,
    SERVICE_DEF_EXTENSION,
};

#[test]
fn service_definition_loader_rejects_file_claiming_different_service() {
    let tmp = tempfile::tempdir().unwrap();
    ensure_service_definition_dir(tmp.path()).unwrap();
    let requested_service = "zccache";
    let path = tmp
        .path()
        .join(format!("{requested_service}.{SERVICE_DEF_EXTENSION}"));
    let mut definition = valid_service_definition(requested_service);
    definition.service_name = "evil-cache".into();
    std::fs::write(&path, definition.encode_to_vec()).unwrap();

    let err = ServiceDefinitionLoader::new(tmp.path())
        .load(requested_service)
        .unwrap_err();

    assert!(
        matches!(
            err,
            ServiceDefinitionError::ServiceNameMismatch {
                ref requested,
                ref actual,
            } if requested == requested_service && actual == "evil-cache"
        ),
        "loader must bind file contents to the requested service name, got {err:?}"
    );
}

#[test]
fn service_definition_paths_reject_path_confusion_service_names() {
    let tmp = tempfile::tempdir().unwrap();

    for service_name in [
        "../zccache",
        r"..\zccache",
        "zccache.service",
        "zccache/service",
        "Zccache",
    ] {
        assert!(
            service_definition_path(tmp.path(), service_name).is_err(),
            "service definition path should reject confused service name {service_name:?}"
        );
    }
}

#[test]
fn service_definition_validation_rejects_relative_binary_paths() {
    for (field, definition) in [
        {
            let mut definition = valid_service_definition("zccache");
            definition.binary_path = "bin/zccache".into();
            ("binary_path", definition)
        },
        {
            let mut definition = valid_service_definition("zccache");
            definition.per_version_binary_dir = "../versions".into();
            ("per_version_binary_dir", definition)
        },
    ] {
        let err = validate_service_definition_for_service(&definition, "zccache").unwrap_err();
        assert!(
            matches!(
                err,
                ServiceDefinitionError::InvalidPath {
                    field: rejected_field,
                    ..
                } if rejected_field == field
            ),
            "{field} should reject relative path confusion, got {err:?}"
        );
    }
}

fn valid_service_definition(service_name: &str) -> ServiceDefinition {
    ServiceDefinition {
        service_name: service_name.into(),
        binary_path: platform_absolute_path("zccache"),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: platform_absolute_path("zccache-versions"),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn platform_absolute_path(leaf: &str) -> String {
    if cfg!(windows) {
        format!(r"C:\running-process-test\{leaf}.exe")
    } else {
        format!("/opt/running-process-test/{leaf}")
    }
}
