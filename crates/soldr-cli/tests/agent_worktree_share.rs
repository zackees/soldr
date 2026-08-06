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
use std::time::{Duration, Instant};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Panic-safe ownership of every process and file created by this fixture.
///
/// The test starts the daemon indirectly through `soldr cargo check`, so a
/// plain temporary-directory guard is insufficient: on Windows the live
/// daemon pins its relocated image and cache root. Stop that exact isolated
/// daemon before removing the root, even when an assertion unwinds.
struct FixtureGuard {
    workdir: PathBuf,
    cache_dir: PathBuf,
}

impl FixtureGuard {
    fn new(workdir: PathBuf, cache_dir: PathBuf) -> Self {
        Self { workdir, cache_dir }
    }

    fn daemon_pid(&self) -> Option<u32> {
        let raw = fs::read_to_string(
            self.cache_dir
                .join("cache")
                .join("soldr-daemon")
                .join("daemon.pid"),
        )
        .ok()?;
        raw.lines().next()?.trim().parse().ok()
    }

    fn stop_daemon(&self) -> std::process::Output {
        Command::new(common::soldr_bin())
            .args(["daemon", "stop"])
            .env("SOLDR_CACHE_DIR", &self.cache_dir)
            .env_remove("RUSTC_WRAPPER")
            .output()
            .expect("run soldr daemon stop")
    }

    fn wait_for_daemon_exit(&self, pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !pid_is_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        !pid_is_alive(pid)
    }

    fn stop_and_assert_exited(&self) {
        let pid = self.daemon_pid().expect("daemon PID publication");
        let output = self.stop_daemon();
        assert!(
            output.status.success(),
            "soldr daemon stop failed: stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            self.wait_for_daemon_exit(pid),
            "soldr daemon PID {pid} survived a successful daemon stop"
        );
    }
}

impl Drop for FixtureGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid() {
            let _ = self.stop_daemon();
            let _ = self.wait_for_daemon_exit(pid);
        }
        if let Err(error) = fs::remove_dir_all(&self.workdir) {
            eprintln!(
                "warning: could not remove fixture root {}: {error}",
                self.workdir.display()
            );
        }
    }
}

#[cfg(windows)]
fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: the handle is opened only for a read-only liveness query and is
    // closed on every successful OpenProcess path.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let queried = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        queried != 0 && exit_code == STILL_ACTIVE as u32
    }
}

soldr_cli::timed_test!(
    windows_long_path_publication_survives_fresh_worktree_reuse,
    Duration::from_secs(300),
    {
        let workdir = unique_temp_dir("windows-cache-publication");
        let cache_dir = workdir.join("shared-cache");
        let guard = FixtureGuard::new(workdir.clone(), cache_dir.clone());
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

        let cold_output =
            soldr_cargo_check(&first_worktree, &cache_dir, &workdir.join("cold-target"));
        let cold = read_json(&latest_archived_session_stats(&cache_dir, &cold_output));
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

        let warm_output =
            soldr_cargo_check(&second_worktree, &cache_dir, &workdir.join("warm-target"));
        let warm = read_json(&new_archived_session_stats(
            &cache_dir,
            &first_session_stats,
            &warm_output,
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
        guard.stop_and_assert_exited();
    }
);

soldr_cli::timed_test!(
    windows_deep_cache_root_keeps_staged_linker_path_short,
    Duration::from_secs(300),
    {
        let workdir = unique_temp_dir("windows-deep-cache-staging");
        let mut cache_dir = workdir.join("cache-root");
        while cache_dir.as_os_str().len() < 165 {
            cache_dir = cache_dir.join("deep-cache-segment");
        }
        let guard = FixtureGuard::new(workdir.clone(), cache_dir.clone());
        let crate_dir = workdir.join("test-crate");
        fs::create_dir_all(&cache_dir).expect("create deep cache dir");
        create_test_crate(&crate_dir);

        let legacy_staging_probe = cache_dir
            .join("cache/zccache/daemon-state/embedded-v1/v1.13.1/staging")
            .join("12345-0-1785588800636122100")
            .join(".compile-12345-1")
            .join("build_script_build-52378a44826b4cb2.exe");
        assert!(
            legacy_staging_probe.as_os_str().len() > 260,
            "fixture must exceed MAX_PATH on the former linker path: {}",
            legacy_staging_probe.display()
        );

        let output = soldr_cargo_check(&crate_dir, &cache_dir, &workdir.join("target"));
        assert!(
            !output.contains("LNK1104"),
            "link.exe must not receive the deep cache-root staging path: {output}"
        );
        assert!(
            !cache_dir
                .join("cache/zccache/daemon-state/embedded-v1")
                .join(zccache::core::config::versioned_subdir())
                .join("staging")
                .exists(),
            "Windows compiler staging must be outside the deep durable cache root"
        );

        let stats = read_json(&latest_archived_session_stats(&cache_dir, &output));
        let misses = u64_field(&stats, "misses");
        assert!(
            misses > 0,
            "cold fixture must exercise compiler misses: {stats:#?}"
        );
        assert_eq!(
            staged_counter(&stats, "publication_success"),
            misses,
            "the fix must preserve staged publication instead of bypassing it: {stats:#?}"
        );
        guard.stop_and_assert_exited();
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

/// Runs the fixture build and returns soldr's combined output.
///
/// The output is returned rather than dropped because it names the paths
/// soldr actually used ("archived session stats <path>"). When a later
/// assertion finds no archived stats, that line is what distinguishes "the
/// build archived nothing" from "the build archived somewhere else" — see
/// `describe_missing_session_stats`.
///
/// `RUSTC_WRAPPER` is cleared for the same reason `stop_daemon` clears it:
/// this repo dogfoods, so `soldr cargo test` runs with a wrapper pointing at
/// the *outer* soldr. Inheriting it into a fixture build mixes two soldr
/// installations in one compile.
fn soldr_cargo_check(worktree: &Path, cache_dir: &Path, target_dir: &Path) -> String {
    let output = Command::new(common::soldr_bin())
        .args(["cargo", "check"])
        .current_dir(worktree)
        .env("SOLDR_CACHE_DIR", cache_dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("RUSTC_WRAPPER")
        .output()
        .expect("spawn soldr cargo check");
    let rendered = format!(
        "stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "soldr cargo check failed in {}: {rendered}",
        worktree.display(),
    );
    rendered
}

/// Explains an empty history directory instead of just naming it.
///
/// A build that succeeds but archives nothing looks identical, from the
/// assertion site, to one that archived under a different cache root. Report
/// what actually exists under the redirected root, plus soldr's own account
/// of where it wrote, so the next reader does not have to guess.
fn describe_missing_session_stats(cache_dir: &Path, check_output: &str) -> String {
    let history = cache_dir.join("cache").join("zccache").join("history");
    let mut report = format!("no archived session stats under {}", history.display());

    let mut probes = vec![
        cache_dir.join("cache").join("zccache"),
        // The archive's source file lives here; if it is missing, the archive
        // had nothing to copy.
        cache_dir.join("cache").join("zccache").join("logs"),
        history.clone(),
    ];
    // A session directory that exists but holds no `last-session-stats.json`
    // means the archive was started and never finished — a different bug from
    // "nothing was archived at all", so list each session's contents too.
    if let Ok(entries) = fs::read_dir(&history) {
        probes.extend(entries.filter_map(|e| Some(e.ok()?.path())));
    }
    // soldr#2186: the compile journal is written under
    // `daemon-state/embedded-v1/v<store-version>/logs/`, while the session
    // stats it archives from are read out of the unversioned
    // `cache/zccache/logs/`. When the latter is empty, the question is whether
    // the stats file was never produced or produced somewhere else — and the
    // two have different fixes, one in publication and one in the path.
    // Listing the versioned logs dirs answers it without another round trip.
    let daemon_state = cache_dir.join("cache").join("zccache").join("daemon-state");
    if let Ok(stores) = fs::read_dir(&daemon_state) {
        for store in stores.filter_map(|e| Some(e.ok()?.path())) {
            if let Ok(versions) = fs::read_dir(&store) {
                probes.extend(versions.filter_map(|e| Some(e.ok()?.path().join("logs"))));
            }
        }
    }

    for probe in probes {
        let listing = match fs::read_dir(&probe) {
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
                    .collect();
                names.sort();
                if names.is_empty() {
                    "<empty>".to_string()
                } else {
                    names.join(", ")
                }
            }
            Err(err) => format!("<unreadable: {err}>"),
        };
        report.push_str(&format!("\n  {} -> {listing}", probe.display()));
    }

    // soldr prints the paths it used; if it archived elsewhere, this says so.
    report.push_str(&format!("\n  soldr cargo check output: {check_output}"));
    report
}

fn latest_archived_session_stats(cache_dir: &Path, check_output: &str) -> PathBuf {
    archived_session_stats(cache_dir)
        .into_iter()
        .next()
        .unwrap_or_else(|| {
            panic!(
                "{}",
                describe_missing_session_stats(cache_dir, check_output)
            )
        })
}

fn new_archived_session_stats(
    cache_dir: &Path,
    previous: &[PathBuf],
    check_output: &str,
) -> PathBuf {
    archived_session_stats(cache_dir)
        .into_iter()
        .find(|path| !previous.contains(path))
        .unwrap_or_else(|| {
            panic!(
                "no newly archived session stats after warm build (cold build left {} session(s))\n{}",
                previous.len(),
                describe_missing_session_stats(cache_dir, check_output),
            )
        })
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
