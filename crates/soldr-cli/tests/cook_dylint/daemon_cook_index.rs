//! Integration tests for the dormant cook-index daemon surface added
//! in PR 1 of meta issue #579 (sub-issue #576).
//!
//! These tests spawn the real `soldr-daemon` binary against a sandboxed
//! `~/.soldr/` (`SOLDR_CACHE_DIR` + `HOME`/`USERPROFILE` redirect to a
//! per-test temp dir) and exercise the new `CookLookup`, `CookRecord`,
//! `CookTouch`, and extended `Status` IPC variants via the public
//! client helpers.
//!
//! ## Docker harness gate
//!
//! Every test below short-circuits with a `println!` when
//! `SOLDR_COOK_DOCKER_HARNESS` is not set to `1`. The meta issue
//! mandates that PRs 1–3 build and test inside the
//! `docker/cook-shared-cache/Dockerfile` image so the host developer's
//! soldr singleton is never mutated. `bench/cook_in_docker.sh` is the
//! supported runner — it builds the image, mounts the source tree, and
//! exports the marker. Bare-host `cargo test` invocations skip these
//! tests, satisfying the "host ~/.soldr/ is byte-identical before and
//! after the test suite" acceptance gate.

#![allow(clippy::print_stdout)]

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::daemon::client::{self, cook_lookup, cook_record, cook_touch, CookLookupOutcome};
use soldr_cli::daemon::protocol::Response;

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
    println!(
        "{test_name}: skipped — {HARNESS_ENV} not set. \
         Run via bench/cook_in_docker.sh per meta #579.",
    );
    true
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-cook-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
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
        // Construct `SoldrPaths::with_root(...)` directly rather than
        // mutating `SOLDR_CACHE_DIR` on the process env — these tests
        // run concurrently under `cargo test` and env mutation is
        // process-wide, so an EnvScope-based approach races between
        // tests. `with_root` is the same shape the daemon resolves
        // to internally (its own SOLDR_CACHE_DIR was set on its
        // child env at spawn).
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

fn standard_key_fields() -> (String, String, String, String) {
    (
        "x86_64-unknown-linux-gnu".to_string(),
        "release".to_string(),
        "1.94.1".to_string(),
        "rustc 1.94.1 (abcdef0 2026-05-30)".to_string(),
    )
}

#[test]
fn cook_record_then_lookup_round_trips_through_daemon() {
    if skip_unless_in_container("cook_record_then_lookup_round_trips_through_daemon") {
        return;
    }
    let cache_root = unique_temp_dir("rt-cache");
    let home_root = unique_temp_dir("rt-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    let (triple, profile, channel, rustc) = standard_key_fields();
    cook_record(
        &sock,
        [1u8; 32],
        triple.clone(),
        profile.clone(),
        channel.clone(),
        rustc.clone(),
        [0xAAu8; 32],
        4_096,
        Some("https://github.com/zackees/soldr".to_string()),
        "cook --release".to_string(),
    )
    .expect("CookRecord");

    let outcome = cook_lookup(
        &sock,
        [1u8; 32],
        triple,
        profile,
        channel,
        rustc,
        Some("https://github.com/zackees/soldr".to_string()),
    )
    .expect("CookLookup");

    match outcome {
        CookLookupOutcome::Hit {
            sha256,
            path,
            size_bytes,
            origin_url_normalized,
            ..
        } => {
            assert_eq!(sha256, [0xAAu8; 32]);
            assert_eq!(size_bytes, 4_096);
            assert_eq!(
                origin_url_normalized.as_deref(),
                Some("https://github.com/zackees/soldr")
            );
            assert!(
                path.ends_with(".tar.zst"),
                "expected <sha256>.tar.zst path, got {path}"
            );
        }
        CookLookupOutcome::Miss { .. } => panic!("expected CookHit, got CookMiss"),
    }
}

#[test]
fn cook_lookup_recipe_miss_falls_back_to_newest_same_origin_artifact() {
    if skip_unless_in_container("cook_lookup_recipe_miss_falls_back_to_newest_same_origin_artifact")
    {
        return;
    }
    let cache_root = unique_temp_dir("drift-cache");
    let home_root = unique_temp_dir("drift-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    let (triple, profile, channel, rustc) = standard_key_fields();
    let origin = "https://github.com/zackees/soldr".to_string();

    // Two prior entries with the same origin/triple/profile/channel/rustc
    // but different recipe hashes.
    cook_record(
        &sock,
        [5u8; 32],
        triple.clone(),
        profile.clone(),
        channel.clone(),
        rustc.clone(),
        [0x55u8; 32],
        1,
        Some(origin.clone()),
        "cook a".to_string(),
    )
    .expect("CookRecord a");
    std::thread::sleep(Duration::from_millis(2));
    cook_record(
        &sock,
        [6u8; 32],
        triple.clone(),
        profile.clone(),
        channel.clone(),
        rustc.clone(),
        [0x66u8; 32],
        2,
        Some(origin.clone()),
        "cook b".to_string(),
    )
    .expect("CookRecord b");

    // Miss probe for a new (third) recipe hash.
    let outcome = cook_lookup(
        &sock,
        [7u8; 32],
        triple,
        profile,
        channel,
        rustc,
        Some(origin),
    )
    .expect("CookLookup");

    match outcome {
        CookLookupOutcome::Hit {
            sha256,
            matched_recipe_hash,
            exact_recipe_match,
            ..
        } => {
            assert_eq!(sha256, [0x66u8; 32]);
            assert_eq!(matched_recipe_hash, Some([6u8; 32]));
            assert!(!exact_recipe_match);
        }
        CookLookupOutcome::Miss { .. } => panic!("expected fallback CookHit, got CookMiss"),
    }
}

#[test]
fn concurrent_cook_records_all_land_consistently() {
    if skip_unless_in_container("concurrent_cook_records_all_land_consistently") {
        return;
    }
    let cache_root = unique_temp_dir("concur-cache");
    let home_root = unique_temp_dir("concur-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    const WORKERS: usize = 8;
    let mut handles = Vec::with_capacity(WORKERS);
    for w in 0..WORKERS {
        let sock = sock.clone();
        handles.push(std::thread::spawn(move || {
            let (triple, profile, channel, rustc) = standard_key_fields();
            let recipe = [w as u8 + 1; 32];
            let sha = [(w as u8) + 0x80; 32];
            cook_record(
                &sock,
                recipe,
                triple,
                profile,
                channel,
                rustc,
                sha,
                1_000 + (w as u64) * 10,
                Some("https://github.com/zackees/soldr".to_string()),
                format!("cook --worker {w}"),
            )
            .expect("CookRecord");
        }));
    }
    for h in handles {
        h.join().expect("worker join");
    }

    // Status reports cook_entries == WORKERS and the total bytes
    // is the sum of all worker payloads.
    let status = client::status(&sock).expect("status");
    let cook = status.cook_stats_or_zero();
    assert_eq!(cook.entries, WORKERS as u64);
    let expected: u64 = (0..WORKERS).map(|w| 1_000u64 + (w as u64) * 10).sum();
    assert_eq!(cook.total_bytes, expected);
}

#[test]
fn per_target_safety_isolates_via_ipc() {
    if skip_unless_in_container("per_target_safety_isolates_via_ipc") {
        return;
    }
    let cache_root = unique_temp_dir("target-cache");
    let home_root = unique_temp_dir("target-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    let (_, profile, channel, rustc) = standard_key_fields();

    // Same recipe hash, two different triples — both inserts succeed
    // and lookups return their respective shas with NO cross-pollination.
    cook_record(
        &sock,
        [0x42u8; 32],
        "x86_64-unknown-linux-gnu".to_string(),
        profile.clone(),
        channel.clone(),
        rustc.clone(),
        [0xAAu8; 32],
        100,
        None,
        "cook linux".to_string(),
    )
    .expect("linux");
    cook_record(
        &sock,
        [0x42u8; 32],
        "aarch64-apple-darwin".to_string(),
        profile.clone(),
        channel.clone(),
        rustc.clone(),
        [0xBBu8; 32],
        200,
        None,
        "cook mac".to_string(),
    )
    .expect("mac");

    let linux = cook_lookup(
        &sock,
        [0x42u8; 32],
        "x86_64-unknown-linux-gnu".to_string(),
        profile.clone(),
        channel.clone(),
        rustc.clone(),
        None,
    )
    .expect("lookup linux");
    let mac = cook_lookup(
        &sock,
        [0x42u8; 32],
        "aarch64-apple-darwin".to_string(),
        profile,
        channel,
        rustc,
        None,
    )
    .expect("lookup mac");

    let CookLookupOutcome::Hit {
        sha256: linux_sha,
        size_bytes: linux_size,
        ..
    } = linux
    else {
        panic!("expected linux hit")
    };
    let CookLookupOutcome::Hit {
        sha256: mac_sha,
        size_bytes: mac_size,
        ..
    } = mac
    else {
        panic!("expected mac hit")
    };

    assert_eq!(linux_sha, [0xAAu8; 32]);
    assert_eq!(linux_size, 100);
    assert_eq!(mac_sha, [0xBBu8; 32]);
    assert_eq!(mac_size, 200);

    // Probing yet a third triple with the same recipe hash → MISS.
    let other = cook_lookup(
        &sock,
        [0x42u8; 32],
        "x86_64-pc-windows-msvc".to_string(),
        "release".to_string(),
        "1.94.1".to_string(),
        "rustc 1.94.1 (abcdef0 2026-05-30)".to_string(),
        None,
    )
    .expect("lookup other");
    assert!(matches!(other, CookLookupOutcome::Miss { .. }));
}

#[test]
fn status_reports_aggregate_cook_metrics() {
    if skip_unless_in_container("status_reports_aggregate_cook_metrics") {
        return;
    }
    let cache_root = unique_temp_dir("status-cache");
    let home_root = unique_temp_dir("status-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    // Empty index — cook_stats should be Some(zero).
    let initial = client::status(&sock).expect("status");
    let initial_cook = initial.cook_stats_or_zero();
    assert_eq!(initial_cook.entries, 0);
    assert_eq!(initial_cook.total_bytes, 0);
    assert_eq!(initial_cook.hits_this_session, 0);

    let (triple, profile, channel, rustc) = standard_key_fields();
    for i in 0..3u8 {
        cook_record(
            &sock,
            [i + 1; 32],
            triple.clone(),
            profile.clone(),
            channel.clone(),
            rustc.clone(),
            [0xC0 + i; 32],
            (i as u64 + 1) * 1_024,
            None,
            "cook".to_string(),
        )
        .expect("record");
    }

    let after_writes = client::status(&sock).expect("status");
    let cook = after_writes.cook_stats_or_zero();
    assert_eq!(cook.entries, 3);
    assert_eq!(cook.total_bytes, 1_024 + 2_048 + 3_072);
    // No CookLookup hits yet.
    assert_eq!(cook.hits_this_session, 0);

    // Drive a hit — Status should now reflect it.
    let outcome =
        cook_lookup(&sock, [1u8; 32], triple, profile, channel, rustc, None).expect("lookup");
    assert!(matches!(outcome, CookLookupOutcome::Hit { .. }));

    let after_hit = client::status(&sock).expect("status");
    assert_eq!(after_hit.cook_stats_or_zero().hits_this_session, 1);
}

#[test]
fn cook_touch_is_fire_and_forget_and_silent_on_unknown_sha() {
    if skip_unless_in_container("cook_touch_is_fire_and_forget_and_silent_on_unknown_sha") {
        return;
    }
    let cache_root = unique_temp_dir("touch-cache");
    let home_root = unique_temp_dir("touch-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    // Bumping an unknown sha is a no-op success — fire-and-forget
    // never errors on the caller side.
    cook_touch(&sock, [0xFEu8; 32]).expect("touch unknown");

    // Daemon still responds to Status normally afterwards.
    let status = client::status(&sock).expect("status");
    assert_eq!(status.cook_stats_or_zero().entries, 0);
}

// The Status response payload still encodes `Response::Status` cleanly
// on the wire with the new cook_stats field — guards against an
// accidental break of the existing variant.
#[test]
fn status_response_decodes_as_expected_variant() {
    if skip_unless_in_container("status_response_decodes_as_expected_variant") {
        return;
    }
    let cache_root = unique_temp_dir("variant-cache");
    let home_root = unique_temp_dir("variant-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();
    let resp = client::submit_request(&sock, &soldr_cli::daemon::protocol::Request::Status)
        .expect("submit_request");
    assert!(matches!(resp, Response::Status(_)));
}
