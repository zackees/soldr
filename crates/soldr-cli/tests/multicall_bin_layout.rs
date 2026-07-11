//! Regression guard for soldr#1592's one-codegen-unit release layout.

use std::path::PathBuf;

use soldr_cli::timed_test;

mod common;

timed_test!(soldr_is_the_only_compiled_binary_target, {
    if common::should_skip_source_tree_test("soldr_is_the_only_compiled_binary_target") {
        return;
    }
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest: toml::Value = toml::from_str(
        &std::fs::read_to_string(&manifest_path).expect("read soldr-cli Cargo.toml"),
    )
    .expect("parse soldr-cli Cargo.toml");
    let bins = manifest
        .get("bin")
        .and_then(toml::Value::as_array)
        .expect("[[bin]] entries");
    let names: Vec<&str> = bins
        .iter()
        .filter_map(|bin| bin.get("name").and_then(toml::Value::as_str))
        .collect();
    assert_eq!(names, ["soldr"]);
});
