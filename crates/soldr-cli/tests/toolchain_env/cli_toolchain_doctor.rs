//! Integration tests for `soldr toolchain doctor [--json]` (issue #407
//! Phase 4). The subcommand exposes env-detection probes that
//! setup-soldr#133 will consume to delegate its TS modules
//! (`detect-musl-cc.ts`, `detect-shared-target-warning.ts`,
//! env-detection helpers from `diagnostics.ts`) to the soldr binary.

#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use serde_json::Value;
use std::fs;
use std::process::Command;

#[test]
fn toolchain_doctor_json_emits_schema_version_one_with_host_and_probes() {
    let workspace = unique_temp_dir("toolchain-doctor-json");

    let output = Command::new(common::soldr_bin())
        .args(["toolchain", "doctor", "--json"])
        .current_dir(&workspace)
        .output()
        .expect("failed to run soldr toolchain doctor --json");

    assert!(
        output.status.success(),
        "soldr toolchain doctor --json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("doctor --json stdout not JSON: {stdout}"));

    // Schema header.
    assert_eq!(parsed["schema_version"], Value::from(1));
    // Host triple summary.
    let host = parsed["host"].as_object().expect("host object");
    assert!(host.contains_key("os"), "host.os missing: {host:?}");
    assert!(host.contains_key("arch"), "host.arch missing: {host:?}");
    assert!(host.contains_key("libc"), "host.libc missing: {host:?}");
    assert!(host["os"].is_string());
    assert!(host["arch"].is_string());
    assert!(host["libc"].is_string());
    // Probes array contains all expected probe names in stable order.
    // soldr#1059 added the `cargo-on-path-shadowing` row.
    // soldr#1188 added the `rustlib-integrity` row.
    let probes = parsed["probes"].as_array().expect("probes array");
    assert_eq!(probes.len(), 4, "expected 4 probes, got {}", probes.len());
    assert_eq!(probes[0]["name"], Value::from("musl-cc"));
    assert_eq!(probes[1]["name"], Value::from("shared-target-warning"));
    assert_eq!(probes[2]["name"], Value::from("cargo-on-path-shadowing"));
    assert_eq!(probes[3]["name"], Value::from("rustlib-integrity"));
    // Each probe MUST carry an `ok` boolean and a `details` object.
    for probe in probes {
        assert!(probe["ok"].is_boolean(), "probe.ok must be bool: {probe}");
        assert!(
            probe["details"].is_object(),
            "probe.details must be object: {probe}"
        );
    }
    assert!(parsed["elapsed_ms"].is_number());
}

#[test]
fn toolchain_doctor_human_mode_prints_summary_lines() {
    let workspace = unique_temp_dir("toolchain-doctor-human");

    let output = Command::new(common::soldr_bin())
        .args(["toolchain", "doctor"])
        .current_dir(&workspace)
        .output()
        .expect("failed to run soldr toolchain doctor");

    assert!(
        output.status.success(),
        "soldr toolchain doctor (human) failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Human output should mention each probe name and the doctor banner.
    assert!(
        stdout.contains("soldr toolchain doctor"),
        "human output missing banner: {stdout}"
    );
    assert!(
        stdout.contains("musl-cc"),
        "human output missing musl-cc probe: {stdout}"
    );
    assert!(
        stdout.contains("shared-target-warning"),
        "human output missing shared-target-warning probe: {stdout}"
    );
}

#[test]
fn toolchain_doctor_detects_populated_fingerprint_dir_in_workspace() {
    let workspace = unique_temp_dir("toolchain-doctor-populated");
    // Seed a populated cargo `.fingerprint/` so the shared-target
    // probe reports would_warn=true.
    let fingerprint = workspace
        .join("target")
        .join("debug")
        .join(".fingerprint")
        .join("crate-abc");
    fs::create_dir_all(&fingerprint).expect("mkdir fingerprint");
    fs::write(fingerprint.join("invoked.timestamp"), "").expect("seed fingerprint");

    let output = Command::new(common::soldr_bin())
        .args(["toolchain", "doctor", "--json"])
        .current_dir(&workspace)
        .output()
        .expect("failed to run soldr toolchain doctor --json");

    assert!(
        output.status.success(),
        "soldr toolchain doctor --json failed (populated target)\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("doctor --json stdout not JSON");
    let shared = parsed["probes"]
        .as_array()
        .expect("probes array")
        .iter()
        .find(|p| p["name"] == "shared-target-warning")
        .expect("missing shared-target-warning probe in JSON");
    assert_eq!(
        shared["details"]["would_warn"],
        Value::from(true),
        "shared-target probe should report would_warn=true when fingerprints exist: {shared}"
    );
    assert!(
        shared["details"]["fingerprint_dirs_found"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "fingerprint_dirs_found should be >=1: {shared}"
    );
}
