//! Integration tests for PR 2 (#577): `soldr cook`'s cross-repo
//! shared-artifact indexing.
//!
//! These tests spawn the real `soldr-daemon` binary against a
//! sandboxed `~/.soldr/` and drive the PR-2 flow end-to-end via the
//! public packer (`cache_lib::cook_archive`) + client IPC helpers
//! (`daemon::client::cook_record` / `cook_lookup`).
//!
//! Docker harness gate: every test short-circuits unless
//! `SOLDR_COOK_DOCKER_HARNESS=1` is set. The supported runner is
//! `bench/cook_in_docker.sh`. Bare-host `cargo test` runs skip
//! everything so the host developer's soldr singleton stays
//! untouched (see meta #579 + `docs/CONTRIBUTING_COOK.md`).

#![allow(clippy::print_stdout)]

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::cache_lib::cook_archive::{
    artifact_path_for_sha, cook_cache_dir, pack_cook_archive,
};
use soldr_cli::core::git::{
    cargo_lock_is_gitignored, cargo_lock_is_tracked, find_git_worktree_root, normalize_origin_url,
    origin_url,
};
use soldr_cli::daemon::client::{self, cook_lookup, cook_record, CookLookupOutcome};

use crate::common;

const HARNESS_ENV: &str = "SOLDR_COOK_DOCKER_HARNESS";

fn harness_enabled() -> bool {
    matches!(
        std::env::var(HARNESS_ENV).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

fn skip_unless_in_container(test_name: &str) -> bool {
    if harness_enabled() {
        return false;
    }
    println!("{test_name}: skipped — {HARNESS_ENV} not set. Run via bench/cook_in_docker.sh.");
    true
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-cookwrites-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn write_file(p: &Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    let mut f = File::create(p).expect("create");
    f.write_all(bytes).expect("write");
}

fn soldr_daemon_bin() -> PathBuf {
    let soldr = common::soldr_bin();
    let parent = soldr.parent().expect("CARGO_BIN_EXE_soldr has a parent");
    let stem = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };

    parent.join(stem)
}

fn direct_sock(root: &Path) -> PathBuf {
    common::isolated_daemon::isolated_daemon_control_endpoint(&soldr_daemon_bin(), root)
}

struct DaemonProc {
    child: Option<Child>,
    cache_root: PathBuf,
}

impl DaemonProc {
    fn spawn(cache_root: &Path, home_root: &Path) -> Self {
        let mut cmd =
            common::isolated_daemon::isolated_daemon_command(&soldr_daemon_bin(), cache_root);
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn soldr-daemon");
        let deadline = Instant::now() + Duration::from_secs(40);
        let pid_path = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("broker-route-claim.pb");
        let sock = direct_sock(cache_root);
        while Instant::now() < deadline {
            if pid_path.exists() && client::status(&sock).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            pid_path.exists() && client::status(&sock).is_ok(),
            "soldr-daemon failed to become ready at {} within 40s",
            pid_path.display()
        );
        Self {
            child: Some(child),
            cache_root: cache_root.to_path_buf(),
        }
    }

    fn sock_path(&self) -> PathBuf {
        direct_sock(&self.cache_root)
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = client::shutdown(&direct_sock(&self.cache_root));
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Synthetic minimal `target/release/` tree mimicking what
/// `cargo chef cook` produces post-trim.
fn write_synthetic_target_release(root: &Path) {
    write_file(&root.join("deps").join("libfoo-abc.rlib"), b"rlib foo\n");
    write_file(&root.join("deps").join("libbar-def.rmeta"), b"rmeta bar\n");
    write_file(&root.join("deps").join("libbaz-ghi.rlib"), b"rlib baz\n");
}

fn run_git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// Initialize a real git repo with `Cargo.lock` committed and an
/// `origin` remote pointed at `origin_url`.
fn init_git_repo_with_origin(repo: &Path, origin: &str) {
    std::fs::create_dir_all(repo).unwrap();
    run_git_in(repo, &["init", "-q", "-b", "main"]);
    run_git_in(repo, &["config", "user.email", "cook@example.com"]);
    run_git_in(repo, &["config", "user.name", "cook test"]);
    run_git_in(repo, &["remote", "add", "origin", origin]);
    write_file(
        &repo.join("Cargo.toml"),
        b"[package]\nname='ci'\nversion='0'\n",
    );
    write_file(&repo.join("Cargo.lock"), b"# lockfile\n");
    run_git_in(repo, &["add", "Cargo.toml", "Cargo.lock"]);
    run_git_in(repo, &["commit", "-q", "-m", "init"]);
}

#[test]
fn end_to_end_pack_then_record_then_lookup_hits() {
    if skip_unless_in_container("end_to_end_pack_then_record_then_lookup_hits") {
        return;
    }
    let cache_root = unique_temp_dir("e2e-cache");
    let home_root = unique_temp_dir("e2e-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    // Pack a synthetic target/release tree into ~/.soldr/cache/cook/.
    let project_target = unique_temp_dir("e2e-project").join("target");
    let release_dir = project_target.join("release");
    write_synthetic_target_release(&release_dir);
    let paths = soldr_cli::core::SoldrPaths::with_root(cache_root.clone());
    let cook_dir = cook_cache_dir(&paths);
    std::fs::create_dir_all(&cook_dir).unwrap();
    let packed = pack_cook_archive(&release_dir, &cook_dir).expect("pack");
    assert_eq!(
        packed.path,
        artifact_path_for_sha(&cook_dir, &packed.sha256)
    );
    assert!(packed.path.is_file());

    // Register with the daemon.
    cook_record(
        &sock,
        [0xA1u8; 32],
        "x86_64-unknown-linux-gnu".to_string(),
        "release".to_string(),
        "1.94.1".to_string(),
        "rustc 1.94.1 (test)".to_string(),
        packed.sha256,
        packed.size_bytes,
        Some("https://github.com/zackees/soldr".to_string()),
        "cook --release".to_string(),
    )
    .expect("cook_record");

    // Subsequent lookup hits.
    let outcome = cook_lookup(
        &sock,
        [0xA1u8; 32],
        "x86_64-unknown-linux-gnu".to_string(),
        "release".to_string(),
        "1.94.1".to_string(),
        "rustc 1.94.1 (test)".to_string(),
        Some("https://github.com/zackees/soldr".to_string()),
    )
    .expect("cook_lookup");

    match outcome {
        CookLookupOutcome::Hit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            ..
        } => {
            assert_eq!(sha256, packed.sha256);
            assert_eq!(size_bytes, packed.size_bytes);
            assert_eq!(
                origin_url_normalized.as_deref(),
                Some("https://github.com/zackees/soldr"),
            );
            assert!(
                path.ends_with(".tar.zst"),
                "expected <sha256>.tar.zst path, got {path}"
            );
            // The daemon's reported path matches what the packer
            // produced.
            assert_eq!(PathBuf::from(&path), packed.path);
        }
        CookLookupOutcome::Miss { .. } => panic!("expected CookHit"),
    }

    // Daemon Status reflects the indexed row.
    let status = client::status(&sock).expect("status");
    let cook = status.cook_stats_or_zero();
    assert_eq!(cook.entries, 1);
    assert_eq!(cook.total_bytes, packed.size_bytes);
}

#[test]
fn origin_url_helper_extracts_and_normalizes() {
    if skip_unless_in_container("origin_url_helper_extracts_and_normalizes") {
        return;
    }
    let repo = unique_temp_dir("origin-repo");
    init_git_repo_with_origin(&repo, "https://User:PASS@GitHub.com/Owner/Repo.git");
    let out = origin_url(&repo).expect("origin_url");
    assert_eq!(out, "https://github.com/Owner/Repo");

    // find_git_worktree_root walks up.
    let nested = repo.join("a").join("b");
    std::fs::create_dir_all(&nested).unwrap();
    let resolved = find_git_worktree_root(&nested).expect("worktree root");
    // Compare canonical paths on Windows where temp paths sometimes
    // surface with a short-name prefix.
    let resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
    let repo_canon = std::fs::canonicalize(&repo).unwrap_or(repo);
    assert_eq!(resolved, repo_canon);
}

#[test]
fn cargo_lock_tracked_returns_true_after_commit() {
    if skip_unless_in_container("cargo_lock_tracked_returns_true_after_commit") {
        return;
    }
    let repo = unique_temp_dir("tracked-repo");
    init_git_repo_with_origin(&repo, "git@github.com:zackees/soldr.git");
    assert!(cargo_lock_is_tracked(&repo));
    assert!(!cargo_lock_is_gitignored(&repo));
}

#[test]
fn cargo_lock_tracked_returns_false_when_lock_is_untracked() {
    if skip_unless_in_container("cargo_lock_tracked_returns_false_when_lock_is_untracked") {
        return;
    }
    let repo = unique_temp_dir("untracked-repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_git_in(&repo, &["init", "-q", "-b", "main"]);
    run_git_in(&repo, &["config", "user.email", "u@example.com"]);
    run_git_in(&repo, &["config", "user.name", "u"]);
    write_file(
        &repo.join("Cargo.toml"),
        b"[package]\nname='x'\nversion='0'\n",
    );
    write_file(&repo.join("Cargo.lock"), b"# lock\n");
    run_git_in(&repo, &["add", "Cargo.toml"]);
    run_git_in(&repo, &["commit", "-q", "-m", "no lock"]);
    // Cargo.lock exists on disk but is NOT in git.
    assert!(!cargo_lock_is_tracked(&repo));
}

#[test]
fn cargo_lock_gitignored_returns_true_when_pattern_matches() {
    if skip_unless_in_container("cargo_lock_gitignored_returns_true_when_pattern_matches") {
        return;
    }
    let repo = unique_temp_dir("gitignored-repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_git_in(&repo, &["init", "-q", "-b", "main"]);
    run_git_in(&repo, &["config", "user.email", "u@example.com"]);
    run_git_in(&repo, &["config", "user.name", "u"]);
    write_file(&repo.join(".gitignore"), b"Cargo.lock\n");
    write_file(
        &repo.join("Cargo.toml"),
        b"[package]\nname='x'\nversion='0'\n",
    );
    write_file(&repo.join("Cargo.lock"), b"# ignored lock\n");
    run_git_in(&repo, &["add", "Cargo.toml", ".gitignore"]);
    run_git_in(&repo, &["commit", "-q", "-m", "gitignore"]);
    assert!(cargo_lock_is_gitignored(&repo));
    assert!(!cargo_lock_is_tracked(&repo));
}

#[test]
fn no_git_workspace_reports_no_origin_and_no_worktree() {
    if skip_unless_in_container("no_git_workspace_reports_no_origin_and_no_worktree") {
        return;
    }
    let dir = unique_temp_dir("no-git");
    write_file(
        &dir.join("Cargo.toml"),
        b"[package]\nname='nogit'\nversion='0'\n",
    );
    write_file(&dir.join("Cargo.lock"), b"# lock\n");
    assert!(find_git_worktree_root(&dir).is_none());
    assert!(origin_url(&dir).is_none());
    assert!(!cargo_lock_is_tracked(&dir));
    assert!(!cargo_lock_is_gitignored(&dir));
}

// Pure unit-style coverage of normalize_origin_url that's safe to run
// outside the harness too — exercised via the wrapper functions above
// in the harness path but useful as a smoke check anywhere.
#[test]
fn normalize_origin_url_matches_design_examples() {
    assert_eq!(
        normalize_origin_url("https://USER:PASS@GitHub.com:443/Owner/Repo.git"),
        "https://github.com/Owner/Repo"
    );
    assert_eq!(
        normalize_origin_url("git@github.com:Owner/Repo.git"),
        "https://github.com/Owner/Repo"
    );
}
