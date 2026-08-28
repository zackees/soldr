//! Regression guard for soldr#1592's one-codegen-unit release layout.

use crate::common;

#[test]
fn soldr_is_the_only_compiled_binary_target() {
    let manifest_path = common::crate_root().join("Cargo.toml");
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
}
