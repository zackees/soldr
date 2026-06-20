use super::{
    CARGO_CHEF_LOCAL_DIR_ENV_VAR, CARGO_CHEF_PINNED_VERSION, CRGX_LOCAL_DIR_ENV_VAR,
    MANAGED_CRGX_VERSION, MANAGED_MATURIN_VERSION, MANAGED_ZCCACHE_PACKAGES,
    MANAGED_ZCCACHE_VERSION, ZCCACHE_LOCAL_DIR_ENV_VAR,
};

// When this test fails, the contributor most likely bumped only one
// side — the panic message names BOTH files so the fix-on-the-spot is
// obvious. See #703.
const DRIFT_HINT: &str =
    "Bump both in lockstep. See \"Bumping managed_zccache_version\" in CLAUDE.md.";

crate::timed_test!(zccache_runtime_contract_matches_rust_constants, {
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../contracts/zccache-runtime.v1.json"
    ))
    .expect("zccache runtime contract should be valid JSON");

    assert_eq!(
        contract["zccache"]["managed_version"].as_str(),
        Some(MANAGED_ZCCACHE_VERSION),
        "zccache.managed_version drift: contracts/zccache-runtime.v1.json says {:?} \
         but MANAGED_ZCCACHE_VERSION in crates/soldr-cli/src/fetch/mod.rs says {:?}. {DRIFT_HINT}",
        contract["zccache"]["managed_version"].as_str(),
        MANAGED_ZCCACHE_VERSION,
    );
    assert_eq!(
        contract["zccache"]["local_dir_env"].as_str(),
        Some(ZCCACHE_LOCAL_DIR_ENV_VAR),
        "zccache.local_dir_env drift: contracts/zccache-runtime.v1.json says {:?} \
         but ZCCACHE_LOCAL_DIR_ENV_VAR in crates/soldr-cli/src/fetch/mod.rs says {:?}. {DRIFT_HINT}",
        contract["zccache"]["local_dir_env"].as_str(),
        ZCCACHE_LOCAL_DIR_ENV_VAR,
    );
    assert_eq!(
        contract["crgx"]["managed_version"].as_str(),
        Some(MANAGED_CRGX_VERSION),
        "crgx.managed_version drift: contracts/zccache-runtime.v1.json says {:?} \
         but MANAGED_CRGX_VERSION in crates/soldr-cli/src/fetch/mod.rs says {:?}. {DRIFT_HINT}",
        contract["crgx"]["managed_version"].as_str(),
        MANAGED_CRGX_VERSION,
    );
    assert_eq!(
        contract["crgx"]["local_dir_env"].as_str(),
        Some(CRGX_LOCAL_DIR_ENV_VAR),
        "crgx.local_dir_env drift: contracts/zccache-runtime.v1.json says {:?} \
         but CRGX_LOCAL_DIR_ENV_VAR in crates/soldr-cli/src/fetch/mod.rs says {:?}. {DRIFT_HINT}",
        contract["crgx"]["local_dir_env"].as_str(),
        CRGX_LOCAL_DIR_ENV_VAR,
    );
    assert_eq!(
        contract["cargo_chef"]["managed_version"].as_str(),
        Some(CARGO_CHEF_PINNED_VERSION),
        "cargo_chef.managed_version drift: contracts/zccache-runtime.v1.json says {:?} \
         but CARGO_CHEF_PINNED_VERSION in crates/soldr-cli/src/fetch/mod.rs says {:?}. {DRIFT_HINT}",
        contract["cargo_chef"]["managed_version"].as_str(),
        CARGO_CHEF_PINNED_VERSION,
    );
    assert_eq!(
        contract["cargo_chef"]["local_dir_env"].as_str(),
        Some(CARGO_CHEF_LOCAL_DIR_ENV_VAR),
        "cargo_chef.local_dir_env drift: contracts/zccache-runtime.v1.json says {:?} \
         but CARGO_CHEF_LOCAL_DIR_ENV_VAR in crates/soldr-cli/src/fetch/mod.rs says {:?}. {DRIFT_HINT}",
        contract["cargo_chef"]["local_dir_env"].as_str(),
        CARGO_CHEF_LOCAL_DIR_ENV_VAR,
    );
    assert_eq!(
        contract["maturin"]["managed_version"].as_str(),
        Some(MANAGED_MATURIN_VERSION),
        "maturin.managed_version drift: contracts/zccache-runtime.v1.json says {:?} \
         but MANAGED_MATURIN_VERSION in crates/soldr-cli/src/fetch/mod.rs says {:?}. {DRIFT_HINT}",
        contract["maturin"]["managed_version"].as_str(),
        MANAGED_MATURIN_VERSION,
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
    assert_eq!(
        contract_zccache_bins, rust_zccache_bins,
        "zccache.required_binaries drift: contracts/zccache-runtime.v1.json says {contract_zccache_bins:?} \
         but MANAGED_ZCCACHE_PACKAGES in crates/soldr-cli/src/fetch/mod.rs yields {rust_zccache_bins:?}. \
         {DRIFT_HINT}",
    );

    let release_bins: Vec<&str> = contract["release_archive"]["required_binaries"]
        .as_array()
        .expect("release_archive.required_binaries should be an array")
        .iter()
        .map(|value| value.as_str().expect("binary name should be a string"))
        .collect();
    let expected_release_bins = vec![
        "soldr",
        "zccache",
        "zccache-daemon",
        "zccache-fp",
        "crgx",
        "cargo-chef",
    ];
    assert_eq!(
        release_bins, expected_release_bins,
        "release_archive.required_binaries drift: contracts/zccache-runtime.v1.json says \
         {release_bins:?} but the hard-coded expectation in this test says \
         {expected_release_bins:?}. Update both in lockstep when adding or removing a \
         packaged binary.",
    );
});
