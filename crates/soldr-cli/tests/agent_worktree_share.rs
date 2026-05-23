#![allow(unused_imports)]
// ^ common/mod.rs has stale imports (Write, Duration, Instant) that fire
//   when a test binary only uses a subset of helpers. Other tests in this
//   directory use the same crate-level allow.

//! Regression test for soldr's parent-cache-sharing mechanism
//! (issue #352, Tier L1.x).
//!
//! Two `git worktree add` worktrees of the same repo — living at
//! DIFFERENT absolute paths under DIFFERENT agent namespaces
//! (`.claude/worktrees/branch-a` vs `.codex/worktrees/branch-b`) —
//! share zccache hits via `ZCCACHE_PATH_REMAP=auto` when they point
//! at the same `SOLDR_CACHE_DIR`. The namespace prefix is irrelevant
//! to the mechanism; this test locks that property in place.
//!
//! Empirical baseline on a Windows host with a `serde + serde_json`
//! fixture (12 cacheable compilations): cold worktree → 0 hits /
//! 12 misses (hit_rate 0.0); warm worktree → 12 hits / 0 misses
//! (hit_rate 1.0). Cold-build wall-clock ≈ 14 s, warm ≈ 5 s.
//!
//! Gated `#[ignore]` because the first run fetches the managed
//! zccache binary (~5–10 s network) and the two cargo builds add
//! ~20 s of compile. Run explicitly with:
//!
//! ```text
//! soldr cargo test -p soldr-cli --test agent_worktree_share -- --ignored --nocapture
//! ```
//!
//! Sanity-check the test's failure mode by reverting the
//! `cargo.env(... ZCCACHE_PATH_REMAP, "auto")` call at
//! `crates/soldr-cli/src/zccache.rs:226-235` locally — the warm-build
//! assertion `hit_rate >= 0.99` should then fail loudly.

mod common;

use common::unique_temp_dir;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
#[ignore = "fetches zccache + does two real cargo builds; run with --ignored"]
fn worktree_share_across_agent_namespaces() {
    let workdir = unique_temp_dir("agent-worktree-share");
    let cache_dir = workdir.join("shared-cache");
    let crate_dir = workdir.join("test-crate");

    fs::create_dir_all(&cache_dir).expect("create cache dir");
    create_test_crate(&crate_dir);

    // Path-remap's auto-detect requires a real `.git/` checkout — see
    // CLAUDE.md ("Requires a real .git/ checkout — tarball/zip
    // checkouts silently fall back to no remap.").
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

    // Two worktrees of the same `.git/` under DIFFERENT agent
    // namespaces. The mechanism is supposed to ignore the prefix —
    // that's what we're asserting.
    let claude_worktree = crate_dir.join(".claude/worktrees/branch-a");
    let codex_worktree = crate_dir.join(".codex/worktrees/branch-b");
    git(
        &[
            "worktree",
            "add",
            "-q",
            claude_worktree
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
            codex_worktree
                .to_str()
                .expect("worktree path must be utf-8"),
            "HEAD",
        ],
        &crate_dir,
    );

    // --- Cold build in .claude/worktrees/branch-a -------------------
    soldr_cargo_build(&claude_worktree, &cache_dir);

    // session-stats.json is overwritten by the next session, so
    // snapshot it before the warm build runs.
    let stats_path = session_stats_path(&cache_dir);
    let cold_snapshot = workdir.join("stats-cold.json");
    fs::copy(&stats_path, &cold_snapshot)
        .unwrap_or_else(|e| panic!("snapshot cold stats from {}: {e}", stats_path.display()));
    let cold = read_json(&cold_snapshot);

    // --- Warm build in .codex/worktrees/branch-b --------------------
    soldr_cargo_build(&codex_worktree, &cache_dir);
    let warm = read_json(&stats_path);

    // --- Assertions -------------------------------------------------
    let cold_hits = u64_field(&cold, "hits");
    let cold_misses = u64_field(&cold, "misses");
    let cold_comp = u64_field(&cold, "compilations");
    assert_eq!(
        cold_hits, 0,
        "cold worktree should have zero hits (cache empty); cold={cold:#?}",
    );
    assert!(
        cold_misses > 0,
        "cold worktree should have at least one cacheable miss; cold={cold:#?}",
    );

    let warm_hits = u64_field(&warm, "hits");
    let warm_misses = u64_field(&warm, "misses");
    let warm_comp = u64_field(&warm, "compilations");
    let warm_hit_rate = warm
        .get("hit_rate")
        .and_then(|v| v.as_f64())
        .expect("warm session stats must include hit_rate");

    assert!(
        warm_hits > 0,
        "warm worktree (different absolute path, different agent namespace) \
         must hit cache via path-remap; got warm={warm:#?}",
    );
    assert_eq!(
        warm_misses, 0,
        "warm worktree must have zero misses; got warm={warm:#?}",
    );
    assert!(
        warm_hit_rate >= 0.99,
        "warm worktree hit_rate must be >= 0.99; got {warm_hit_rate} \
         (warm={warm:#?})",
    );

    // Invariant: every cold miss became a warm hit (same compile set,
    // path-remap normalized both worktrees' paths to the same keys).
    assert_eq!(
        cold_misses, warm_hits,
        "warm hits should equal cold misses (every miss became a hit); \
         cold_misses={cold_misses}, warm_hits={warm_hits}; \
         cold={cold:#?}; warm={warm:#?}",
    );
    assert_eq!(
        cold_comp, warm_comp,
        "both sessions should compile the same set; cold={cold_comp}, \
         warm={warm_comp}",
    );
}

// ─────────────────────────────────────────────────────── helpers

fn create_test_crate(dir: &Path) {
    fs::create_dir_all(dir.join("src")).expect("create src/");
    fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "agent_share_test"
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
struct Foo { x: i32 }

fn main() {
    let f = Foo { x: 42 };
    println!("{}", serde_json::to_string(&f).unwrap());
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

fn soldr_cargo_build(worktree: &Path, cache_dir: &Path) {
    let zccache_dir = cache_dir.join("cache").join("zccache");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .current_dir(worktree)
        .env("SOLDR_CACHE_DIR", cache_dir)
        .env("ZCCACHE_CACHE_DIR", &zccache_dir)
        .output()
        .expect("spawn soldr cargo build");
    assert!(
        output.status.success(),
        "soldr cargo build failed in {}: stdout={}; stderr={}",
        worktree.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn session_stats_path(cache_dir: &Path) -> PathBuf {
    cache_dir
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session-stats.json")
}

fn read_json(path: &Path) -> Value {
    let raw = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("parse {}: {e}\n{raw}", path.display()))
}

fn u64_field(stats: &Value, key: &str) -> u64 {
    stats
        .get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("missing or non-u64 `{key}` in {stats:#?}"))
}
