#![cfg(feature = "client")]

use running_process::broker::manifest;
use running_process::broker::protocol::{
    CacheManifest, CacheRoot, CacheRootKind, StorageDisposition,
};

pub fn sample_manifest(
    service: &str,
    version: &str,
    root: &std::path::Path,
    kind: CacheRootKind,
    disposition: StorageDisposition,
) -> CacheManifest {
    CacheManifest {
        manifest_schema_version: 1,
        media_type: manifest::CACHE_MANIFEST_MEDIA_TYPE.to_string(),
        self_sha256: Vec::new(),
        host: Some(running_process::broker::host_identity::current()),
        current_operation: None,
        valid_until_unix_ms: 0,
        service_name: service.to_string(),
        service_version: version.to_string(),
        broker_envelope_version: "v1".to_string(),
        created_at_unix_ms: 1,
        last_active_unix_ms: 1,
        roots: vec![CacheRoot {
            path: root.to_string_lossy().into_owned(),
            kind: kind as i32,
            estimated_size_bytes: 1,
            disposition: disposition as i32,
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
        bundle_id: String::new(),
    }
}
