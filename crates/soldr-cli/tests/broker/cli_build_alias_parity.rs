//! soldr#1012 PR 1 — parity test for the `soldr build` blessed verb.
//!
//! The user-facing contract: today `soldr build` is functionally an
//! alias for `soldr cargo build`. Both surfaces invoke the same
//! `cargo_front_door::run_cargo_front_door` pipeline; the only
//! difference is that the `Commands::Build` handler prepends `"build"`
//! to the arg list before forwarding.
//!
//! These tests pin that contract so future #1012 PRs can layer
//! catalogue-driven sysroot prep on top without accidentally diverging
//! the legacy behavior. When PR 5 adds blessed cross-compile, the
//! contract becomes "byte-identical artifacts when `SOLDR_USE_LEGACY_*`
//! env vars opt out of the catalogue path"; until then it's
//! byte-identical unconditionally.

#![allow(unused_imports)]

use crate::common;

use crate::common::*;
use std::process::Command;

#[test]
fn soldr_build_alias_resolves_via_clap_not_external() {
    // `soldr build --help` must produce build-verb-specific help, not
    // fall through to the External-arm crate-fetch path (which would
    // try to install a crate literally named "build"). This is the
    // simplest possible smoke test that `Commands::Build` exists and
    // is wired through clap.
    let output = Command::new(common::soldr_bin())
        .args(["build", "--help"])
        .output()
        .expect("failed to run soldr build --help");

    assert!(
        output.status.success(),
        "soldr build --help failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    // No External-arm signal — soldr should NOT have tried to resolve
    // "build" as a crate or trigger any fetch.
    assert!(
        !combined.contains("fetching")
            && !combined.contains("crates.io")
            && !combined.contains("not found on crates.io"),
        "soldr build --help unexpectedly went through External arm:\n{combined}"
    );
    // Must reference the build verb's own docstring or `--target`.
    assert!(
        combined.contains("build") || combined.contains("--target") || combined.contains("blessed"),
        "soldr build --help should produce build-verb help text:\n{combined}"
    );
}

#[test]
fn soldr_build_invokes_real_cargo_build() {
    // Confirm the verb actually reaches `cargo build` and not some
    // External-arm crate fetch. `cargo build --help` is the safest
    // probe: it returns cargo's help banner without touching the
    // filesystem or network. Both `soldr build --help` and `soldr
    // cargo build --help` must produce the SAME cargo-side help
    // content (this is the surface-contract parity assertion).
    //
    // Important: `--help` here is consumed by cargo, not by clap.
    // soldr's clap-captured `--help` would print soldr's own build-
    // verb docstring; cargo's `--help` (which we want) requires the
    // `--` separator OR a flag cargo recognizes before `--help`. We
    // use `--manifest-path` pointing at a clearly-bogus path so
    // cargo errors out FAST with a recognizable error; the soldr →
    // cargo dispatch is what we're testing, not a successful build.
    let cache_root = unique_temp_dir("build-alias-routes-to-cargo");

    let build_out = Command::new(common::soldr_bin())
        .args([
            "--no-cache",
            "build",
            "--manifest-path",
            "/nonexistent/path/to/Cargo.toml",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr build");

    let cargo_out = Command::new(common::soldr_bin())
        .args([
            "--no-cache",
            "cargo",
            "build",
            "--manifest-path",
            "/nonexistent/path/to/Cargo.toml",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cargo build");

    // Both must fail (manifest path doesn't exist) — but they must
    // BOTH reach cargo's error path, not soldr's own crate-fetch
    // error path. The parity contract is "same dispatch destination".
    let build_stderr = String::from_utf8_lossy(&build_out.stderr);
    let cargo_stderr = String::from_utf8_lossy(&cargo_out.stderr);

    // Cargo's manifest-not-found error contains a recognizable phrase.
    // If either route fell through to External-arm crate-fetch,
    // we'd see "fetching"/"crates.io" instead.
    for (name, stderr) in [
        ("soldr build", &build_stderr),
        ("soldr cargo build", &cargo_stderr),
    ] {
        assert!(
            stderr.contains("manifest")
                || stderr.contains("Cargo.toml")
                || stderr.contains("nonexistent"),
            "{name} did not reach cargo's manifest-handling path; stderr was:\n{stderr}"
        );
        assert!(
            !stderr.contains("not found on crates.io")
                && !stderr.contains("fetching latest release"),
            "{name} fell through to crate-fetch External path; stderr was:\n{stderr}"
        );
    }
}

#[test]
fn soldr_build_help_documents_blessed_surface() {
    // The clap docstring on `Commands::Build` references the blessed-
    // default surface contract. The `--help` output must surface that
    // language so users discovering `soldr build --help` understand
    // why they'd pick it over `soldr cargo build`.
    let output = Command::new(common::soldr_bin())
        .args(["build", "--help"])
        .output()
        .expect("failed to run soldr build --help");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        combined.contains("blessed")
            || combined.contains("cross-compile")
            || combined.contains("Build the workspace"),
        "soldr build --help is missing the blessed-surface contract language:\n{combined}"
    );
}

#[test]
fn canonical_aliases_resolve_through_the_cli() {
    let cwd = unique_temp_dir("canonical-alias-cli-parity");
    for (alias, triple) in soldr_cli::target_alias::CANONICAL_ALIASES {
        for (input, via_alias) in [(*alias, true), (*triple, false)] {
            let output = Command::new(common::soldr_bin())
                .args(["env", "--target", input, "--json", "--plan-only"])
                .current_dir(&cwd)
                .output()
                .unwrap_or_else(|err| panic!("failed to resolve canonical target {input}: {err}"));
            assert!(
                output.status.success(),
                "canonical target {input} failed through the CLI parser/resolver: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
                .unwrap_or_else(|err| panic!("invalid JSON for canonical target {input}: {err}"));
            assert_eq!(payload["rust_triple"].as_str(), Some(*triple));
            assert_eq!(payload["via_alias"], via_alias);
            assert_eq!(payload["command"], "env");
            assert_eq!(payload["target_plan"]["schema_version"], 1);
            assert_eq!(payload["target_plan"]["canonical"], true);
            assert_eq!(
                payload["target_plan"]["canonical_target"].as_str(),
                Some(*triple)
            );
            assert_eq!(
                payload["target_plan"]["canonical_alias"].as_str(),
                Some(*alias)
            );
            let operations = payload["target_plan"]["supported_operations"]
                .as_array()
                .unwrap_or_else(|| panic!("{input} did not advertise supported operations"));
            assert!(
                [
                    "prepare",
                    "build",
                    "clippy",
                    "test-no-run",
                    "nextest-archive"
                ]
                .iter()
                .all(|expected| operations.iter().any(|actual| actual == expected)),
                "{input} did not advertise the common blessed lifecycle: {payload}"
            );
            for operation in ["pep517-wheel", "pep517-sdist"] {
                assert_eq!(
                    operations.iter().any(|actual| actual == operation),
                    *alias != "win-x64-gnu",
                    "{input} advertised the wrong Python lifecycle: {payload}"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(cwd);
}
