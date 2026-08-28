//! End-to-end wiring for `soldr wheel` (soldr#2139 gap 1).
//!
//! Modeled on `cli_maturin.rs`: a fake maturin shell script records the
//! argument vector it was handed, which is how the dispatch chain
//! (`Commands::Wheel` -> `wheel_cmd::maturin_invocation` -> re-entry through
//! the existing `soldr maturin ...` execution path) is proven without a real
//! maturin, a real cross toolchain, or a Linux host.
//!
//! Same `cfg(not(windows))` gate as `cli_maturin.rs`: the fixture is a `sh`
//! script.

use crate::common::*;
use soldr_cli::fetch::MANAGED_MATURIN_VERSION;
use std::path::{Path, PathBuf};

/// Logs argv and exits — no nested cargo, because this test is about the
/// argument vector soldr composed, not about the compile that follows.
fn fake_maturin_script(log_path: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"maturin args=$*\" >> \"{0}\"\n\
         if [ \"${{1:-}}\" = \"--version\" ]; then\n\
           echo \"maturin {1}\"\n\
         fi\n\
         exit 0\n",
        log_path.display(),
        MANAGED_MATURIN_VERSION
    )
}

fn seed_cached_fake_maturin(cache_root: &Path, log_path: &Path) -> PathBuf {
    let dir = cache_root
        .join("bin")
        .join(format!("maturin-{MANAGED_MATURIN_VERSION}"));
    std::fs::create_dir_all(&dir).expect("create fake maturin cache dir");
    let maturin = dir.join("maturin");
    write_fake_script(&maturin, &fake_maturin_script(log_path));
    maturin
}

fn maturin_argv(log: &str) -> Vec<String> {
    let line = log
        .lines()
        .find(|line| line.starts_with("maturin args=build "))
        .unwrap_or_else(|| panic!("missing maturin build invocation in log: {log}"));
    line.trim_start_matches("maturin args=")
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn flag_value(argv: &[String], flag: &str) -> Option<String> {
    argv.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| argv.get(idx + 1))
        .cloned()
}

/// The host triple spelled as a friendly alias, when soldr has one for it.
///
/// Using the *host* keeps the fixture off the cross-compile path (no sysroot
/// download, no `cargo metadata`), while still driving alias resolution end to
/// end: `linux-x64` must reach maturin as `x86_64-unknown-linux-gnu`.
fn host_alias_and_triple() -> (String, String) {
    let triple = soldr_cli::pyo3_detect::host_triple().to_string();
    let alias = match triple.as_str() {
        "x86_64-unknown-linux-gnu" => "linux-x64",
        "aarch64-unknown-linux-gnu" => "linux-arm64",
        "x86_64-unknown-linux-musl" => "linux-x64-musl",
        "aarch64-unknown-linux-musl" => "linux-arm64-musl",
        "x86_64-apple-darwin" => "mac-x64",
        "aarch64-apple-darwin" => "mac-arm64",
        // No alias for this host: pass the triple through unchanged. The
        // rest of the assertions still hold.
        other => other,
    };
    (alias.to_string(), triple)
}

/// Independent restatement of the tag policy — deliberately not a call into
/// `wheel_cmd::compatibility_for_target`, so the test can disagree with the
/// implementation instead of echoing it.
///
/// Every case in this file builds for the *host*, and a host build gets no
/// target preparation (`soldr_main.rs` gates it on `target != host`), so soldr
/// has enforced no floor and may claim none. `pypi` is maturin's
/// derive-the-tag-from-the-bytes pseudo-option.
fn expected_compatibility(_triple: &str) -> &'static str {
    "pypi"
}

#[test]
fn soldr_wheel_resolves_the_alias_and_tags_the_wheel() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("soldr-wheel-argv");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    seed_cached_fake_maturin(&cache_root, &log_path);
    let (alias, triple) = host_alias_and_triple();

    let output = isolated_soldr_command()
        .args(["wheel", "--release", "--target", &alias])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("SOLDR_RUSTC_WRAPPER")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr wheel");

    assert!(
        output.status.success(),
        "soldr wheel failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&log_path).expect("read fake tool log");
    let argv = maturin_argv(&log);
    assert_eq!(argv.first().map(String::as_str), Some("build"), "{log}");
    assert!(
        argv.iter().any(|arg| arg == "--release"),
        "`--release` must reach maturin when the caller asked for it: {log}"
    );
    assert_eq!(
        flag_value(&argv, "--target").as_deref(),
        Some(triple.as_str()),
        "the friendly alias must reach maturin as a rustc-legal triple: {log}"
    );
    assert_eq!(
        flag_value(&argv, "--compatibility").as_deref(),
        Some(expected_compatibility(&triple)),
        "wheel tag must follow the target family: {log}"
    );
}

#[test]
fn soldr_wheel_forwards_extra_arguments_to_maturin() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("soldr-wheel-passthrough");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    seed_cached_fake_maturin(&cache_root, &log_path);
    let (alias, _) = host_alias_and_triple();

    let output = isolated_soldr_command()
        .args(["wheel", "--release", "--target", &alias, "--out", "dist"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("SOLDR_RUSTC_WRAPPER")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr wheel");

    assert!(
        output.status.success(),
        "soldr wheel failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&log_path).expect("read fake tool log");
    let argv = maturin_argv(&log);
    assert_eq!(
        flag_value(&argv, "--out").as_deref(),
        Some("dist"),
        "passthrough arguments must reach maturin: {log}"
    );
}

// soldr#2139 follow-up. Two properties in one run, because they share the
// same fixture: a bare `soldr wheel` is legal (host target, dev profile), and
// it must not claim a manylinux floor that no target preparation enforced.
// (Plain comment, not `///`: a doc comment on a macro invocation attaches to
// nothing and `-D unused-doc-comments` rejects it.)
#[test]
fn soldr_wheel_defaults_to_a_dev_host_wheel_with_no_floor_claim() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("soldr-wheel-default");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    seed_cached_fake_maturin(&cache_root, &log_path);

    let output = isolated_soldr_command()
        .args(["wheel"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("SOLDR_RUSTC_WRAPPER")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr wheel");

    assert!(
        output.status.success(),
        "bare `soldr wheel` must build a host wheel
stdout:
{}
stderr:
{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&log_path).expect("read fake tool log");
    let argv = maturin_argv(&log);
    assert!(
        !argv.iter().any(|arg| arg == "--release"),
        "the default wheel is a quick dev build: {log}"
    );
    assert_eq!(
        flag_value(&argv, "--target").as_deref(),
        Some(soldr_cli::pyo3_detect::host_triple()),
        "--target defaults to the host: {log}"
    );
    assert_eq!(
        flag_value(&argv, "--compatibility").as_deref(),
        Some("pypi"),
        "a dev host wheel must not claim a manylinux floor: {log}"
    );
}

#[test]
fn soldr_wheel_rejects_an_unknown_target_with_a_suggestion() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("soldr-wheel-unknown-target");
    let log_path = cache_root.join("tool.log");
    seed_cached_fake_maturin(&cache_root, &log_path);

    let output = isolated_soldr_command()
        .args(["wheel", "--release", "--target", "linux-arm65"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr wheel");

    assert!(!output.status.success(), "unknown target must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("linux-arm64"), "stderr:\n{stderr}");
    assert!(
        !log_path.exists(),
        "maturin must not be spawned for an unresolvable target"
    );
}
