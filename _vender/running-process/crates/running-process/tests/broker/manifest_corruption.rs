#![cfg(feature = "client")]

use prost::Message;
use running_process::broker::manifest::{self, ManifestError};
use running_process::broker::protocol::CacheManifest;

#[test]
fn tampered_manifest_hash_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("registry");
    let manifest = CacheManifest {
        manifest_schema_version: 1,
        media_type: manifest::CACHE_MANIFEST_MEDIA_TYPE.to_string(),
        self_sha256: Vec::new(),
        host: Some(running_process::broker::host_identity::current()),
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
    let path = manifest::write_to_central_in_dir(&registry, "zccache", "1.2.3", &manifest).unwrap();
    let mut decoded = manifest::read_manifest(&path).unwrap();
    decoded.service_name = "tampered".to_string();
    let mut bytes = Vec::new();
    decoded.encode(&mut bytes).unwrap();
    std::fs::write(&path, bytes).unwrap();

    let err = manifest::read_manifest(&path).unwrap_err();
    assert!(matches!(err, ManifestError::Corruption));
}
