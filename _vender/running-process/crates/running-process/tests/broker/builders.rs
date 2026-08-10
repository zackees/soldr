//! #433 R2: builder ergonomics for the two consumer registration messages.

use running_process::broker::builders::{CacheManifestBuilder, ServiceDefinitionBuilder};
use running_process::broker::manifest::read_manifest;
use running_process::broker::protocol::{BrokerIsolation, CacheRootKind};
use running_process::broker::server::ServiceDefinitionLoader;

#[cfg(windows)]
const ABS_BINARY: &str = "C:\\tools\\zccache.exe";
#[cfg(not(windows))]
const ABS_BINARY: &str = "/usr/local/bin/zccache";

#[test]
fn service_definition_builder_builds_validated_shared_broker() {
    let definition = ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .min_version("1.10.0")
        .allow_version("1.11.20")
        .label("team", "cache")
        .build()
        .expect("valid shared-broker definition");

    assert_eq!(definition.service_name, "zccache");
    assert_eq!(definition.isolation, BrokerIsolation::SharedBroker as i32);
    assert_eq!(definition.version_allow_list, vec!["1.11.20".to_string()]);
    assert_eq!(
        definition.labels.get("team").map(String::as_str),
        Some("cache")
    );
}

#[test]
fn service_definition_builder_install_in_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = ServiceDefinitionBuilder::shared_broker("zccache", ABS_BINARY)
        .min_version("1.10.0")
        .allow_version("1.11.20")
        .install_in(dir.path())
        .expect("install service definition");
    assert!(path.exists());

    let loaded = ServiceDefinitionLoader::new(dir.path())
        .load("zccache")
        .expect("load installed definition");
    assert_eq!(loaded.service_name, "zccache");
    assert_eq!(loaded.min_version, "1.10.0");
}

#[test]
fn service_definition_builder_rejects_relative_binary_path() {
    let error = ServiceDefinitionBuilder::shared_broker("zccache", "relative/zccache")
        .build()
        .expect_err("relative binary_path must be rejected");
    // Validation runs on build; the exact variant is a path error.
    let _ = error;
}

#[test]
fn explicit_instance_builder_sets_instance() {
    let definition = ServiceDefinitionBuilder::explicit_instance("zccache", ABS_BINARY, "ci-pool")
        .allow_version("1.11.20")
        .build()
        .expect("valid explicit-instance definition");
    assert_eq!(
        definition.isolation,
        BrokerIsolation::ExplicitInstance as i32
    );
    assert_eq!(definition.explicit_instance, "ci-pool");
}

#[test]
fn cache_manifest_builder_builds_sealed_manifest() {
    let manifest = CacheManifestBuilder::new("zccache", "1.11.20")
        .broker_instance("shared")
        .root(CacheRootKind::CacheData, "/var/cache/zccache")
        .build()
        .expect("seal manifest");

    assert_eq!(manifest.service_name, "zccache");
    assert_eq!(manifest.service_version, "1.11.20");
    assert_eq!(manifest.self_sha256.len(), 32);
    assert_eq!(manifest.roots.len(), 1);
    assert_eq!(manifest.roots[0].kind, CacheRootKind::CacheData as i32);
    assert!(manifest.host.is_some());
}

#[test]
fn cache_manifest_builder_publish_in_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let path = CacheManifestBuilder::new("zccache", "1.11.20")
        .broker_instance("shared")
        .root(CacheRootKind::CacheData, "/var/cache/zccache")
        .publish_in(dir.path())
        .expect("publish manifest");
    assert!(path.exists());

    let loaded = read_manifest(&path).expect("read published manifest");
    assert_eq!(loaded.service_name, "zccache");
    assert_eq!(loaded.service_version, "1.11.20");
}
