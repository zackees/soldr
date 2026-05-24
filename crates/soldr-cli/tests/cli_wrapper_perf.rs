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

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR;
use soldr_cli::wrapper_target::{record_target_dir_in_registry, TargetTouchPath};

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
        Self {
            keys,
            prior,
            _guard: guard,
        }
    }

    fn add(mut self, key: &'static str, value: &str) -> Self {
        self.prior.push(std::env::var_os(key));
        std::env::set_var(key, value);
        self.keys.push(key);
        self
    }

    fn remove(mut self, key: &'static str) -> Self {
        self.prior.push(std::env::var_os(key));
        std::env::remove_var(key);
        self.keys.push(key);
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
    // Empirically <2 ms on Linux + Windows in CI; 50 ms is the safety
    // margin so flakiness on a slow runner doesn't trip the test.
    let started = Instant::now();
    for _ in 0..5 {
        let _ = record_target_dir_in_registry(&args);
    }
    let avg = started.elapsed() / 5;
    assert!(
        avg < Duration::from_millis(50),
        "fast path avg = {avg:?} exceeds 50ms budget — accidental daemon IPC, socket probe, or fs walk?",
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
