#![cfg(windows)]

//! Windows regression for durable staged publication and parent-cache reuse.
//!
//! The embedded zccache store used to pass raw paths to `MoveFileExW` when it
//! committed the durable-digest sidecar.  A normal soldr cache root can put
//! that path beyond Windows' legacy MAX_PATH limit, so a successful compiler
//! invocation was salvaged to `target/` but never became a reusable cache
//! entry.  This test deliberately makes the staged sidecar path longer than
//! MAX_PATH, then proves a cold build publishes every miss and a separate
//! worktree/target consumes those entries.

mod common;

use common::unique_temp_dir;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

soldr_cli::timed_test!(
    windows_long_path_publication_survives_fresh_worktree_reuse,
    Duration::from_secs(300),
    {
        let workdir = unique_temp_dir("windows-cache-publication");
        let cache_dir = workdir.join("shared-cache");
        let crate_dir = workdir.join("test-crate");

        fs::create_dir_all(&cache_dir).expect("create cache dir");
        assert!(
            embedded_artifact_dir(&cache_dir).as_os_str().len() >= 81,
            "test cache root must be at least as deep as the production embedded store: {}",
            embedded_artifact_dir(&cache_dir).display(),
        );
        assert!(
            durable_digest_temp_path(&cache_dir).as_os_str().len() > 260,
            "test durable-digest sidecar must exceed MAX_PATH: {}",
            durable_digest_temp_path(&cache_dir).display(),
        );
        create_test_crate(&crate_dir);

        // A real `.git/` checkout makes this match the user-facing fresh
        // worktree case, and makes zccache's path-remap auto mode use the
        // common checkout root.
        git(&["init", "-q"], &crate_dir);
        git(&["add", "."], &crate_dir);
        git(
            &[
                "-c",
                "user.email=test@soldr.invalid",
                "-c",
                "user.name=test",
                "commit",
                "-q",
                "-m",
                "initial",
            ],
            &crate_dir,
        );

        let first_worktree = crate_dir.join(".claude/worktrees/cold");
        let second_worktree = crate_dir.join(".codex/worktrees/warm");
        git(
            &[
                "worktree",
                "add",
                "-q",
                first_worktree
                    .to_str()
                    .expect("worktree path must be utf-8"),
                "HEAD",
            ],
            &crate_dir,
        );
        git(
            &[
                "worktree",
                "add",
                "-q",
                second_worktree
                    .to_str()
                    .expect("worktree path must be utf-8"),
                "HEAD",
            ],
            &crate_dir,
        );

        soldr_cargo_check(&first_worktree, &cache_dir, &workdir.join("cold-target"));
        let cold = read_json(&latest_archived_session_stats(&cache_dir));
        let first_session_stats = archived_session_stats(&cache_dir);

        let cold_hits = u64_field(&cold, "hits");
        let cold_misses = u64_field(&cold, "misses");
        assert_eq!(cold_hits, 0, "cold build unexpectedly hit cache: {cold:#?}");
        assert!(
            cold_misses > 0,
            "cold build must contain cacheable misses: {cold:#?}"
        );
        assert_eq!(
            staged_counter(&cold, "publication_success"),
            cold_misses,
            "every cacheable cold miss must be durably published: {cold:#?}",
        );
        assert_eq!(
            staged_failure(&cold, "durable_digest"),
            0,
            "long-path durable digest publication must not fail: {cold:#?}",
        );

        soldr_cargo_check(&second_worktree, &cache_dir, &workdir.join("warm-target"));
        let warm = read_json(&new_archived_session_stats(
            &cache_dir,
            &first_session_stats,
        ));
        let warm_hits = u64_field(&warm, "hits");
        let warm_misses = u64_field(&warm, "misses");

        assert!(
            warm_hits > 0,
            "fresh worktree and target must reuse the cold build: {warm:#?}",
        );
        let warm_published = staged_counter(&warm, "publication_success");
        let warm_conflicts = staged_counter(&warm, "publication_conflict");
        // A few compiler outputs legitimately encode their target directory.
        // Those cannot be reused across fresh targets, but the staged store
        // must safely quarantine the candidate rather than replacing the
        // durable generation from the first worktree. Every other miss must
        // still publish successfully.
        assert_eq!(
            warm_published + warm_conflicts,
            warm_misses,
            "every fresh-target miss must publish or be safely quarantined as a conflict: {warm:#?}",
        );
        assert_eq!(
            staged_failure(&warm, "publication_conflict"),
            warm_conflicts,
            "each quarantined fresh-target miss must report its conflict: {warm:#?}",
        );
        assert_eq!(
            staged_failure(&warm, "durable_digest"),
            0,
            "fresh-target publication must not fail durable digest creation: {warm:#?}",
        );
    }
);

fn embedded_artifact_dir(cache_dir: &Path) -> PathBuf {
    cache_dir
        .join("cache")
        .join("zccache")
        .join("daemon-state")
        .join("embedded-v1")
        .join("artifacts")
}

fn durable_digest_temp_path(cache_dir: &Path) -> PathBuf {
    // Match the durable-digest publication shape: a staged artifact key,
    // a process-wide temporary generation, and a 64-hex cowhash sidecar.
    // This path is intentionally separate from rustc's short-lived staging
    // output paths, so the compiler/linker can still start normally.
    embedded_artifact_dir(cache_dir)
        .join(".staged-v2")
        .join("a".repeat(64))
        .join(".tmp-12345-123456789")
        .join(format!("..cowhash-{}.tmp-12345-123456789", "b".repeat(64)))
}

fn create_test_crate(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("create src/");
    fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "windows_cache_publication"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
"#,
    )
    .expect("write Cargo.toml");
    fs::write(
        dir.join("src").join("main.rs"),
        r#"use serde::Serialize;

#[derive(Serialize)]
struct Fixture { value: u32 }

fn main() {
    println!("{}", serde_json::to_string(&Fixture { value: 42 }).unwrap());
}
"#,
    )
    .expect("write src/main.rs");
}

fn git(args: &[&str], cwd: &Path) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed (cwd: {}): stderr={}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn soldr_cargo_check(worktree: &Path, cache_dir: &Path, target_dir: &Path) {
    let output = Command::new(common::soldr_bin())
        .args(["cargo", "check"])
        .current_dir(worktree)
        .env("SOLDR_CACHE_DIR", cache_dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .output()
        .expect("spawn soldr cargo check");
    assert!(
        output.status.success(),
        "soldr cargo check failed in {}: stdout={}; stderr={}",
        worktree.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn latest_archived_session_stats(cache_dir: &Path) -> PathBuf {
    archived_session_stats(cache_dir)
        .into_iter()
        .next()
        .unwrap_or_else(|| {
            panic!(
                "no archived session stats under {}",
                cache_dir
                    .join("cache")
                    .join("zccache")
                    .join("history")
                    .display()
            )
        })
}

fn new_archived_session_stats(cache_dir: &Path, previous: &[PathBuf]) -> PathBuf {
    archived_session_stats(cache_dir)
        .into_iter()
        .find(|path| !previous.contains(path))
        .unwrap_or_else(|| panic!("no newly archived session stats after warm build"))
}

fn archived_session_stats(cache_dir: &Path) -> Vec<PathBuf> {
    let history_dir = cache_dir.join("cache").join("zccache").join("history");
    let mut sessions: Vec<_> = fs::read_dir(&history_dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", history_dir.display()))
        .map(|entry| entry.expect("read history entry").path())
        .filter(|path| path.join("last-session-stats.json").is_file())
        .collect();
    sessions.sort();
    sessions
        .into_iter()
        .map(|session| session.join("last-session-stats.json"))
        .collect()
}

fn read_json(path: &Path) -> Value {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("parse {}: {e}\n{raw}", path.display()))
}

fn u64_field(stats: &Value, key: &str) -> u64 {
    stats
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing or non-u64 `{key}` in {stats:#?}"))
}

fn staged_counter(stats: &Value, key: &str) -> u64 {
    stats
        .pointer(&format!("/phase_profile/staged/counters/{key}"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing staged counter `{key}` in {stats:#?}"))
}

fn staged_failure(stats: &Value, key: &str) -> u64 {
    stats
        .pointer(&format!("/phase_profile/staged/failures/{key}"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing staged failure `{key}` in {stats:#?}"))
}
