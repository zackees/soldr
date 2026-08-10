#![cfg(feature = "client")]

use running_process::broker::manifest;
use running_process::broker::protocol::{
    CacheManifest, CacheRoot, CacheRootKind, HostIdentity, Operation, StorageDisposition,
};

fn sample_manifest(root: &std::path::Path) -> CacheManifest {
    let host = running_process::broker::host_identity::current();
    CacheManifest {
        manifest_schema_version: 1,
        media_type: manifest::CACHE_MANIFEST_MEDIA_TYPE.to_string(),
        self_sha256: Vec::new(),
        host: Some(host),
        current_operation: Some(Operation {
            kind: 0,
            started_at_unix_ms: 1,
            expected_done_unix_ms: 0,
        }),
        valid_until_unix_ms: 0,
        service_name: "zccache".to_string(),
        service_version: "1.2.3".to_string(),
        broker_envelope_version: "v1".to_string(),
        created_at_unix_ms: 1,
        last_active_unix_ms: 2,
        roots: vec![CacheRoot {
            path: root.to_string_lossy().into_owned(),
            kind: CacheRootKind::CacheData as i32,
            estimated_size_bytes: 10,
            disposition: StorageDisposition::PruneWhenDormant as i32,
            labels: Default::default(),
            quota: None,
            teardown_hook: None,
            exclude_globs: Vec::new(),
            platform_paths: Default::default(),
            ownership: None,
            endpoint: None,
        }],
        current_daemon: None,
        cleanup_policy: None,
        broker_instance: "shared".to_string(),
        depends_on: Vec::new(),
        provides: Vec::new(),
        observability: None,
        bundle_id: "bundle".to_string(),
    }
}

#[test]
fn write_to_root_then_read_returns_same_manifest_content() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache-root");
    let manifest = sample_manifest(&root);

    manifest::write_to_root(&root, &manifest).unwrap();
    let read = manifest::read_manifest(&root.join(manifest::ROOT_MANIFEST_FILE)).unwrap();

    assert_eq!(read.service_name, manifest.service_name);
    assert_eq!(read.service_version, manifest.service_version);
    assert_eq!(read.roots.len(), 1);
    assert_eq!(read.self_sha256.len(), 32);
}

#[test]
fn write_to_central_enumerates_current_host_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let cache_root = tmp.path().join("cache");
    let registry = tmp.path().join("registry");
    let manifest = sample_manifest(&cache_root);

    let path = manifest::write_to_central_in_dir(&registry, "zccache", "1.2.3", &manifest).unwrap();
    assert!(path.exists());

    let manifests = manifest::enumerate_central(&registry);
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0].service_name, "zccache");
}

#[test]
fn enumerate_skips_prior_boot_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("registry");
    let mut manifest = sample_manifest(tmp.path());
    manifest.host = Some(HostIdentity {
        hostname: "host".to_string(),
        machine_id: "machine-a".to_string(),
        boot_id: "boot-old".to_string(),
        fs_dev_id: 0,
        namespace_id: String::new(),
    });
    manifest::write_to_central_in_dir(&registry, "zccache", "1.2.3", &manifest).unwrap();

    let current = HostIdentity {
        hostname: "host".to_string(),
        machine_id: "machine-a".to_string(),
        boot_id: "boot-new".to_string(),
        fs_dev_id: 0,
        namespace_id: String::new(),
    };
    assert!(manifest::enumerate_central_for_host(&registry, &current).is_empty());
}
