//! Tests for the prepopulated-target restore guard (issue #480).
//!
//! Verifies that `should_skip_restore_due_to_prepopulated_target` fires
//! when cargo `.fingerprint/` dirs already exist under `target/` and stays
//! quiet on an empty or absent tree. Also covers the
//! `SOLDR_RUST_PLAN_FORCE_RESTORE` override that lets operators bypass the
//! guard for diagnostic purposes.

use crate::rust_plan::{
    count_populated_fingerprint_dirs, should_skip_restore_due_to_prepopulated_target,
    RustArtifactPlanContext, SOLDR_FORCE_RESTORE_ENV_VAR,
};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Mutex;
use tempfile::TempDir;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var(self.key, value);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

fn make_plan_ctx(target_dir: &std::path::Path) -> RustArtifactPlanContext {
    RustArtifactPlanContext {
        path: PathBuf::from("/tmp/plan.json"),
        cache_dir: PathBuf::from("/tmp/cache"),
        session_id: "session".to_string(),
        journal_path: PathBuf::from("/tmp/journal"),
        backend: "auto".to_string(),
        cache_profile: Some("thin-v2"),
        plan_inputs_hash: "hash".to_string(),
        target_dir: target_dir.display().to_string(),
    }
}

fn seed_fingerprint_dir(target: &std::path::Path, profile: &str) -> std::path::PathBuf {
    let fp = target.join(profile).join(".fingerprint");
    std::fs::create_dir_all(&fp).expect("create .fingerprint");
    let entry = fp.join("serde-abc123");
    std::fs::create_dir_all(&entry).expect("create fingerprint entry");
    std::fs::write(entry.join("invoked.timestamp"), b"").expect("seed invoked.timestamp");
    fp
}

#[test]
fn skip_reason_emitted_when_fingerprint_present() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::remove(SOLDR_FORCE_RESTORE_ENV_VAR);

    let target = TempDir::new().expect("tempdir");
    seed_fingerprint_dir(target.path(), "release");
    let ctx = make_plan_ctx(target.path());
    let reason = should_skip_restore_due_to_prepopulated_target(&ctx);
    assert!(reason.is_some(), "expected skip reason; got None");
    let msg = reason.unwrap();
    assert!(
        msg.contains("prepopulated"),
        "skip reason must mention prepopulated state: {msg}"
    );
    assert!(
        msg.contains(SOLDR_FORCE_RESTORE_ENV_VAR),
        "skip reason must mention the override env var: {msg}"
    );
}

#[test]
fn no_skip_when_target_absent() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::remove(SOLDR_FORCE_RESTORE_ENV_VAR);

    let ctx = make_plan_ctx(std::path::Path::new("/nonexistent/path/to/target"));
    let reason = should_skip_restore_due_to_prepopulated_target(&ctx);
    assert!(
        reason.is_none(),
        "expected no skip when target/ does not exist: {reason:?}"
    );
}

#[test]
fn no_skip_when_target_empty() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::remove(SOLDR_FORCE_RESTORE_ENV_VAR);

    let target = TempDir::new().expect("tempdir");
    let ctx = make_plan_ctx(target.path());
    let reason = should_skip_restore_due_to_prepopulated_target(&ctx);
    assert!(
        reason.is_none(),
        "expected no skip when target/ is empty: {reason:?}"
    );
}

#[test]
fn no_skip_when_fingerprint_dir_empty() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::remove(SOLDR_FORCE_RESTORE_ENV_VAR);

    let target = TempDir::new().expect("tempdir");
    let fp = target.path().join("release").join(".fingerprint");
    std::fs::create_dir_all(&fp).expect("create empty .fingerprint");
    let ctx = make_plan_ctx(target.path());
    let reason = should_skip_restore_due_to_prepopulated_target(&ctx);
    assert!(
        reason.is_none(),
        "empty .fingerprint/ must not trigger the guard: {reason:?}"
    );
}

#[test]
fn force_restore_env_var_overrides_guard() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::set(SOLDR_FORCE_RESTORE_ENV_VAR, "1");

    let target = TempDir::new().expect("tempdir");
    seed_fingerprint_dir(target.path(), "release");
    let ctx = make_plan_ctx(target.path());
    let reason = should_skip_restore_due_to_prepopulated_target(&ctx);
    assert!(
        reason.is_none(),
        "force-restore env var must bypass guard: {reason:?}"
    );
}

#[test]
fn force_restore_env_var_falsy_does_not_override() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::set(SOLDR_FORCE_RESTORE_ENV_VAR, "0");

    let target = TempDir::new().expect("tempdir");
    seed_fingerprint_dir(target.path(), "release");
    let ctx = make_plan_ctx(target.path());
    let reason = should_skip_restore_due_to_prepopulated_target(&ctx);
    assert!(
        reason.is_some(),
        "falsy SOLDR_RUST_PLAN_FORCE_RESTORE must not bypass guard"
    );
}

#[test]
fn walker_finds_fingerprint_under_explicit_triple_layout() {
    // `target/<triple>/<profile>/.fingerprint/` shape used by
    // `cargo build --target <triple>`.
    let target = TempDir::new().expect("tempdir");
    let fp = target
        .path()
        .join("x86_64-unknown-linux-gnu")
        .join("release")
        .join(".fingerprint");
    std::fs::create_dir_all(&fp).expect("create nested .fingerprint");
    std::fs::create_dir_all(fp.join("serde-abc")).expect("create entry");

    let count = count_populated_fingerprint_dirs(target.path(), 3);
    assert_eq!(count, 1, "walker should find triple-nested fingerprint");
}

#[test]
fn walker_counts_multiple_profile_fingerprints() {
    let target = TempDir::new().expect("tempdir");
    seed_fingerprint_dir(target.path(), "release");
    seed_fingerprint_dir(target.path(), "debug");

    let count = count_populated_fingerprint_dirs(target.path(), 3);
    assert_eq!(count, 2, "walker should count both profile fingerprints");
}
