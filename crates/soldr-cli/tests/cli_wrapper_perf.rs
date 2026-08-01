//! Regression coverage for issue #474 — wrapper hot-path routing.
//!
//! Asserts the Option A invariant: when `SOLDR_BUILD_SESSION_ID` is
//! NOT set in the environment, `record_target_dir_in_registry` writes
//! the target row to redb directly and skips the daemon entirely (no
//! socket connect, no PID-file probe, no spawn attempt). When it IS
//! set, the function takes the daemon path.
//!
//! Also pins a coarse latency budget for the fast path so an
//! accidental re-introduction of per-invocation IPC or fs walks
//! shows up as a test failure rather than a silent perf regression.

#![allow(clippy::print_stdout)]

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR;
use soldr_cli::compile_dispatch::build_compile_request;
use soldr_cli::wrapper_target::{
    record_target_dir_in_registry, TargetTouchPath, TARGET_REGISTRY_RECORDED_ENV_VAR,
};

/// Cross-test env lock: every test in this file mutates the same
/// per-process env vars (`SOLDR_CACHE_DIR`, `HOME`, `USERPROFILE`,
/// `SOLDR_BUILD_SESSION_ID`). Cargo runs integration tests in
/// parallel by default — without serialization they'd clobber each
/// other's `read_build_session_id_env` reads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

struct EnvScope {
    keys: Vec<&'static str>,
    prior: Vec<Option<OsString>>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl EnvScope {
    fn set(pairs: &[(&'static str, &Path)]) -> Self {
        Self::set_strs(
            &pairs
                .iter()
                .map(|(k, v)| (*k, v.as_os_str().to_os_string()))
                .collect::<Vec<_>>(),
        )
    }

    fn set_strs(pairs: &[(&'static str, OsString)]) -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut prior = Vec::with_capacity(pairs.len());
        let mut keys = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            prior.push(std::env::var_os(k));
            std::env::set_var(k, v);
            keys.push(*k);
        }
        for key in [
            SOLDR_BUILD_SESSION_ID_ENV_VAR,
            TARGET_REGISTRY_RECORDED_ENV_VAR,
            "CARGO_TARGET_DIR",
        ] {
            if !keys.contains(&key) {
                prior.push(std::env::var_os(key));
                std::env::remove_var(key);
                keys.push(key);
            }
        }
        Self {
            keys,
            prior,
            _guard: guard,
        }
    }

    fn add(mut self, key: &'static str, value: &str) -> Self {
        if !self.keys.contains(&key) {
            self.prior.push(std::env::var_os(key));
            self.keys.push(key);
        }
        std::env::set_var(key, value);
        self
    }

    fn remove(mut self, key: &'static str) -> Self {
        if !self.keys.contains(&key) {
            self.prior.push(std::env::var_os(key));
            self.keys.push(key);
        }
        std::env::remove_var(key);
        self
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (k, p) in self.keys.iter().zip(self.prior.iter()) {
            match p {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

fn rustc_args_for(target_root: &Path, crate_name: &str) -> Vec<String> {
    let target = target_root.join("target");
    std::fs::create_dir_all(&target).expect("seed target dir");
    vec![
        "--crate-name".to_string(),
        crate_name.to_string(),
        "--out-dir".to_string(),
        target.join("debug").join("deps").display().to_string(),
        "--emit".to_string(),
        "dep-info,link".to_string(),
    ]
}

#[test]
fn session_compile_request_carries_lifecycle_on_compile_connection() {
    let cache_root = unique_temp_dir("compile-lifecycle-cache");
    let home_root = unique_temp_dir("compile-lifecycle-home");
    let workspace = unique_temp_dir("compile-lifecycle-workspace");
    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ])
    .add(SOLDR_BUILD_SESSION_ID_ENV_VAR, "4242");

    let mut argv = vec!["/toolchain/bin/rustc".to_string()];
    argv.extend(rustc_args_for(&workspace, "demo_crate"));
    let request = build_compile_request(&argv);
    let lifecycle = request.lifecycle.expect("session lifecycle metadata");
    assert_eq!(lifecycle.session_id, 4242);
    assert_eq!(lifecycle.crate_name, "demo_crate");
    assert!(lifecycle.target_dir.ends_with("target"));
    assert!(lifecycle.started_at_ms > 0);
}

#[test]
fn standalone_compile_request_has_no_lifecycle_metadata() {
    let workspace = unique_temp_dir("compile-no-lifecycle-workspace");
    let _scope = EnvScope::set_strs(&[]).remove(SOLDR_BUILD_SESSION_ID_ENV_VAR);
    let mut argv = vec!["/toolchain/bin/rustc".to_string()];
    argv.extend(rustc_args_for(&workspace, "demo_crate"));
    assert!(build_compile_request(&argv).lifecycle.is_none());
}

#[test]
fn embedded_wrapper_path_has_no_standalone_compile_telemetry_calls() {
    let manifest = common::crate_root();
    for relative in ["src/wrapper.rs", "src/wrapper_target.rs"] {
        let source = std::fs::read_to_string(manifest.join(relative)).expect("read wrapper source");
        assert!(
            !source.contains("record_compile("),
            "{relative} must not reopen daemon IPC for standalone compile telemetry; \
             lifecycle metadata belongs on CompileRequest"
        );
        assert!(
            !source.contains("record_compile_end_for_wrapper"),
            "{relative} must not restore the post-compile telemetry connection"
        );
    }
}

#[test]
fn fast_path_when_no_session_id() {
    let cache_root = unique_temp_dir("perf-fast-cache");
    let home_root = unique_temp_dir("perf-fast-home");
    let workspace = unique_temp_dir("perf-fast-workspace");
    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ])
    .remove(SOLDR_BUILD_SESSION_ID_ENV_VAR);

    let args = rustc_args_for(&workspace, "demo_crate");
    let path = record_target_dir_in_registry(&args);
    assert_eq!(
        path,
        TargetTouchPath::FastDirect,
        "without SOLDR_BUILD_SESSION_ID the wrapper must take the fast direct-redb path",
    );

    // Hard latency budget for the fast path. The function does:
    //   1. resolve_workspace_target_dir (string parse)
    //   2. SoldrPaths::new (env reads)
    //   3. read_build_session_id_env (env read, returns None)
    //   4. TargetRegistry::open (redb open, ~ms)
    //   5. registry.upsert (single write txn)
    // Empirically <2 ms on Linux dev boxes; ~5-20 ms on shared GHA
    // runners. The original 50 ms ceiling tripped a flaky failure on
    // Windows x86_64 runners under contention (#692 follow-up). The
    // budget's purpose is to catch an *order-of-magnitude* regression
    // (accidental daemon IPC, socket probe, or fs walk) — 250 ms is
    // still ~10x the worst CI observation but well under any of the
    // "regression smells" the budget exists to detect.
    // Best of 5, not the mean of 5. The budget exists to catch an
    // order-of-magnitude regression -- accidental daemon IPC, a socket probe,
    // an fs walk -- and every one of those makes *each* call slow, so the
    // fastest call still exposes them. A mean does not survive one scheduler
    // stall on a shared runner: a single 10s hiccup drags the average past any
    // budget while the other four calls were fine, which is what has been
    // failing here.
    //
    // This is the alternative the comment below asks for. The budget has been
    // raised three times (50ms -> 250ms -> 1s -> 2s), each after a flake, and
    // it failed again on a Windows target-run lane. Raising it a fourth time
    // would keep trading away the coverage the number is for.
    let fastest = (0..5)
        .map(|_| {
            let started = Instant::now();
            let _ = record_target_dir_in_registry(&args);
            started.elapsed()
        })
        .min()
        .expect("five samples");
    let avg = fastest;
    // Budget was 250ms; raised to 1s in #1139 after GHA
    // aarch64-linux-gnu (via `target-run` on ubuntu-24.04-arm)
    // averaged ~500ms in run 28492… ; raised to 2s in #1311 after
    // aarch64-unknown-linux-musl target-run averaged ~1.25s (redb
    // write per call × 5 calls; musl's per-syscall cost on aarch64
    // is materially higher than glibc's). The budget's purpose is
    // to catch order-of-magnitude regressions (accidental daemon
    // IPC, socket probe, or fs walk) — 2s is still ~100x the worst
    // pre-slowdown observation. If this needs to grow AGAIN,
    // investigate whether the fast path itself regressed rather
    // than raising a fourth time.
    assert!(
        avg < Duration::from_millis(2000),
        "fast path best-of-5 = {avg:?} exceeds 2s budget — accidental daemon IPC, socket probe, or fs walk?",
    );
}

#[test]
fn slow_path_when_session_id_set() {
    let cache_root = unique_temp_dir("perf-slow-cache");
    let home_root = unique_temp_dir("perf-slow-home");
    let workspace = unique_temp_dir("perf-slow-workspace");
    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ])
    .add(SOLDR_BUILD_SESSION_ID_ENV_VAR, "1234567890123456789");

    let args = rustc_args_for(&workspace, "demo_crate");
    let path = record_target_dir_in_registry(&args);
    assert_eq!(
        path,
        TargetTouchPath::DaemonFirst,
        "with SOLDR_BUILD_SESSION_ID set the wrapper must take the daemon-first path",
    );
}

#[test]
fn fast_path_writes_target_registry_row_directly() {
    use soldr_cli::cache_lib::target_registry::TargetRegistry;

    let cache_root = unique_temp_dir("perf-fast-write-cache");
    let home_root = unique_temp_dir("perf-fast-write-home");
    let workspace = unique_temp_dir("perf-fast-write-workspace");
    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ])
    .remove(SOLDR_BUILD_SESSION_ID_ENV_VAR);

    let args = rustc_args_for(&workspace, "demo_crate");
    let expected_target = workspace.join("target");
    assert!(expected_target.exists(), "test seeded target dir");

    let path = record_target_dir_in_registry(&args);
    assert_eq!(path, TargetTouchPath::FastDirect);

    // Verify exactly one row landed in redb directly, NOT via a
    // daemon round trip (there is no daemon running in this test).
    // We check the row count + that the recorded path normalizes to
    // the same target dir we seeded; resolve_workspace_target_dir
    // may canonicalize the path so an exact-match lookup is brittle.
    let registry = TargetRegistry::open(&cache_root.join("state.redb")).expect("open registry");
    let rows = registry.list().expect("list rows");
    assert_eq!(
        rows.len(),
        1,
        "fast path must populate exactly one target row; got: {rows:?}",
    );
    let row = &rows[0];
    assert!(
        row.path.ends_with("target"),
        "registered path should end in `target`; got {:?}",
        row.path,
    );
    let canonical_expected = std::fs::canonicalize(&expected_target).unwrap_or(expected_target);
    let canonical_actual = std::fs::canonicalize(&row.path).unwrap_or_else(|_| row.path.clone());
    assert_eq!(
        canonical_actual, canonical_expected,
        "registered path must point at the seeded target dir",
    );
    assert!(
        row.last_used > 0,
        "fast path must stamp `last_used` with a current unix timestamp",
    );
}

#[test]
fn memo_path_skips_redb_when_env_matches_resolved_target() {
    // Issue #440: when the cargo front door has already recorded the
    // target dir for the build session (signalled via
    // SOLDR_TARGET_REGISTRY_RECORDED), the wrapper must return
    // MemoSkipped and NOT touch redb. Verifies the registry stays
    // empty after a memo hit so we know the redb write was actually
    // skipped, not just no-op'd.
    use soldr_cli::cache_lib::target_registry::TargetRegistry;

    let cache_root = unique_temp_dir("perf-memo-cache");
    let home_root = unique_temp_dir("perf-memo-home");
    let workspace = unique_temp_dir("perf-memo-workspace");
    // Seed the target dir so the resolver's canonicalization
    // succeeds, but DO NOT prepopulate the registry — we want to
    // prove the memo path stays out of redb.
    let target = workspace.join("target");
    std::fs::create_dir_all(&target).expect("seed target");
    let canon = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());

    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ])
    .remove(SOLDR_BUILD_SESSION_ID_ENV_VAR)
    .add(
        TARGET_REGISTRY_RECORDED_ENV_VAR,
        canon.to_string_lossy().as_ref(),
    );

    let args = rustc_args_for(&workspace, "demo_crate");
    let path = record_target_dir_in_registry(&args);
    assert_eq!(
        path,
        TargetTouchPath::MemoSkipped,
        "matching memo env var must short-circuit the redb write",
    );

    // Critical: the registry must be empty because the memo path
    // skipped both the direct redb write and the daemon target-touch
    // IPC. The cargo front door is responsible for the one-time
    // upsert in production; the test simulates that by not touching
    // the registry itself.
    let registry_path = cache_root.join("state.redb");
    if registry_path.exists() {
        let registry = TargetRegistry::open(&registry_path).expect("open registry");
        let rows = registry.list().expect("list rows");
        assert!(
            rows.is_empty(),
            "memo path must NOT write to redb; got rows: {rows:?}",
        );
    }
}

#[test]
fn memo_path_falls_through_when_env_path_does_not_match() {
    // Defensive: if SOLDR_TARGET_REGISTRY_RECORDED points at a
    // different dir than the wrapper-resolved target, the memo must
    // NOT short-circuit. Falling through preserves correctness when
    // the env var was leaked across worktrees (e.g. nested cargo).
    let cache_root = unique_temp_dir("perf-memo-mismatch-cache");
    let home_root = unique_temp_dir("perf-memo-mismatch-home");
    let workspace = unique_temp_dir("perf-memo-mismatch-workspace");
    let unrelated = unique_temp_dir("perf-memo-mismatch-other");
    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ])
    .remove(SOLDR_BUILD_SESSION_ID_ENV_VAR)
    .add(
        TARGET_REGISTRY_RECORDED_ENV_VAR,
        unrelated.to_string_lossy().as_ref(),
    );

    let args = rustc_args_for(&workspace, "demo_crate");
    let path = record_target_dir_in_registry(&args);
    assert_eq!(
        path,
        TargetTouchPath::FastDirect,
        "memo env pointing at an unrelated dir must NOT short-circuit",
    );
}

#[test]
fn no_target_path_when_args_lack_workspace_target() {
    let cache_root = unique_temp_dir("perf-no-target-cache");
    let home_root = unique_temp_dir("perf-no-target-home");
    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ])
    .remove(SOLDR_BUILD_SESSION_ID_ENV_VAR);

    // No --out-dir → can't resolve a workspace target.
    let args = vec!["--crate-name".to_string(), "demo_crate".to_string()];
    let path = record_target_dir_in_registry(&args);
    assert_eq!(
        path,
        TargetTouchPath::NoTarget,
        "without a resolvable workspace target the wrapper must short-circuit before opening redb",
    );
}
