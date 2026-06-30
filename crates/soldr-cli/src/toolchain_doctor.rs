//! `soldr toolchain doctor [--json]` — env-detection probes that ship
//! the diagnostic intel `setup-soldr` used to compute in TypeScript.
//! Phase 4 of #407 (Wave 3.3 of zackees/soldr#514).
//!
//! Ports the env-detection halves of three setup-soldr modules:
//!   * `detect-musl-cc.ts` — locate `musl-gcc` / `musl-clang` so
//!     `*-unknown-linux-musl` cross-compiles don't silently link against
//!     host glibc.
//!   * `detect-shared-target-warning.ts` — flag a pre-populated cargo
//!     `target/` directory that will collide with rust-plan restore.
//!   * `diagnostics.ts` — env-detection helpers only (the GitHub
//!     Actions dump stays in setup-soldr).
//!
//! Each probe is a pure function over its inputs; the wrapping
//! [`run_toolchain_doctor`] orchestrates them and emits either a
//! human-readable summary or the stable `schema_version: 1` JSON
//! payload that setup-soldr#133 will consume.

use serde::Serialize;
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::core::SoldrError;

const SCHEMA_VERSION: u32 = 1;

/// Distinct from the top-level `soldr doctor` subcommand: this one is
/// namespaced under `toolchain` and performs pure env-detection probes
/// rather than the full system check.
pub(crate) fn run_toolchain_doctor(json: bool) -> Result<i32, SoldrError> {
    let started = Instant::now();
    let host = HostInfo::detect();
    let workspace = std::env::current_dir().map_err(SoldrError::from)?;

    let probes = vec![
        probe_musl_cc(&host),
        probe_shared_target_warning(&workspace),
        probe_cargo_on_path_shadowing(),
    ];

    let all_ok = probes.iter().all(|p| p.ok);
    let output = DoctorOutput {
        schema_version: SCHEMA_VERSION,
        host: host.clone(),
        probes,
        elapsed_ms: started.elapsed().as_millis(),
    };

    if json {
        emit_json(&output)?;
    } else {
        emit_human(&output);
    }

    Ok(if all_ok { 0 } else { 1 })
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostInfo {
    pub os: String,
    pub arch: String,
    pub libc: String,
}

impl HostInfo {
    pub(crate) fn detect() -> HostInfo {
        HostInfo {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            libc: detect_libc(),
        }
    }
}

#[cfg(target_os = "linux")]
fn detect_libc() -> String {
    // We can't introspect glibc-vs-musl at compile-time without a build
    // script; default to "gnu" for the standard linux-gnu host triple
    // and leave the musl-cc probe to determine whether musl tooling is
    // available.
    "gnu".to_string()
}

#[cfg(target_os = "windows")]
fn detect_libc() -> String {
    // CLAUDE.md mandates MSVC on Windows; the GNU host is opt-in.
    "msvc".to_string()
}

#[cfg(target_os = "macos")]
fn detect_libc() -> String {
    "darwin".to_string()
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn detect_libc() -> String {
    "unknown".to_string()
}

#[derive(Serialize, Debug)]
pub(crate) struct DoctorOutput {
    pub schema_version: u32,
    pub host: HostInfo,
    pub probes: Vec<ProbeResult>,
    pub elapsed_ms: u128,
}

#[derive(Serialize, Debug)]
pub(crate) struct ProbeResult {
    pub name: String,
    pub ok: bool,
    pub details: Value,
}

/// Probe name for `musl-gcc` / `musl-clang` detection (ports
/// `detect-musl-cc.ts`'s env-detection half).
pub(crate) const PROBE_MUSL_CC: &str = "musl-cc";
/// Probe name for the pre-populated-target warning (ports
/// `detect-shared-target-warning.ts`).
pub(crate) const PROBE_SHARED_TARGET_WARNING: &str = "shared-target-warning";
/// Probe name for the soldr#1059 Chocolatey-cargo-shadowing detector.
pub(crate) const PROBE_CARGO_ON_PATH_SHADOWING: &str = "cargo-on-path-shadowing";

/// Detect availability of a musl C compiler on PATH. On non-Linux
/// hosts the probe is skipped (ok=true, details=`{"skipped": "not-linux"}`)
/// since musl cross-compilers are only meaningful for
/// `*-unknown-linux-musl` targets.
pub(crate) fn probe_musl_cc(host: &HostInfo) -> ProbeResult {
    if host.os != "linux" {
        return ProbeResult {
            name: PROBE_MUSL_CC.to_string(),
            ok: true,
            details: json!({ "skipped": "not-linux" }),
        };
    }

    let candidates = ["musl-gcc", "musl-clang"];
    for cmd in candidates {
        if let Some(found) = find_on_path(cmd) {
            let version = read_version(&found).unwrap_or_default();
            return ProbeResult {
                name: PROBE_MUSL_CC.to_string(),
                ok: true,
                details: json!({
                    "musl_cc": found.display().to_string(),
                    "tool": cmd,
                    "version": version,
                }),
            };
        }
    }

    // No musl-cc found. Not all linux hosts need musl tooling, so we
    // return ok=true with a `found: false` detail rather than failing
    // the entire doctor run. setup-soldr's TS predecessor was similarly
    // permissive — it only emitted a warning at most.
    ProbeResult {
        name: PROBE_MUSL_CC.to_string(),
        ok: true,
        details: json!({
            "found": false,
            "searched": candidates,
        }),
    }
}

/// Probe whether the current workspace already contains a pre-populated
/// `target/` directory that would collide with `rust-plan restore`.
/// Mirrors the `.fingerprint/`-based detector in `rust_plan.rs` (added
/// in PR #508) and `detect-shared-target-warning.ts`.
pub(crate) fn probe_shared_target_warning(workspace: &Path) -> ProbeResult {
    let target_dir = workspace.join("target");
    if !target_dir.is_dir() {
        return ProbeResult {
            name: PROBE_SHARED_TARGET_WARNING.to_string(),
            ok: true,
            details: json!({
                "target_dir": target_dir.display().to_string(),
                "fingerprint_dirs_found": 0,
                "would_warn": false,
                "reason": "no-target-dir",
            }),
        };
    }

    let fingerprint_dirs_found = crate::rust_plan::count_populated_fingerprint_dirs(&target_dir, 3);
    let would_warn = fingerprint_dirs_found > 0;
    // ok=true even when would_warn=true: the probe successfully made a
    // diagnosis. The caller decides whether the warning is a blocker.
    ProbeResult {
        name: PROBE_SHARED_TARGET_WARNING.to_string(),
        ok: true,
        details: json!({
            "target_dir": target_dir.display().to_string(),
            "fingerprint_dirs_found": fingerprint_dirs_found,
            "would_warn": would_warn,
        }),
    }
}

/// PATH lookup helper. Mirrors setup-soldr's `findOnPathSync`.
/// soldr#1059 — probe the first `cargo` on PATH and classify whether
/// it honors per-crate `rust-toolchain.toml` overrides. The probe
/// returns `ok = true` even when the resolved cargo is a shadowing
/// shim — the diagnosis itself succeeded; the *finding* (in
/// `details.honors_rust_toolchain_toml`) tells the caller whether to
/// act on it. Mirrors how `probe_shared_target_warning` already
/// reports `would_warn: true` with `ok: true`.
pub(crate) fn probe_cargo_on_path_shadowing() -> ProbeResult {
    let Some(finding) = crate::cargo_path_check::detect_cargo_on_path() else {
        return ProbeResult {
            name: PROBE_CARGO_ON_PATH_SHADOWING.to_string(),
            ok: true,
            details: json!({
                "cargo_on_path": null,
                "found": false,
            }),
        };
    };
    let warning = crate::cargo_path_check::warning_for(&finding);
    ProbeResult {
        name: PROBE_CARGO_ON_PATH_SHADOWING.to_string(),
        ok: true,
        details: json!({
            "cargo_on_path": finding.resolved.display().to_string(),
            "classification": finding.classification.label(),
            "honors_rust_toolchain_toml": finding.honors_rust_toolchain_toml,
            "would_warn": warning.is_some(),
        }),
    }
}

fn find_on_path(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) { &["", ".exe"] } else { &[""] };
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in exts {
            let candidate: PathBuf = if ext.is_empty() {
                dir.join(cmd)
            } else {
                dir.join(format!("{cmd}{ext}"))
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Run `<bin> --version` and capture the first line of stdout. Returns
/// `None` when the spawn fails, the exit code is non-zero, or stdout is
/// empty.
fn read_version<P: AsRef<OsStr>>(bin: P) -> Option<String> {
    let output = std::process::Command::new(bin.as_ref())
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .map(str::to_string)?;
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

fn emit_json(output: &DoctorOutput) -> Result<(), SoldrError> {
    let payload = serde_json::to_string_pretty(output)
        .map_err(|e| SoldrError::Other(format!("doctor: failed to serialize JSON: {e}")))?;
    println!("{payload}");
    Ok(())
}

fn emit_human(output: &DoctorOutput) {
    println!(
        "soldr toolchain doctor: host {os}/{arch}/{libc}",
        os = output.host.os,
        arch = output.host.arch,
        libc = output.host.libc,
    );
    for probe in &output.probes {
        let status = if probe.ok { "ok" } else { "FAIL" };
        println!("  [{status}] {} {}", probe.name, probe.details);
    }
    println!("soldr toolchain doctor: {} ms", output.elapsed_ms);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("soldr-doctor-test-{label}-{nanos}"));
        fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    crate::timed_test!(host_info_serializes_with_three_string_fields, {
        let host = HostInfo {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            libc: "gnu".to_string(),
        };
        let json = serde_json::to_value(&host).expect("serialize");
        assert_eq!(json["os"], Value::from("linux"));
        assert_eq!(json["arch"], Value::from("x86_64"));
        assert_eq!(json["libc"], Value::from("gnu"));
    });

    crate::timed_test!(host_info_detect_populates_all_fields, {
        let host = HostInfo::detect();
        assert!(!host.os.is_empty(), "os should not be empty: {host:?}");
        assert!(!host.arch.is_empty(), "arch should not be empty: {host:?}");
        assert!(!host.libc.is_empty(), "libc should not be empty: {host:?}");
    });

    crate::timed_test!(probe_musl_cc_is_skipped_on_non_linux, {
        let host = HostInfo {
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            libc: "darwin".to_string(),
        };
        let probe = probe_musl_cc(&host);
        assert_eq!(probe.name, PROBE_MUSL_CC);
        assert!(probe.ok, "non-linux probe should report ok=true");
        assert_eq!(
            probe.details["skipped"],
            Value::from("not-linux"),
            "non-linux probe should report skipped=not-linux: {}",
            probe.details
        );
    });

    crate::timed_test!(probe_musl_cc_on_linux_returns_search_metadata, {
        // We can't guarantee musl is installed on the test host, so just
        // assert that the linux path returns either a found-tool record
        // (with `musl_cc` populated) or a not-found record (`found: false`).
        let host = HostInfo {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            libc: "gnu".to_string(),
        };
        let probe = probe_musl_cc(&host);
        assert_eq!(probe.name, PROBE_MUSL_CC);
        assert!(probe.ok);
        assert!(
            probe.details.get("skipped").is_none(),
            "linux probe must NOT be skipped: {}",
            probe.details
        );
        let has_musl_cc = probe.details.get("musl_cc").is_some();
        let has_found_false = probe.details.get("found").and_then(Value::as_bool) == Some(false);
        assert!(
            has_musl_cc || has_found_false,
            "linux probe must report either a found musl-cc or found=false: {}",
            probe.details
        );
    });

    crate::timed_test!(probe_shared_target_warning_reports_no_target_dir, {
        let dir = tempdir("no-target");
        let probe = probe_shared_target_warning(&dir);
        assert_eq!(probe.name, PROBE_SHARED_TARGET_WARNING);
        assert!(probe.ok);
        assert_eq!(probe.details["would_warn"], Value::from(false));
        assert_eq!(probe.details["fingerprint_dirs_found"], Value::from(0));
        assert_eq!(probe.details["reason"], Value::from("no-target-dir"));
    });

    crate::timed_test!(probe_shared_target_warning_clean_target_dir, {
        let dir = tempdir("clean-target");
        // Empty target/ dir — exists but has no `.fingerprint/`.
        fs::create_dir_all(dir.join("target")).expect("mkdir target");
        let probe = probe_shared_target_warning(&dir);
        assert!(probe.ok);
        assert_eq!(probe.details["would_warn"], Value::from(false));
        assert_eq!(probe.details["fingerprint_dirs_found"], Value::from(0));
    });

    crate::timed_test!(
        probe_shared_target_warning_detects_populated_fingerprint_dir,
        {
            let dir = tempdir("populated-target");
            // Mirror cargo's layout: target/<profile>/.fingerprint/<crate>/...
            let fingerprint = dir.join("target").join("debug").join(".fingerprint");
            fs::create_dir_all(fingerprint.join("some-crate-abc123")).expect("mkdir fingerprint");
            fs::write(
                fingerprint
                    .join("some-crate-abc123")
                    .join("invoked.timestamp"),
                "",
            )
            .expect("seed fingerprint entry");

            let probe = probe_shared_target_warning(&dir);
            assert!(probe.ok);
            assert_eq!(probe.details["would_warn"], Value::from(true));
            assert!(
                probe.details["fingerprint_dirs_found"]
                    .as_u64()
                    .unwrap_or(0)
                    >= 1,
                "expected at least 1 fingerprint dir, got {}",
                probe.details["fingerprint_dirs_found"]
            );
        }
    );

    crate::timed_test!(doctor_output_serializes_schema_version_one, {
        let output = DoctorOutput {
            schema_version: SCHEMA_VERSION,
            host: HostInfo {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                libc: "gnu".to_string(),
            },
            probes: vec![
                ProbeResult {
                    name: PROBE_MUSL_CC.to_string(),
                    ok: true,
                    details: json!({"skipped": "not-linux"}),
                },
                ProbeResult {
                    name: PROBE_SHARED_TARGET_WARNING.to_string(),
                    ok: true,
                    details: json!({"would_warn": false, "fingerprint_dirs_found": 0}),
                },
            ],
            elapsed_ms: 7,
        };
        let json = serde_json::to_value(&output).expect("serialize");
        assert_eq!(json["schema_version"], Value::from(1));
        assert!(json["host"].is_object());
        let probes = json["probes"].as_array().expect("probes array");
        assert_eq!(probes.len(), 2);
        assert_eq!(probes[0]["name"], Value::from(PROBE_MUSL_CC));
        assert_eq!(probes[1]["name"], Value::from(PROBE_SHARED_TARGET_WARNING));
        assert!(json["elapsed_ms"].is_number());
    });
}
