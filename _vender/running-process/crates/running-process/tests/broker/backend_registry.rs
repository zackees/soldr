#![cfg(feature = "client")]

use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{BackendRegistry, BrokerInstanceKey};

use crate::backend_handle_common::verified_current_backend;

fn service_definition(service_name: &str, isolation: BrokerIsolation) -> ServiceDefinition {
    ServiceDefinition {
        service_name: service_name.into(),
        binary_path: "/usr/bin/backend".into(),
        isolation: isolation as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: String::new(),
        min_version: "1.11.20".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn handle(service_name: &str, version: &str) -> BackendHandle {
    verified_current_backend(service_name, version)
}

#[test]
fn registry_returns_registered_backend_for_hello() {
    let definition = service_definition("zccache", BrokerIsolation::SharedBroker);
    let instance = BrokerInstanceKey::from_service_definition(&definition).unwrap();
    let mut registry = BackendRegistry::new();
    let handle = handle("zccache", "1.11.20");
    let expected_pipe = handle.daemon_process.ipc_endpoint.path.clone();
    registry.insert(instance.clone(), handle);

    let registered = registry
        .registered_backend_for(&instance, &definition, "1.11.20")
        .unwrap();

    assert_eq!(registered.service_definition.service_name, "zccache");
    assert_eq!(registered.daemon_version, "1.11.20");
    assert_eq!(registered.backend_pipe, expected_pipe);
}

#[test]
fn registry_isolates_same_service_version_by_instance() {
    let shared = BrokerInstanceKey::Shared;
    let private = BrokerInstanceKey::Private {
        service_name: "zccache".into(),
    };
    let mut registry = BackendRegistry::new();
    registry.insert(shared.clone(), handle("zccache", "1.11.20"));

    assert!(registry.get(&shared, "zccache", "1.11.20").is_some());
    assert!(registry.get(&private, "zccache", "1.11.20").is_none());
}

#[test]
fn registry_replaces_existing_backend() {
    let instance = BrokerInstanceKey::Shared;
    let mut registry = BackendRegistry::new();

    assert!(registry
        .insert(instance.clone(), handle("zccache", "1.11.20"))
        .is_none());
    assert!(registry
        .insert(instance, handle("zccache", "1.11.20"))
        .is_some());
    assert_eq!(registry.len(), 1);
}
