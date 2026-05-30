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
    cook_cache_dir, extract_skip_existing, pack_cook_archive, quarantine_artifact, verify_sha256,
};
use soldr_cli::daemon::client::{self, cook_lookup, cook_record, CookLookupOutcome};
use soldr_cli::timed_test;

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
    let soldr = PathBuf::from(env!("CARGO_BIN_EXE_soldr"));
    let parent = soldr.parent().expect("CARGO_BIN_EXE_soldr has a parent");
    let stem = if cfg!(windows) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    parent.join(stem)
}

struct DaemonProc {
    child: Option<Child>,
    cache_root: PathBuf,
}

impl DaemonProc {
    fn spawn(cache_root: &Path, home_root: &Path) -> Self {
        let mut cmd = Command::new(soldr_daemon_bin());
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn soldr-daemon");
        let deadline = Instant::now() + Duration::from_secs(10);
        let pid_path = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("daemon.pid");
        while Instant::now() < deadline {
            if pid_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            pid_path.exists(),
            "soldr-daemon failed to write {} within 10s",
            pid_path.display()
        );
        Self {
            child: Some(child),
            cache_root: cache_root.to_path_buf(),
        }
    }

    fn sock_path(&self) -> PathBuf {
        let paths = soldr_cli::core::SoldrPaths::with_root(self.cache_root.clone());
        client::default_sock_path(&paths)
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let paths = soldr_cli::core::SoldrPaths::with_root(self.cache_root.clone());
            let _ = client::shutdown(&client::default_sock_path(&paths));
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
timed_test!(
    pack_record_lookup_verify_extract_completes_hydrate_cycle,
    Duration::from_secs(60),
    {
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
);

// SHA verification mismatch must NOT extract into target/. Hydrate
// must move the corrupted file to .quarantine and fall through.
timed_test!(
    sha_mismatch_quarantines_artifact_and_does_not_extract,
    Duration::from_secs(60),
    {
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
);

// Hydrate is additive — files that already exist in `target/` are
// preserved. This is the "never overwrite user state" invariant.
timed_test!(
    hydrate_is_additive_skip_existing_preserves_user_files,
    Duration::from_secs(60),
    {
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
);

// CookLookup miss on a daemon-up workspace returns silently — the
// pre-flight gives cargo a clean fall-through path. This guards
// the "miss is silent" UX invariant.
timed_test!(
    cook_lookup_miss_is_silent_and_returns_cleanly,
    Duration::from_secs(60),
    {
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
);
