#![cfg(feature = "client")]

use running_process::broker::manifest;
use running_process::broker::protocol::{CacheRootKind, StorageDisposition};
use running_process::cleanup::prune::{self, PruneOptions};

use super::common::sample_manifest;

#[test]
fn prune_dryrun_does_not_delete_root() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("registry");
    let cache = tmp.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("file"), b"data").unwrap();
    let manifest = sample_manifest(
        "zccache",
        "1.2.3",
        &cache,
        CacheRootKind::CacheData,
        StorageDisposition::PruneWhenDormant,
    );
    manifest::write_to_central_in_dir(&registry, "zccache", "1.2.3", &manifest).unwrap();

    let actions = prune::run(
        &registry,
        &PruneOptions {
            dormant_after_secs: Some(0),
            keep_current: false,
            keep_last: None,
            service: None,
            version: None,
            confirm: false,
        },
    )
    .unwrap();

    assert_eq!(actions.len(), 1);
    assert!(!actions[0].deleted);
    assert!(cache.exists());
}
