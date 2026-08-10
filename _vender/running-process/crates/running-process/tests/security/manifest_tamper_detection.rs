use prost::Message;
use running_process::broker::manifest::{self, ManifestError};
use running_process::broker::protocol::CacheManifest;

#[test]
fn bit_flipped_manifest_self_sha256_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("manifest.pb");
    let mut manifest = manifest::manifest_with_self_sha256(&sample_manifest()).unwrap();

    let mut bytes = Vec::new();
    manifest.encode(&mut bytes).unwrap();
    std::fs::write(&path, bytes).unwrap();
    manifest::read_manifest(&path).expect("signed manifest should be readable before tampering");

    manifest.self_sha256[0] ^= 0x01;

    let mut bytes = Vec::new();
    manifest.encode(&mut bytes).unwrap();
    std::fs::write(&path, bytes).unwrap();

    let err = manifest::read_manifest(&path).unwrap_err();
    assert!(
        matches!(err, ManifestError::Corruption),
        "bit-flipped self_sha256 must be rejected, got {err:?}"
    );
}

fn sample_manifest() -> CacheManifest {
    CacheManifest {
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
    }
}
