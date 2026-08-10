#![cfg(feature = "client")]

use running_process::broker::manifest;
use running_process::broker::protocol::{CacheRootKind, StorageDisposition};
use running_process::cleanup::uninstall;

use super::common::sample_manifest;

#[test]
fn uninstall_keep_config_preserves_config_root() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = tmp.path().join("registry");
    let config = tmp.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("settings.toml"), b"data").unwrap();
    let manifest = sample_manifest(
        "zccache",
        "1.2.3",
        &config,
        CacheRootKind::CacheConfig,
        StorageDisposition::PruneOnUninstall,
    );
    manifest::write_to_central_in_dir(&registry, "zccache", "1.2.3", &manifest).unwrap();

    let actions = uninstall::run(&registry, "zccache", true, true).unwrap();

    assert_eq!(actions.len(), 1);
    assert!(actions[0].skipped);
    assert!(config.exists());
}
