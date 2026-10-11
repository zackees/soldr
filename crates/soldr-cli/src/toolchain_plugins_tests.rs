//! Hermetic tests for `[soldr.plugins]` prebuilt-first acquisition
//! (soldr#3699). No network: fetch and `cargo install` are injected.

use super::*;
use std::cell::Cell;
use std::path::PathBuf;

fn ok_fetch(version: &str) -> impl FnOnce(&str, &VersionSpec) -> Result<FetchResult, SoldrError> {
    let version = version.to_string();
    move |_, _| {
        Ok(FetchResult {
            binary_path: PathBuf::from("/fixture/bin/tool"),
            version,
            cached: true,
        })
    }
}

fn nextest_pin() -> String {
    crate::fetch::known_tools::CARGO_NEXTEST_PINNED_VERSION.to_string()
}

#[test]
fn registered_nextest_uses_prebuilt_and_never_spawns_cargo_install() {
    let spec = PluginSpec::Version("0.9".to_string());
    let installs = Cell::new(0);
    let fetched = Cell::new(false);
    let pin = nextest_pin();
    let code = install_plugin_with(
        "cargo-nextest",
        &spec,
        |name, version| {
            fetched.set(true);
            assert_eq!(name, "cargo-nextest");
            assert!(matches!(version, VersionSpec::Exact(v) if *v == pin));
            ok_fetch(&pin)(name, version)
        },
        || {
            installs.set(installs.get() + 1);
            Ok(0)
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    assert!(fetched.get());
    assert_eq!(installs.get(), 0, "cargo install must not be spawned");
}

#[test]
fn unregistered_plugin_still_uses_cargo_install() {
    let spec = PluginSpec::Version("1".to_string());
    let installs = Cell::new(0);
    let code = install_plugin_with(
        "some-unregistered-crate",
        &spec,
        |_, _| panic!("must not fetch an unregistered plugin"),
        || {
            installs.set(installs.get() + 1);
            Ok(7)
        },
    )
    .unwrap();
    assert_eq!(code, 7);
    assert_eq!(installs.get(), 1);
}

#[test]
fn requirement_not_satisfied_by_pin_falls_back_to_cargo_install() {
    // The pin is 0.9.x; a 0.8 requirement must not be served by it.
    let spec = PluginSpec::Version("=0.8.0".to_string());
    let installs = Cell::new(0);
    install_plugin_with(
        "cargo-nextest",
        &spec,
        |_, _| panic!("must not fetch when the pin cannot satisfy the requirement"),
        || {
            installs.set(installs.get() + 1);
            Ok(0)
        },
    )
    .unwrap();
    assert_eq!(installs.get(), 1);
}

#[test]
fn latest_prebuilt_outside_requirement_falls_back() {
    // cargo-deny has no pin: latest is fetched, then checked.
    let spec = PluginSpec::Version("0.14".to_string());
    let installs = Cell::new(0);
    install_plugin_with("cargo-deny", &spec, ok_fetch("0.16.1"), || {
        installs.set(installs.get() + 1);
        Ok(0)
    })
    .unwrap();
    assert_eq!(installs.get(), 1);
}

#[test]
fn latest_prebuilt_inside_requirement_is_used() {
    let spec = PluginSpec::Version("*".to_string());
    let code = install_plugin_with("cargo-deny", &spec, ok_fetch("0.16.1"), || {
        panic!("cargo install must not run")
    })
    .unwrap();
    assert_eq!(code, 0);
}

#[test]
fn fetch_failure_falls_back_to_cargo_install() {
    let spec = PluginSpec::Version("0.9".to_string());
    let installs = Cell::new(0);
    install_plugin_with(
        "cargo-nextest",
        &spec,
        |_, _| Err(SoldrError::Network("offline fixture".to_string())),
        || {
            installs.set(installs.get() + 1);
            Ok(0)
        },
    )
    .unwrap();
    assert_eq!(installs.get(), 1);
}

#[test]
fn features_request_cannot_be_served_by_a_prebuilt() {
    let spec = PluginSpec::Detailed {
        version: None,
        locked: None,
        features: Some(vec!["extra".to_string()]),
        no_default_features: None,
    };
    let installs = Cell::new(0);
    install_plugin_with(
        "cargo-nextest",
        &spec,
        |_, _| panic!("must not fetch"),
        || {
            installs.set(installs.get() + 1);
            Ok(0)
        },
    )
    .unwrap();
    assert_eq!(installs.get(), 1);
}
