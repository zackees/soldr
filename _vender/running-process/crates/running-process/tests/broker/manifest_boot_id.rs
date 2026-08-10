#![cfg(feature = "client")]

use running_process::broker::manifest;
use running_process::broker::protocol::{CacheManifest, HostIdentity};

#[test]
fn prior_boot_manifest_is_flagged_stale_by_enumerator() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("registry");
    let manifest = CacheManifest {
        manifest_schema_version: 1,
        media_type: manifest::CACHE_MANIFEST_MEDIA_TYPE.to_string(),
        self_sha256: Vec::new(),
        host: Some(HostIdentity {
            hostname: "host".to_string(),
            machine_id: "same-machine".to_string(),
            boot_id: "old-boot".to_string(),
            fs_dev_id: 0,
            namespace_id: String::new(),
        }),
        current_operation: None,
        valid_until_unix_ms: 0,
        service_name: "zccache".to_string(),
        service_version: "1.2.3".to_string(),
        broker_envelope_version: "v1".to_string(),
        created_at_unix_ms: 1,
        last_active_unix_ms: 2,
        roots: Vec::new(),
        current_daemon: None,
        cleanup_policy: None,
        broker_instance: "shared".to_string(),
        depends_on: Vec::new(),
        provides: Vec::new(),
        observability: None,
        bundle_id: String::new(),
    };
    manifest::write_to_central_in_dir(&registry, "zccache", "1.2.3", &manifest).unwrap();

    let current = HostIdentity {
        hostname: "host".to_string(),
        machine_id: "same-machine".to_string(),
        boot_id: "new-boot".to_string(),
        fs_dev_id: 0,
        namespace_id: String::new(),
    };
    assert_eq!(
        manifest::enumerate_central_for_host(&registry, &current).len(),
        0
    );
}
