//! Integration tests for PR 3 (#578): cargo-front-door pre-flight
//! auto-hydrate.
//!
//! These tests spawn `soldr-daemon` against a sandboxed `~/.soldr/`
//! and drive the hydrate flow end-to-end via the public surfaces:
//! pack → record → lookup → SHA-verify → extract → touch. The flow
//! mirrors what `cargo_front_door::cook_hydrate::maybe_hydrate` does
//! internally; this test exercises the same building blocks.
//!
//! Docker harness gate: every test short-circuits when
//! `SOLDR_COOK_DOCKER_HARNESS` is not set to `1`. The supported
//! runner is `bench/cook_in_docker.sh`. Bare-host runs skip
//! everything so the host developer's soldr singleton stays
//! untouched (see meta #579 + `docs/CONTRIBUTING_COOK.md`).

#![allow(clippy::print_stdout)]

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::cache_lib::cook_archive::{
    compute_recipe_hash_proxy, cook_cache_dir, extract_skip_existing, pack_cook_archive,
    quarantine_artifact, verify_sha256,
};
use soldr_cli::core::{probe_toolchain_binary, TargetTriple};
use soldr_cli::daemon::client::{
    self, cook_lookup, cook_record, cook_record_with_branch_timing, CookLookupOutcome,
};

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
    let dir = std::env::temp_dir().join(format!("soldr-cookhydrate-{label}-{nanos}"));
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

fn soldr_bin() -> PathBuf {
    common::soldr_bin()
}

fn run_git_in(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?} failed in {}\nstdout:\n{}\nstderr:\n{}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn run_command_success(mut command: Command, label: &str) {
    let output = command.output().expect(label);
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn rustc_version_string(manifest_dir: &Path) -> String {
    let rustc = probe_toolchain_binary("rustc", Some(manifest_dir))
        .unwrap_or_else(|| PathBuf::from("rustc"));
    let output = Command::new(rustc).arg("-V").output().expect("rustc -V");
    assert!(
        output.status.success(),
        "rustc -V failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .expect("rustc -V stdout is UTF-8")
        .lines()
        .next()
        .expect("rustc -V emitted a line")
        .trim()
        .to_string()
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
            if pid_path.exists() && soldr_cli::daemon::client::status(&sock).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            pid_path.exists() && soldr_cli::daemon::client::status(&sock).is_ok(),
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

fn write_synthetic_target_release(root: &Path) {
    write_file(
        &root.join("deps").join("libfoo-abc.rlib"),
        b"rlib foo bytes\n",
    );
    write_file(
        &root.join("deps").join("libbar-def.rmeta"),
        b"rmeta bar bytes\n",
    );
    write_file(
        &root.join("deps").join("libbaz-ghi.rlib"),
        b"rlib baz bytes\n",
    );
}

// End-to-end hydrate cycle, mirrors the call sequence inside
// `cook_hydrate::maybe_hydrate`:
// pack → record → lookup → verify_sha256 → extract → touch.
#[test]
fn pack_record_lookup_verify_extract_completes_hydrate_cycle() {
    if skip_unless_in_container("pack_record_lookup_verify_extract_completes_hydrate_cycle") {
        return;
    }
    let cache_root = unique_temp_dir("hyd-e2e-cache");
    let home_root = unique_temp_dir("hyd-e2e-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    // Producer side: pack a synthetic target/release tree.
    let project_target = unique_temp_dir("hyd-e2e-project").join("target");
    let release_dir = project_target.join("release");
    write_synthetic_target_release(&release_dir);
    let paths = soldr_cli::core::SoldrPaths::with_root(cache_root.clone());
    let cook_dir = cook_cache_dir(&paths);
    std::fs::create_dir_all(&cook_dir).unwrap();
    let packed = pack_cook_archive(&release_dir, &cook_dir).expect("pack");

    // Register with the daemon under a fixed recipe hash.
    cook_record(
        &sock,
        [0x42u8; 32],
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

    // Consumer side: lookup, verify, extract into a fresh
    // (empty) target/ tree.
    let outcome = cook_lookup(
        &sock,
        [0x42u8; 32],
        "x86_64-unknown-linux-gnu".to_string(),
        "release".to_string(),
        "1.94.1".to_string(),
        "rustc 1.94.1 (test)".to_string(),
        Some("https://github.com/zackees/soldr".to_string()),
    )
    .expect("cook_lookup");

    let CookLookupOutcome::Hit {
        sha256,
        path,
        size_bytes,
        ..
    } = outcome
    else {
        panic!("expected CookHit");
    };

    assert_eq!(sha256, packed.sha256);
    assert_eq!(size_bytes, packed.size_bytes);

    let artifact = PathBuf::from(&path);
    assert!(verify_sha256(&artifact, &sha256).expect("verify"));

    let restore_target = unique_temp_dir("hyd-e2e-restore").join("target");
    std::fs::create_dir_all(&restore_target).unwrap();
    let report = extract_skip_existing(&artifact, &restore_target).expect("extract");
    assert!(report.files_written >= 3, "expected at least 3 files");
    assert_eq!(report.files_skipped, 0);

    // Verify the extracted file contents match the source.
    let restored = std::fs::read(
        restore_target
            .join("release")
            .join("deps")
            .join("libfoo-abc.rlib"),
    )
    .expect("read foo");
    assert_eq!(restored, b"rlib foo bytes\n");
}

// SHA verification mismatch must NOT extract into target/. Hydrate
// must move the corrupted file to .quarantine and fall through.
#[test]
fn sha_mismatch_quarantines_artifact_and_does_not_extract() {
    if skip_unless_in_container("sha_mismatch_quarantines_artifact_and_does_not_extract") {
        return;
    }
    let cache_root = unique_temp_dir("hyd-mismatch-cache");
    let home_root = unique_temp_dir("hyd-mismatch-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    let project_target = unique_temp_dir("hyd-mismatch-project").join("target");
    let release_dir = project_target.join("release");
    write_synthetic_target_release(&release_dir);
    let paths = soldr_cli::core::SoldrPaths::with_root(cache_root.clone());
    let cook_dir = cook_cache_dir(&paths);
    std::fs::create_dir_all(&cook_dir).unwrap();
    let packed = pack_cook_archive(&release_dir, &cook_dir).expect("pack");

    // Corrupt the artifact on disk so verify_sha256 fails.
    {
        use std::io::Seek;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&packed.path)
            .expect("open");
        f.seek(std::io::SeekFrom::Start(0)).unwrap();
        f.write_all(&[0x00u8; 4]).unwrap();
    }

    // verify_sha256 must report mismatch.
    assert!(!verify_sha256(&packed.path, &packed.sha256).expect("verify"));

    // Hydrate caller would quarantine — exercise the function.
    let quarantined = quarantine_artifact(&packed.path).expect("quarantine");
    assert!(!packed.path.exists());
    assert!(quarantined.exists());
    assert!(quarantined
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(".quarantine"));

    // Nothing extracted to a fresh restore target.
    let restore_target = unique_temp_dir("hyd-mismatch-restore").join("target");
    std::fs::create_dir_all(&restore_target).unwrap();
    // (We don't call extract on the quarantine path; the
    // hydrate code path would have already returned None.)
    let entries: Vec<_> = std::fs::read_dir(&restore_target).unwrap().collect();
    assert!(
        entries.is_empty(),
        "target/ must stay untouched on mismatch"
    );

    // Smoke test: lookup still works on the daemon side after
    // the on-disk artifact is gone.
    let lookup = cook_lookup(
        &sock,
        [0x55u8; 32],
        "x86_64-unknown-linux-gnu".to_string(),
        "release".to_string(),
        "1.94.1".to_string(),
        "rustc 1.94.1 (test)".to_string(),
        None,
    )
    .expect("lookup");
    assert!(matches!(lookup, CookLookupOutcome::Miss { .. }));
}

// Hydrate is additive — files that already exist in `target/` are
// preserved. This is the "never overwrite user state" invariant.
#[test]
fn hydrate_is_additive_skip_existing_preserves_user_files() {
    if skip_unless_in_container("hydrate_is_additive_skip_existing_preserves_user_files") {
        return;
    }
    let cache_root = unique_temp_dir("hyd-add-cache");
    let home_root = unique_temp_dir("hyd-add-home");
    let _daemon = DaemonProc::spawn(&cache_root, &home_root);

    // Source: an archive containing libfoo with content FROM_ARCHIVE.
    let project = unique_temp_dir("hyd-add-project").join("target");
    let release = project.join("release");
    write_file(&release.join("deps").join("libfoo.rlib"), b"FROM_ARCHIVE\n");
    let paths = soldr_cli::core::SoldrPaths::with_root(cache_root.clone());
    let cook_dir = cook_cache_dir(&paths);
    std::fs::create_dir_all(&cook_dir).unwrap();
    let packed = pack_cook_archive(&release, &cook_dir).expect("pack");

    // Destination: user-owned target/ already contains libfoo
    // with a different content. extract_skip_existing must
    // preserve the user's bytes.
    let dest_target = unique_temp_dir("hyd-add-restore").join("target");
    let user_path = dest_target.join("release").join("deps").join("libfoo.rlib");
    write_file(&user_path, b"USER_OWNED\n");

    let report = extract_skip_existing(&packed.path, &dest_target).expect("extract");
    assert!(
        report.files_skipped >= 1,
        "must skip at least the user file"
    );

    let on_disk = std::fs::read(&user_path).expect("read");
    assert_eq!(on_disk, b"USER_OWNED\n");
}

// CookLookup miss on a daemon-up workspace returns silently — the
// pre-flight gives cargo a clean fall-through path. This guards
// the "miss is silent" UX invariant.
#[test]
fn cook_lookup_miss_is_silent_and_returns_cleanly() {
    if skip_unless_in_container("cook_lookup_miss_is_silent_and_returns_cleanly") {
        return;
    }
    let cache_root = unique_temp_dir("hyd-miss-cache");
    let home_root = unique_temp_dir("hyd-miss-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    // Lookup with no prior records → CookMiss. Hydrate would
    // simply return None here.
    let outcome = cook_lookup(
        &sock,
        [0xEEu8; 32],
        "x86_64-unknown-linux-gnu".to_string(),
        "release".to_string(),
        "1.94.1".to_string(),
        "rustc 1.94.1 (test)".to_string(),
        None,
    )
    .expect("cook_lookup");
    assert!(matches!(outcome, CookLookupOutcome::Miss { .. }));

    // No artifact should exist under ~/.soldr/cache/cook/.
    let paths = soldr_cli::core::SoldrPaths::with_root(cache_root.clone());
    let cook_dir = cook_cache_dir(&paths);
    let count = std::fs::read_dir(&cook_dir)
        .map(|it| {
            it.flatten()
                .filter(|e| e.path().extension().map(|x| x == "zst").unwrap_or(false))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(count, 0);
}

// A feature branch with a new recipe hash can still hydrate from an
// ancestor `main` branch artifact when the origin, target, profile,
// channel, and rustc version are compatible. This exercises the real
// `soldr cargo build` pre-flight path in a container-local git repo.
#[test]
fn feature_branch_soldr_cargo_build_hydrates_from_main_fallback() {
    if skip_unless_in_container("feature_branch_soldr_cargo_build_hydrates_from_main_fallback") {
        return;
    }

    let cache_root = unique_temp_dir("branch-fallback-cache");
    let home_root = unique_temp_dir("branch-fallback-home");
    let daemon = DaemonProc::spawn(&cache_root, &home_root);
    let sock = daemon.sock_path();

    let origin = "https://github.com/example/branch-fallback-demo";
    let repo = unique_temp_dir("branch-fallback-repo");
    run_git_in(&repo, &["init", "-q", "-b", "main"]);
    run_git_in(&repo, &["config", "user.email", "cook@example.com"]);
    run_git_in(&repo, &["config", "user.name", "cook test"]);
    run_git_in(&repo, &["remote", "add", "origin", origin]);

    write_file(
        &repo.join("Cargo.toml"),
        br#"[package]
name = "branch_fallback_demo"
version = "0.1.0"
edition = "2021"

[package.metadata.soldr-test]
marker = "main"
"#,
    );
    write_file(
        &repo.join("src").join("main.rs"),
        b"fn main() { println!(\"branch fallback demo\"); }\n",
    );
    let mut generate_lock = Command::new(common::cargo_bin());
    generate_lock.arg("generate-lockfile").current_dir(&repo);
    run_command_success(generate_lock, "cargo generate-lockfile");
    run_git_in(&repo, &["add", "Cargo.toml", "Cargo.lock", "src/main.rs"]);
    run_git_in(&repo, &["commit", "-q", "-m", "main"]);

    let main_recipe = compute_recipe_hash_proxy(&repo).expect("main recipe hash");
    let target_triple = TargetTriple::detect_in_dir(&repo)
        .expect("target triple")
        .to_string();
    let rustc_version = rustc_version_string(&repo);
    let channel = String::new();

    let paths = soldr_cli::core::SoldrPaths::with_root(cache_root.clone());
    let cook_dir = cook_cache_dir(&paths);
    std::fs::create_dir_all(&cook_dir).unwrap();
    let artifact_root = unique_temp_dir("branch-main-artifact");
    let debug_dir = artifact_root.join("debug");
    let sentinel_path = Path::new("debug")
        .join("deps")
        .join("soldr-main-seed.rmeta");
    write_file(
        &debug_dir.join("deps").join("soldr-main-seed.rmeta"),
        b"main branch synthetic artifact\n",
    );
    let packed = pack_cook_archive(&debug_dir, &cook_dir).expect("pack main artifact");

    cook_record_with_branch_timing(
        &sock,
        main_recipe,
        target_triple,
        "dev".to_string(),
        channel,
        rustc_version,
        packed.sha256,
        packed.size_bytes,
        Some(origin.to_string()),
        Some("main".to_string()),
        "synthetic main cook".to_string(),
        60_000,
        5_000,
    )
    .expect("record main cook artifact");

    run_git_in(&repo, &["checkout", "-q", "-b", "feature/reuse-main"]);
    write_file(
        &repo.join("Cargo.toml"),
        br#"[package]
name = "branch_fallback_demo"
version = "0.1.0"
edition = "2021"

[package.metadata.soldr-test]
marker = "feature"
"#,
    );
    run_git_in(&repo, &["add", "Cargo.toml"]);
    run_git_in(&repo, &["commit", "-q", "-m", "feature recipe drift"]);

    let feature_recipe = compute_recipe_hash_proxy(&repo).expect("feature recipe hash");
    assert_ne!(
        feature_recipe, main_recipe,
        "feature branch metadata edit must drift the recipe hash"
    );
    assert!(
        !repo.join("target").join(&sentinel_path).exists(),
        "sentinel should not exist before auto-hydrate"
    );

    let mut soldr = Command::new(soldr_bin());
    common::isolated_daemon::configure_isolated_daemon_client(
        &mut soldr,
        &soldr_daemon_bin(),
        &cache_root,
    );
    soldr
        .current_dir(&repo)
        .args(["--no-cache", "cargo", "build", "--quiet"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("HOME", &home_root)
        .env("USERPROFILE", &home_root)
        .env("CARGO_TARGET_DIR", repo.join("target"))
        .env("SOLDR_TEST_DIRECT_DAEMON_CONTROL", "1")
        .env("SOLDR_COOK_AUTO_HYDRATE", "1")
        .env("SOLDR_NO_GC_TARGET", "1")
        .env("SOLDR_TEST_FREE_DISK_BYTES", "21474836480")
        .env("SOLDR_TEST_DISK_FREE_BYTES", "21474836480");
    let output = soldr.output().expect("soldr cargo build");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "soldr cargo build failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("soldr cook: auto-hydrate activated"),
        "expected auto-hydrate line\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("size_bytes={}", packed.size_bytes)),
        "expected raw archive bytes in hydrate line\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("decision=restore"),
        "expected restore decision in hydrate line\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("match=fallback"),
        "expected branch fallback hydrate\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("branch=main"),
        "expected fallback artifact to come from main\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
            repo.join("target").join(&sentinel_path).is_file(),
            "expected sentinel artifact to be extracted into target/\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
}
