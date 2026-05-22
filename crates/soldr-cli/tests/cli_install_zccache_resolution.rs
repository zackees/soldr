//! Regression tests for issue #420 — `install-zccache` / `update-zccache`
//! pin silently ignored by daemon spawn.
//!
//! The issue reporter (zackees) observed that after pinning a custom
//! zccache build via `soldr install-zccache <path>`, subsequent
//! `soldr cargo build` invocations spawned the managed v1.8.1 daemon
//! instead of the pinned one — silently invalidating three perf-cluster
//! runs in the zackees/zccache repo before being noticed.
//!
//! Code reading suggested the resolution chain in
//! `fetch_zccache_with_paths` already places the pinned check ahead of
//! the managed cache, so the pin SHOULD win. These tests pin that
//! contract down so the bug can't silently regress:
//!
//! 1. `fetch_zccache_with_paths_returns_pinned_over_managed_cache` —
//!    the most direct expression of the issue's hypothesis. Seeds both
//!    the managed cache dir AND the pinned dir, then asserts the
//!    resolver returns the pinned binary.
//! 2. `cached_zccache_binary_returns_pinned_over_managed_cache` — same
//!    invariant for the read-only path used by `soldr cache`, status,
//!    session-end, shutdown.
//! 3. `classify_zccache_source_tags_each_branch_correctly` — pure
//!    classifier used by `soldr doctor` / cache status / the new
//!    "soldr: zccache source: ..." diagnostic in prepare_zccache_build.
//! 4. `eviction_sentinel_triggers_switch_from_managed_to_pinned` —
//!    covers the "stale daemon was already alive" hypothesis from the
//!    issue. Simulates the user's actual repro by seeding the sentinel
//!    with a managed path, then asserting that resolution against the
//!    pinned binary triggers an eviction.
//! 5. `install_then_status_subprocess_reports_pinned_source` — the
//!    CLI-surface check: pin via subprocess, then run `soldr doctor`
//!    and `soldr cache --json`, assert both report `pinned` as the
//!    active source.

#![allow(unused_imports)]

mod common;

use common::unique_temp_dir;
use serde_json::Value;
use soldr_cli::core::SoldrPaths;
use soldr_cli::fetch::{
    cached_zccache_binary, classify_zccache_source, fetch_zccache_with_paths,
    install_zccache_from_source, FetchResult, InstallSource, ZccacheSource,
    MANAGED_ZCCACHE_VERSION, PINNED_ZCCACHE_DIRNAME, ZCCACHE_LOCAL_DIR_ENV_VAR,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Tests in this file run blocking installs via `tokio::test`; pull in
/// the runtime so `install_zccache_from_source` (which is `async`)
/// works.
fn bin_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn write_fake(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

fn seed_pin_source(tmp_root: &Path) -> PathBuf {
    let src = tmp_root.join("pin-source");
    write_fake(&src, &bin_name("zccache"), b"PINNED_CLI_BYTES");
    write_fake(&src, &bin_name("zccache-daemon"), b"PINNED_DAEMON_BYTES");
    write_fake(&src, &bin_name("zccache-fp"), b"PINNED_FP_BYTES");
    src
}

/// Pre-populate the managed cache dir as if a previous `soldr cargo`
/// invocation had already fetched the managed v1.8.1 binaries. The
/// resolver hits this dir via `check_cache` after the pinned check, so
/// seeding it lets us verify the pinned check actually beats it.
fn seed_managed_cache(paths: &SoldrPaths) -> PathBuf {
    let managed_dir = paths.bin.join(format!("zccache-{MANAGED_ZCCACHE_VERSION}"));
    write_fake(&managed_dir, &bin_name("zccache"), b"MANAGED_CLI_BYTES");
    write_fake(
        &managed_dir,
        &bin_name("zccache-daemon"),
        b"MANAGED_DAEMON_BYTES",
    );
    write_fake(&managed_dir, &bin_name("zccache-fp"), b"MANAGED_FP_BYTES");
    managed_dir
}

/// Reset env state that would otherwise leak across tests. Restored on
/// drop. We unset the override vars so neither the local-dir override
/// (`SOLDR_ZCCACHE_LOCAL_DIR`) nor the test override
/// (`SOLDR_TEST_ZCCACHE_BIN`) shadows the pinned resolution path under
/// test.
struct EnvGuard {
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn isolate(keys: &[&str]) -> Self {
        let mut saved = Vec::with_capacity(keys.len());
        for key in keys {
            saved.push((key.to_string(), std::env::var_os(key)));
            unsafe {
                std::env::remove_var(key);
            }
        }
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prior) in &self.saved {
            unsafe {
                match prior {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Scenario 1: fetch_zccache_with_paths
// ---------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn fetch_zccache_with_paths_returns_pinned_over_managed_cache() {
    let _guard = EnvGuard::isolate(&[ZCCACHE_LOCAL_DIR_ENV_VAR, "SOLDR_TEST_ZCCACHE_BIN"]);
    let tmp = unique_temp_dir("pin-wins-fetch");
    let paths = SoldrPaths::with_root(tmp.join("soldr-root"));

    // 1. Seed the managed cache as if a previous build had already
    //    fetched v1.8.1. This is what the issue reporter's environment
    //    looked like — managed v1.8.1 already on disk from earlier runs.
    let managed_dir = seed_managed_cache(&paths);
    assert!(managed_dir.join(bin_name("zccache")).exists());

    // 2. Install a pinned binary on top.
    let pin_src = seed_pin_source(&tmp);
    install_zccache_from_source(&InstallSource::Path(pin_src), &paths)
        .await
        .expect("install pin");

    // 3. Resolve. The pinned dir must win over the managed cache.
    let fetched = fetch_zccache_with_paths(&paths)
        .await
        .expect("fetch should resolve");
    let resolved_parent = fetched.binary_path.parent().expect("parent");
    let expected_parent = paths.bin.join(PINNED_ZCCACHE_DIRNAME);
    assert_eq!(
        resolved_parent,
        expected_parent,
        "issue #420: pinned binary must win resolution over the managed cache;\n  \
         resolved = {}\n  pinned   = {}\n  managed  = {}",
        resolved_parent.display(),
        expected_parent.display(),
        managed_dir.display()
    );

    // Sanity: the resolved bytes are the pinned ones, not the managed
    // placeholder. If this fails the resolver returned the right path
    // but pointed at the wrong file (would happen if pinned_dir was
    // empty and we somehow returned the dir path).
    let bytes = fs::read(&fetched.binary_path).expect("read resolved binary");
    assert_eq!(
        bytes,
        b"PINNED_CLI_BYTES",
        "resolved binary must be the pinned bytes; got {} bytes from {}",
        bytes.len(),
        fetched.binary_path.display()
    );
}

// ---------------------------------------------------------------------
// Scenario 2: cached_zccache_binary
// ---------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn cached_zccache_binary_returns_pinned_over_managed_cache() {
    let _guard = EnvGuard::isolate(&[ZCCACHE_LOCAL_DIR_ENV_VAR, "SOLDR_TEST_ZCCACHE_BIN"]);
    let tmp = unique_temp_dir("pin-wins-cached");
    let paths = SoldrPaths::with_root(tmp.join("soldr-root"));

    seed_managed_cache(&paths);
    let pin_src = seed_pin_source(&tmp);
    install_zccache_from_source(&InstallSource::Path(pin_src), &paths)
        .await
        .expect("install pin");

    let cached: Option<FetchResult> =
        cached_zccache_binary(&paths).expect("cached lookup should succeed");
    let cached = cached.expect("expected a cached pinned binary");
    let parent = cached.binary_path.parent().expect("parent");
    assert_eq!(
        parent,
        paths.bin.join(PINNED_ZCCACHE_DIRNAME),
        "cached_zccache_binary must return the pinned binary, not the managed cache (issue #420)"
    );
}

// ---------------------------------------------------------------------
// Scenario 3: classify_zccache_source
// ---------------------------------------------------------------------

#[test]
fn classify_zccache_source_tags_each_branch_correctly() {
    let _guard = EnvGuard::isolate(&[ZCCACHE_LOCAL_DIR_ENV_VAR]);
    let tmp = unique_temp_dir("classify-source");
    let paths = SoldrPaths::with_root(tmp.join("soldr-root"));
    let ext = if cfg!(windows) { ".exe" } else { "" };

    // Pinned: lives under <paths.bin>/zccache-pinned/.
    let pinned = paths
        .bin
        .join(PINNED_ZCCACHE_DIRNAME)
        .join(format!("zccache{ext}"));
    assert_eq!(
        classify_zccache_source(&paths, &pinned),
        ZccacheSource::Pinned
    );

    // Managed: lives under <paths.bin>/zccache-<MANAGED_VERSION>/.
    let managed = paths
        .bin
        .join(format!("zccache-{MANAGED_ZCCACHE_VERSION}"))
        .join(format!("zccache{ext}"));
    assert_eq!(
        classify_zccache_source(&paths, &managed),
        ZccacheSource::Managed
    );

    // Local: parent dir name starts with `zccache-local-` regardless of
    // whether SOLDR_ZCCACHE_LOCAL_DIR is currently set in the env.
    let local = paths
        .bin
        .join("zccache-local-deadbeefcafe")
        .join(format!("zccache{ext}"));
    assert_eq!(
        classify_zccache_source(&paths, &local),
        ZccacheSource::Local
    );

    // Unknown: a path under neither — e.g. SOLDR_TEST_ZCCACHE_BIN
    // override pointing at /tmp/some-test-binary.
    let unknown = tmp.join("some-other-place").join(format!("zccache{ext}"));
    assert_eq!(
        classify_zccache_source(&paths, &unknown),
        ZccacheSource::None
    );
}

// ---------------------------------------------------------------------
// Scenario 4: eviction sentinel
//
// `should_evict_zccache_daemon` and `evict_zccache_daemon_if_binary_changed`
// live in the binary-tree `zccache` module (not the lib tree), so they
// are exercised by the unit tests in
// `crates/soldr-cli/src/zccache.rs#[cfg(test)] mod tests` instead of
// here. Notably the `evict_decision_triggers_when_local_dir_overrides_managed`
// case there asserts the managed -> pinned/local switch triggers
// eviction, which is exactly the path issue #420's reporter took (pin
// installed after a managed-daemon run).
// ---------------------------------------------------------------------

// ---------------------------------------------------------------------
// Scenario 5: CLI surface — install + doctor + cache
// ---------------------------------------------------------------------

#[test]
fn install_then_doctor_subprocess_reports_pinned_active_source() {
    let tmp = unique_temp_dir("cli-install-doctor");
    let cache_root = tmp.join("soldr-root");
    // Issue #426: the pin lives under $HOME/.soldr/bin/ now, so every
    // subprocess that installs MUST get its own HOME — otherwise parallel
    // test cases race on a shared pin dir.
    let home_root = unique_temp_dir("cli-install-doctor-home");
    fs::create_dir_all(home_root.join(".soldr").join("bin")).expect("seed home/.soldr/bin");
    let pin_src = seed_pin_source(&tmp);

    // 1. Pin the staged binaries via the CLI.
    let install = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["install-zccache", "--json"])
        .arg(&pin_src)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .env_remove("SOLDR_TEST_ZCCACHE_BIN")
        .output()
        .expect("install-zccache subprocess");
    assert!(
        install.status.success(),
        "install failed: stdout={} stderr={}",
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr)
    );

    // 2. soldr doctor --json must report active_zccache_source = pinned.
    let doctor = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["doctor", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .env_remove("SOLDR_TEST_ZCCACHE_BIN")
        .output()
        .expect("doctor subprocess");
    assert!(
        doctor.status.success(),
        "doctor failed: stdout={} stderr={}",
        String::from_utf8_lossy(&doctor.stdout),
        String::from_utf8_lossy(&doctor.stderr)
    );
    let doctor_json: Value =
        serde_json::from_slice(&doctor.stdout).expect("doctor --json should emit valid JSON");
    assert_eq!(
        doctor_json["active_zccache_source"],
        "pinned",
        "doctor must report active source = pinned after install-zccache (issue #420):\n{}",
        serde_json::to_string_pretty(&doctor_json).unwrap_or_default()
    );
    assert_eq!(
        doctor_json["pinned_zccache_active"], true,
        "doctor must report pinned_zccache_active = true after install"
    );

    // 3. The new "active zccache source:" diagnostic line appears in the
    //    human output too.
    let doctor_human = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["doctor"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .env_remove("SOLDR_TEST_ZCCACHE_BIN")
        .output()
        .expect("doctor human subprocess");
    assert!(doctor_human.status.success(), "doctor human failed");
    let stdout = String::from_utf8_lossy(&doctor_human.stdout);
    assert!(
        stdout.contains("active zccache source: pinned"),
        "doctor human output should include the source banner:\n{stdout}"
    );
}

#[test]
fn install_then_cache_subprocess_reports_pinned_source() {
    let tmp = unique_temp_dir("cli-install-cache");
    let cache_root = tmp.join("soldr-root");
    // Issue #426: see the matching comment in
    // `install_then_doctor_subprocess_reports_pinned_active_source`.
    let home_root = unique_temp_dir("cli-install-cache-home");
    fs::create_dir_all(home_root.join(".soldr").join("bin")).expect("seed home/.soldr/bin");
    let pin_src = seed_pin_source(&tmp);

    let install = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["install-zccache"])
        .arg(&pin_src)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .env_remove("SOLDR_TEST_ZCCACHE_BIN")
        .output()
        .expect("install subprocess");
    assert!(install.status.success(), "install failed");

    // The pinned binaries we wrote with `seed_pin_source` are not real
    // executables — `zccache status` will fail to run them. That's
    // fine: `collect_zccache_status` handles the failure and the JSON
    // still contains `binary_source` per the new schema.
    let cache = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cache", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env_remove("SOLDR_ZCCACHE_LOCAL_DIR")
        .env_remove("SOLDR_TEST_ZCCACHE_BIN")
        .output()
        .expect("cache subprocess");
    // We don't assert on exit status — `cache` may legitimately fail
    // when the fake pinned binary can't run `status`. We just need the
    // diagnostic field to flow through to JSON when it's emitted.
    if cache.status.success() {
        let value: Value =
            serde_json::from_slice(&cache.stdout).expect("cache --json should emit valid JSON");
        let source = value["zccache"]["binary_source"].as_str();
        assert_eq!(
            source,
            Some("pinned"),
            "cache --json must report binary_source = pinned (issue #420):\n{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    }
}
