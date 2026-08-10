#![cfg(feature = "client")]

use running_process::broker::manifest;
use running_process::broker::protocol::{CacheRootKind, StorageDisposition};
use running_process::cleanup::list;

use super::common::sample_manifest;

#[test]
fn list_returns_written_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("registry");
    let cache = tmp.path().join("cache");
    let manifest = sample_manifest(
        "zccache",
        "1.2.3",
        &cache,
        CacheRootKind::CacheData,
        StorageDisposition::PruneWhenDormant,
    );
    manifest::write_to_central_in_dir(&registry, "zccache", "1.2.3", &manifest).unwrap();

    let manifests = list::list(&registry);
    assert_eq!(manifests.len(), 1);
    assert!(list::render_json(&manifests).contains("\"schema_version\":1"));
}
