use super::{
    CARGO_CHEF_LOCAL_DIR_ENV_VAR, CARGO_CHEF_PINNED_VERSION, CRGX_LOCAL_DIR_ENV_VAR,
    MANAGED_CRGX_VERSION, MANAGED_ZCCACHE_PACKAGES, MANAGED_ZCCACHE_VERSION,
    ZCCACHE_LOCAL_DIR_ENV_VAR,
};

crate::timed_test!(zccache_runtime_contract_matches_rust_constants, {
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../contracts/zccache-runtime.v1.json"
    ))
    .expect("zccache runtime contract should be valid JSON");

    assert_eq!(
        contract["zccache"]["managed_version"].as_str(),
        Some(MANAGED_ZCCACHE_VERSION)
    );
    assert_eq!(
        contract["zccache"]["local_dir_env"].as_str(),
        Some(ZCCACHE_LOCAL_DIR_ENV_VAR)
    );
    assert_eq!(
        contract["crgx"]["managed_version"].as_str(),
        Some(MANAGED_CRGX_VERSION)
    );
    assert_eq!(
        contract["crgx"]["local_dir_env"].as_str(),
        Some(CRGX_LOCAL_DIR_ENV_VAR)
    );
    assert_eq!(
        contract["cargo_chef"]["managed_version"].as_str(),
        Some(CARGO_CHEF_PINNED_VERSION)
    );
    assert_eq!(
        contract["cargo_chef"]["local_dir_env"].as_str(),
        Some(CARGO_CHEF_LOCAL_DIR_ENV_VAR)
    );

    let contract_zccache_bins: Vec<&str> = contract["zccache"]["required_binaries"]
        .as_array()
        .expect("zccache.required_binaries should be an array")
        .iter()
        .map(|value| value.as_str().expect("binary name should be a string"))
        .collect();
    let rust_zccache_bins: Vec<&str> = MANAGED_ZCCACHE_PACKAGES
        .iter()
        .map(|(_, binary)| *binary)
        .collect();
    assert_eq!(contract_zccache_bins, rust_zccache_bins);

    let release_bins: Vec<&str> = contract["release_archive"]["required_binaries"]
        .as_array()
        .expect("release_archive.required_binaries should be an array")
        .iter()
        .map(|value| value.as_str().expect("binary name should be a string"))
        .collect();
    assert_eq!(
        release_bins,
        vec![
            "soldr",
            "zccache",
            "zccache-daemon",
            "zccache-fp",
            "crgx",
            "cargo-chef"
        ]
    );
});
