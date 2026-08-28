//! End-to-end save/load tests.
//!
//! Each test synthesizes a workspace + cache dir in a tempdir, calls
//! `save`, mutates the workspace to simulate a fresh `actions/checkout`
//! (mtimes pushed forward, sometimes content changed), then calls
//! `load` and asserts the right files got their mtimes restored.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use soldr_cli::cache_lib::save::{
    ci_profile_excludes_cache_path, load, read_manifest_from_archive, save, save_delta,
    CacheLayerKind, LoadOptions, SaveDeltaOptions, SaveOptions, SaveProfile, DEFAULT_ZSTD_LEVEL,
};

fn write(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn touch(path: &Path, ms: i64) {
    let t = filetime::FileTime::from_system_time(UNIX_EPOCH + Duration::from_millis(ms as u64));
    filetime::set_file_times(path, t, t).unwrap();
}

fn archive_paths(archive: &Path) -> Vec<String> {
    let file = fs::File::open(archive).unwrap();
    let reader = std::io::BufReader::new(file);
    let zstd = zstd::stream::read::Decoder::new(reader).unwrap();
    let mut tar = tar::Archive::new(zstd);
    let mut paths = Vec::new();
    for entry in tar.entries().unwrap() {
        let entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().replace('\\', "/");
        paths.push(path);
    }
    paths.sort();
    paths
}

/// Make a tiny realistic workspace + cache.
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspace");
    let cache = dir.path().join("cache");
    let archive = dir.path().join("snap.tar.zst");

    write(
        &ws.join("Cargo.toml"),
        b"[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
    );
    write(&ws.join("Cargo.lock"), b"# lock\n");
    write(&ws.join("src/main.rs"), b"fn main() {}\n");
    write(
        &ws.join("src/lib.rs"),
        b"pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    );
    write(
        &ws.join("crates/sub/Cargo.toml"),
        b"[package]\nname=\"sub\"\n",
    );
    write(
        &ws.join("crates/sub/src/lib.rs"),
        b"pub fn one() -> i32 { 1 }\n",
    );

    // Cache dir with subdirectories — mimics zccache's hash-bucket layout.
    write(&cache.join("ab/cd/object-1.bin"), &[0xAA; 4096]);
    write(&cache.join("ab/cd/object-2.bin"), &[0xBB; 8192]);
    write(&cache.join("ef/01/object-3.bin"), &[0xCC; 2048]);
    write(&cache.join("index.json"), b"{\"version\":1}\n");

    (dir, ws, cache, archive)
}

#[test]
fn ci_profile_exclusion_filter_keeps_cache_payloads_and_drops_runtime_noise() {
    for rel in [
        "logs/session.log",
        "debug/logs/session.jsonl",
        "tmp/extracting.bin",
        "scratch/retry.tmp",
        "server.sock",
        "compile.lock",
        "zccache/runtime-binaries/zccache.exe",
        "bin/soldr.exe",
        "sdk/x86_64-pc-windows-msvc/crt/include/windows.h",
    ] {
        assert!(
            ci_profile_excludes_cache_path(Path::new(rel)),
            "ci profile should exclude {rel}"
        );
    }

    for rel in [
        "ab/cd/object-1.bin",
        "index.json",
        "debug/deps/libsoldr_cli.rlib",
        "release/build/ring/out/libring_core.a",
        ".fingerprint/soldr-cli/dep-bin-soldr",
    ] {
        assert!(
            !ci_profile_excludes_cache_path(Path::new(rel)),
            "ci profile should keep cache payload {rel}"
        );
    }
}

#[test]
fn roundtrip_basic_mtime_restoration() {
    let (_g, ws, cache, archive) = fixture();

    // Snapshot the original mtimes so we can confirm restoration.
    let original_main_mtime = mtime_ms(&ws.join("src/main.rs"));

    let report = save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");
    assert!(
        report.source_files >= 6,
        "expected >=6 source files, got {}",
        report.source_files
    );
    assert_eq!(report.cache_files, 4);

    // Simulate `actions/checkout`: bump every source file's mtime to "now".
    let future = (SystemTime::now() + Duration::from_secs(60))
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    for rel in ["Cargo.toml", "Cargo.lock", "src/main.rs", "src/lib.rs"] {
        touch(&ws.join(rel), future);
    }

    // Clear the cache dir to make the load do real work.
    fs::remove_dir_all(&cache).unwrap();
    fs::create_dir_all(&cache).unwrap();

    let lreport = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&cache),
        workspace: Some(&ws),
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok");

    assert_eq!(lreport.cache_files_restored, 4);
    assert!(
        lreport.mtimes_applied >= 6,
        "expected >=6 mtimes applied, got {}",
        lreport.mtimes_applied
    );
    assert_eq!(lreport.mtimes_skipped_modified, 0);
    assert_eq!(lreport.mtimes_skipped_size_mismatch, 0);

    // Confirm src/main.rs is back to its original mtime.
    let restored = mtime_ms(&ws.join("src/main.rs"));
    assert_eq!(
        restored, original_main_mtime,
        "src/main.rs mtime should be restored exactly",
    );

    // Confirm cache files are at the expected paths (no double-nesting).
    assert!(
        cache.join("ab/cd/object-1.bin").exists(),
        "object-1 should be restored"
    );
    assert!(
        cache.join("index.json").exists(),
        "index.json should be restored"
    );
    assert!(
        !cache.join("cache/ab/cd/object-1.bin").exists(),
        "no double-nesting under cache/"
    );
}

#[test]
fn ci_profile_save_excludes_noise_but_roundtrips_cache_payloads() {
    let (_g, ws, cache, archive) = fixture();
    write(&cache.join("logs/session.log"), b"runtime log");
    write(&cache.join("tmp/inflight.tmp"), &[0xDD; 512]);
    write(&cache.join("daemon.sock"), b"socket placeholder");
    write(&cache.join("sdk/toolchain/bin/clang"), b"managed tool");
    write(
        &cache.join("zccache/runtime-binaries/zccache"),
        b"runtime binary",
    );

    let report = save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Ci,
    })
    .expect("ci profile save ok");
    assert_eq!(report.profile, SaveProfile::Ci);
    assert_eq!(report.cache_files, 4, "only fixture cache payloads remain");
    assert_eq!(report.excluded_files, 5);
    assert!(report.excluded_bytes > 0);

    let paths = archive_paths(&archive);
    assert!(paths.contains(&"cache/ab/cd/object-1.bin".to_string()));
    assert!(paths.contains(&"cache/index.json".to_string()));
    assert!(!paths.iter().any(|p| p.contains("logs/session.log")));
    assert!(!paths.iter().any(|p| p.contains("tmp/inflight.tmp")));
    assert!(!paths.iter().any(|p| p.contains("daemon.sock")));
    assert!(!paths.iter().any(|p| p.contains("sdk/toolchain/bin/clang")));
    assert!(!paths.iter().any(|p| p.contains("runtime-binaries/zccache")));

    let manifest = read_manifest_from_archive(&archive).expect("manifest");
    assert_eq!(manifest.cache_file_count, 4);
    assert!(manifest
        .cache_files
        .iter()
        .all(|entry| !entry.path.contains("logs/") && !entry.path.contains("tmp/")));

    fs::remove_dir_all(&cache).unwrap();
    fs::create_dir_all(&cache).unwrap();
    let lreport = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&cache),
        workspace: Some(&ws),
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("ci profile load ok");

    assert_eq!(lreport.cache_files_restored, 4);
    assert!(cache.join("ab/cd/object-1.bin").exists());
    assert!(cache.join("index.json").exists());
    assert!(!cache.join("logs/session.log").exists());
    assert!(!cache.join("tmp/inflight.tmp").exists());
    assert!(!cache.join("daemon.sock").exists());
    assert!(!cache.join("sdk/toolchain/bin/clang").exists());
    assert!(!cache.join("zccache/runtime-binaries/zccache").exists());
}

#[test]
fn save_and_delta_support_long_cache_paths() {
    let (g, ws, cache, archive) = fixture();
    let long_rel = PathBuf::from("debug/.fingerprint")
        .join("icu_normalizer_data-2aeb5eb1026c3544")
        .join("dep-lib-icu_normalizer_data-2aeb5eb1026c3544-with-extra-long-suffix.bin");
    let archive_rel = format!("cache/{}", long_rel.to_string_lossy().replace('\\', "/"));
    assert!(
        archive_rel.len() > 100,
        "test path must exceed the classic tar path field limit"
    );
    write(&cache.join(&long_rel), b"base-long-path-payload");

    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save accepts long cache archive paths");
    assert!(
        archive_paths(&archive).contains(&archive_rel),
        "complete archive should contain the long cache path"
    );

    let base_manifest = read_manifest_from_archive(&archive).unwrap();
    let delta_archive = g.path().join("delta.tar.zst");
    write(&cache.join(&long_rel), b"delta-long-path-payload");
    save_delta(&SaveDeltaOptions {
        workspace: Some(&ws),
        cache_dir: &cache,
        base_manifest: &base_manifest,
        out: &delta_archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        profile: SaveProfile::Full,
    })
    .expect("delta save accepts long cache archive paths");
    assert!(
        archive_paths(&delta_archive).contains(&archive_rel),
        "delta archive should contain the changed long cache path"
    );
}

#[test]
fn load_skips_content_changed_files() {
    let (_g, ws, cache, archive) = fixture();
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    // Modify src/main.rs CONTENT but keep the size identical (13 bytes)
    // so the hash check is what catches the difference, not size.
    assert_eq!(b"fn main() {}\n".len(), 13);
    assert_eq!(b"fn xain() {}\n".len(), 13);
    write(&ws.join("src/main.rs"), b"fn xain() {}\n");
    let future = (SystemTime::now() + Duration::from_secs(60))
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    touch(&ws.join("src/main.rs"), future);
    let warm_mtime_before = mtime_ms(&ws.join("src/main.rs"));

    let lreport = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&cache),
        workspace: Some(&ws),
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok");

    assert_eq!(
        lreport.mtimes_skipped_modified, 1,
        "exactly one file changed content"
    );
    // The changed file's mtime must NOT be restored.
    let warm_mtime_after = mtime_ms(&ws.join("src/main.rs"));
    assert_eq!(
        warm_mtime_after, warm_mtime_before,
        "changed file's mtime must stay current"
    );
}

#[test]
fn load_skips_missing_files() {
    let (_g, ws, cache, archive) = fixture();
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    fs::remove_file(ws.join("src/main.rs")).unwrap();

    let lreport = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&cache),
        workspace: Some(&ws),
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok");

    assert!(
        lreport.mtimes_skipped_missing >= 1,
        "expected >=1 missing file"
    );
}

#[test]
fn load_skips_size_mismatch_without_hashing() {
    let (_g, ws, cache, archive) = fixture();
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    // Make the file a different size from what was snapshotted.
    write(&ws.join("src/main.rs"), b"fn main() {} // appended\n");

    let lreport = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&cache),
        workspace: Some(&ws),
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok");

    assert!(
        lreport.mtimes_skipped_size_mismatch >= 1,
        "size mismatch should be caught early"
    );
}

#[test]
fn cache_only_archive_skips_mtime_replay() {
    let (_g, _ws, cache, archive) = fixture();
    save(&SaveOptions {
        workspace: None,
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");
    fs::remove_dir_all(&cache).unwrap();
    let r = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&cache),
        workspace: None,
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok");
    assert_eq!(r.cache_files_restored, 4);
    assert_eq!(r.source_files_in_manifest, 0);
    assert_eq!(r.mtimes_applied, 0);
}

// ---- mtimes-only mode (setup-soldr's preserve-source-mtimes feature
// promoted into the soldr CLI; see plan PR 2). ----

#[test]
fn mtimes_only_save_and_load_roundtrip() {
    let (_g, ws, _cache, archive) = fixture();

    // Snapshot original mtimes so we can verify they survive the round
    // trip. The fixture writes files with whatever the current clock
    // says; we pin them to known values for assertion stability.
    for (rel, ms) in [
        ("Cargo.toml", 1_700_000_000_000i64),
        ("Cargo.lock", 1_700_000_010_000),
        ("src/main.rs", 1_700_000_020_000),
        ("src/lib.rs", 1_700_000_030_000),
        ("crates/sub/Cargo.toml", 1_700_000_040_000),
        ("crates/sub/src/lib.rs", 1_700_000_050_000),
    ] {
        touch(&ws.join(rel), ms);
    }

    let sreport = save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: None,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: true,
        profile: SaveProfile::Full,
    })
    .expect("mtimes-only save ok");

    assert!(
        sreport.source_files >= 6,
        "saw {} source files",
        sreport.source_files
    );
    assert_eq!(
        sreport.cache_files, 0,
        "mtimes-only must NOT bundle any cache files"
    );
    assert!(sreport.archive_bytes > 0, "archive must not be empty");

    // Simulate actions/checkout pushing every mtime forward — the
    // standard CI scenario this feature exists to neutralize.
    let later = 1_900_000_000_000i64;
    for rel in [
        "Cargo.toml",
        "Cargo.lock",
        "src/main.rs",
        "src/lib.rs",
        "crates/sub/Cargo.toml",
        "crates/sub/src/lib.rs",
    ] {
        touch(&ws.join(rel), later);
        assert_eq!(mtime_ms(&ws.join(rel)), later, "fixture mtime push failed");
    }

    let lreport = load(&LoadOptions {
        archive: &archive,
        cache_dir: None,
        workspace: Some(&ws),
        threads: None,
        mtimes_only: true,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("mtimes-only load ok");

    assert_eq!(
        lreport.cache_files_restored, 0,
        "mtimes-only must NOT restore any cache files"
    );
    assert_eq!(lreport.mtimes_applied, sreport.source_files);
    assert_eq!(lreport.mtimes_skipped_missing, 0);
    assert_eq!(lreport.mtimes_skipped_size_mismatch, 0);
    assert_eq!(lreport.mtimes_skipped_modified, 0);

    // The pinned mtimes should be back where they were before checkout
    // pushed them forward.
    for (rel, want) in [
        ("Cargo.toml", 1_700_000_000_000i64),
        ("Cargo.lock", 1_700_000_010_000),
        ("src/main.rs", 1_700_000_020_000),
        ("src/lib.rs", 1_700_000_030_000),
    ] {
        let got = mtime_ms(&ws.join(rel));
        assert_eq!(
            got, want,
            "{rel}: mtime not restored (got {got}, want {want})"
        );
    }
}

#[test]
fn mtimes_only_load_refuses_modified_source() {
    let (_g, ws, _cache, archive) = fixture();

    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: None,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: true,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    // Modify a source file with SAME-SIZE content so we exercise the
    // blake3 gate (size-mismatch would short-circuit before hashing).
    // The blake3 gate must refuse to apply the mtime — protecting
    // against the underbuild scenario.
    let original = b"fn main() {}\n"; // 13 bytes
    let altered = b"fn x_n() {}\n\n"; // 13 bytes, same length, different bytes
    assert_eq!(
        original.len(),
        altered.len(),
        "test prep: lengths must match"
    );
    write(&ws.join("src/main.rs"), altered);
    let now_before = mtime_ms(&ws.join("src/main.rs"));

    let r = load(&LoadOptions {
        archive: &archive,
        cache_dir: None,
        workspace: Some(&ws),
        threads: None,
        mtimes_only: true,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok");

    assert!(
        r.mtimes_skipped_modified >= 1,
        "blake3-modified source must be counted as skipped_modified, got {r:?}"
    );
    assert_eq!(
        r.mtimes_skipped_size_mismatch, 0,
        "no size mismatch expected, got {r:?}"
    );

    // Verify on disk: the modified file's mtime was NOT rewound.
    let now_after = mtime_ms(&ws.join("src/main.rs"));
    assert_eq!(
        now_before, now_after,
        "modified file's mtime must be left alone"
    );
}

#[test]
fn mtimes_only_save_without_workspace_errors() {
    let (_g, _ws, _cache, archive) = fixture();
    let err = save(&SaveOptions {
        workspace: None,
        cache_dir: None,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: true,
        profile: SaveProfile::Full,
    })
    .expect_err("save --mtimes-only without workspace must error");
    let msg = err.to_string();
    assert!(
        msg.contains("workspace") || msg.contains("--workspace"),
        "error message must mention workspace: {msg}"
    );
}

#[test]
fn mtimes_only_save_with_cache_dir_errors() {
    let (_g, ws, cache, archive) = fixture();
    let err = save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: true,
        profile: SaveProfile::Full,
    })
    .expect_err("save --mtimes-only WITH cache-dir must error");
    let msg = err.to_string();
    assert!(
        msg.contains("cache") || msg.contains("--cache-dir"),
        "error message must mention cache: {msg}"
    );
}

#[test]
fn mtimes_only_load_rejects_cache_entries() {
    // Build a real cache+manifest archive, then try to load it as
    // mtimes-only. The load must refuse the first cache entry it sees.
    let (_g, ws, cache, archive) = fixture();
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("normal save ok");

    let err = load(&LoadOptions {
        archive: &archive,
        cache_dir: None,
        workspace: Some(&ws),
        threads: None,
        mtimes_only: true,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect_err("mtimes-only load of cache-bearing archive must error");
    let msg = err.to_string();
    assert!(
        msg.contains("mtimes_only") || msg.contains("cache"),
        "error message must mention the cache entry rejection: {msg}"
    );
}

#[test]
fn save_without_cache_or_mtimes_only_errors() {
    let (_g, ws, _cache, archive) = fixture();
    let err = save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: None,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect_err("save without cache and without mtimes_only must error");
    let msg = err.to_string();
    assert!(
        msg.contains("--cache-dir") || msg.contains("--mtimes-only"),
        "error message must mention required flag: {msg}"
    );
}

#[test]
fn delta_cache_roundtrip_restores_base_overlay_tombstones_and_mtimes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let base_archive = dir.path().join("base.tar.zst");
    let delta_archive = dir.path().join("delta.tar.zst");
    let restore = dir.path().join("restore");

    let large = vec![0xAB; 256 * 1024];
    write(&cache.join("deps/large.rlib"), &large);
    write(&cache.join("deps/changed.rmeta"), b"base-content\n");
    write(&cache.join("deps/deleted.d"), b"delete-me\n");
    write(&cache.join("deps/mtime-only.d"), b"same-content\n");

    let large_mtime = 1_700_000_000_000i64;
    let changed_base_mtime = 1_700_000_010_000i64;
    let deleted_mtime = 1_700_000_020_000i64;
    let mtime_only_base_mtime = 1_700_000_025_000i64;
    touch(&cache.join("deps/large.rlib"), large_mtime);
    touch(&cache.join("deps/changed.rmeta"), changed_base_mtime);
    touch(&cache.join("deps/deleted.d"), deleted_mtime);
    touch(&cache.join("deps/mtime-only.d"), mtime_only_base_mtime);

    let base_report = save(&SaveOptions {
        workspace: None,
        cache_dir: Some(&cache),
        out: &base_archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: Some(1),
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("base save ok");
    assert_eq!(base_report.cache_files, 4);

    let base_manifest = read_manifest_from_archive(&base_archive).expect("base manifest");
    assert_eq!(
        base_manifest.cache_layer_kind,
        CacheLayerKind::Complete as i32
    );
    assert_eq!(base_manifest.cache_files.len(), 4);

    write(&cache.join("deps/changed.rmeta"), b"delta-content\n");
    fs::remove_file(cache.join("deps/deleted.d")).unwrap();
    write(&cache.join("deps/new.rmeta"), b"new-content\n");
    let changed_delta_mtime = 1_700_000_030_000i64;
    let new_mtime = 1_700_000_040_000i64;
    let mtime_only_delta_mtime = 1_700_000_050_000i64;
    touch(&cache.join("deps/changed.rmeta"), changed_delta_mtime);
    touch(&cache.join("deps/new.rmeta"), new_mtime);
    touch(&cache.join("deps/mtime-only.d"), mtime_only_delta_mtime);

    let delta_report = save_delta(&SaveDeltaOptions {
        workspace: None,
        cache_dir: &cache,
        base_manifest: &base_manifest,
        out: &delta_archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: Some(1),
        profile: SaveProfile::Full,
    })
    .expect("delta save ok");
    assert_eq!(
        delta_report.cache_files, 3,
        "changed/new files plus a metadata-only mtime update"
    );
    assert_eq!(delta_report.deleted_cache_files, 1);

    let delta_manifest = read_manifest_from_archive(&delta_archive).expect("delta manifest");
    assert_eq!(
        delta_manifest.cache_layer_kind,
        CacheLayerKind::Delta as i32
    );
    assert_eq!(delta_manifest.deleted_cache_paths, vec!["deps/deleted.d"]);
    assert!(
        delta_manifest
            .cache_files
            .iter()
            .any(|entry| entry.path == "deps/mtime-only.d"),
        "mtime-only change should be carried in protobuf metadata"
    );
    let paths = archive_paths(&delta_archive);
    assert!(paths.contains(&"SOLDR_MANIFEST.pb".to_string()));
    assert!(paths.contains(&"cache/deps/changed.rmeta".to_string()));
    assert!(paths.contains(&"cache/deps/new.rmeta".to_string()));
    assert!(
        !paths.contains(&"cache/deps/large.rlib".to_string()),
        "unchanged large files must stay out of the delta archive"
    );
    assert!(
        !paths.contains(&"cache/deps/mtime-only.d".to_string()),
        "mtime-only changes should not upload file bytes"
    );

    load(&LoadOptions {
        archive: &base_archive,
        cache_dir: Some(&restore),
        workspace: None,
        threads: Some(1),
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("base load ok");
    load(&LoadOptions {
        archive: &delta_archive,
        cache_dir: Some(&restore),
        workspace: None,
        threads: Some(1),
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("delta load ok");

    assert_eq!(fs::read(restore.join("deps/large.rlib")).unwrap(), large);
    assert_eq!(
        fs::read(restore.join("deps/changed.rmeta")).unwrap(),
        b"delta-content\n"
    );
    assert_eq!(
        fs::read(restore.join("deps/new.rmeta")).unwrap(),
        b"new-content\n"
    );
    assert_eq!(
        fs::read(restore.join("deps/mtime-only.d")).unwrap(),
        b"same-content\n"
    );
    assert!(!restore.join("deps/deleted.d").exists());
    assert_eq!(mtime_ms(&restore.join("deps/large.rlib")), large_mtime);
    assert_eq!(
        mtime_ms(&restore.join("deps/changed.rmeta")),
        changed_delta_mtime
    );
    assert_eq!(mtime_ms(&restore.join("deps/new.rmeta")), new_mtime);
    assert_eq!(
        mtime_ms(&restore.join("deps/mtime-only.d")),
        mtime_only_delta_mtime
    );
}

/// #575 — exercise the parallel cache-file extraction path with a
/// deliberately large fan-out of small files. Verifies (a) every file
/// round-trips bit-identical, (b) parent directories are created
/// idempotently across workers, (c) per-file mtimes are preserved by
/// the worker-side filetime call, (d) the dispatch shuts down cleanly
/// even when --threads is small enough to back up the bounded channel.
#[test]
fn parallel_extract_many_small_files() {
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspace");
    let cache = dir.path().join("cache");
    let archive = dir.path().join("snap.tar.zst");

    // Workspace stub — the manifest needs something to be valid.
    write(&ws.join("Cargo.toml"), b"[package]\nname=\"x\"\n");
    write(&ws.join("Cargo.lock"), b"# lock\n");

    // Synthesize a fan-out cache: 256 dirs × ~20 small files each = ~5120
    // entries. Small enough to keep the test fast (~1-2s); large enough to
    // exercise dispatch back-pressure on the bounded (cap=64) queue and
    // observe per-worker concurrency.
    let mut expected: BTreeMap<String, (Vec<u8>, i64)> = BTreeMap::new();
    // Pick a fixed deterministic mtime so we can assert on it round-tripping.
    // 2026-04-01 12:00:00 UTC = 1775390400. Use a number divisible into
    // tar's second resolution so we don't fight rounding.
    let pinned_mtime_ms: i64 = 1_775_390_400_000;
    for bucket in 0..16u32 {
        for sub in 0..16u32 {
            for f in 0..20u32 {
                let rel = format!("bucket{:02}/sub{:02}/file{:02}.bin", bucket, sub, f);
                // Vary content per file so a bad worker (e.g. one that
                // wrote the previous job's body to this dest) would corrupt
                // the round-trip.
                let body = format!(
                    "bucket={}, sub={}, file={}, content-{}",
                    bucket,
                    sub,
                    f,
                    bucket * 1000 + sub * 100 + f,
                )
                .into_bytes();
                let abs = cache.join(&rel);
                write(&abs, &body);
                touch(&abs, pinned_mtime_ms);
                // `rel` already uses POSIX "/" separators by construction.
                expected.insert(rel.clone(), (body, pinned_mtime_ms));
            }
        }
    }

    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    let restore = dir.path().join("restored");
    let report = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&restore),
        workspace: None,
        // Force a small worker count so the bounded (cap=64) channel is
        // guaranteed to fill and apply back-pressure to the driver.
        threads: Some(2),
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok");

    assert_eq!(report.cache_files_restored, expected.len() as u64);

    // Bit-identical content + preserved mtime for every entry.
    for (rel, (body, mtime_ms_expected)) in &expected {
        let restored = restore.join(rel);
        let actual = fs::read(&restored).unwrap_or_else(|e| {
            panic!(
                "missing or unreadable restored file {}: {}",
                restored.display(),
                e
            );
        });
        assert_eq!(&actual, body, "content mismatch at {}", restored.display());
        // tar stores mtime at second resolution; allow ±1000 ms slack.
        let restored_mtime = mtime_ms(&restored);
        let diff = (restored_mtime - mtime_ms_expected).abs();
        assert!(
            diff <= 1000,
            "mtime drift {} ms at {} (expected {}, got {})",
            diff,
            restored.display(),
            mtime_ms_expected,
            restored_mtime,
        );
    }
}

/// #575 deterministic-failure semantic: when a worker can't write a
/// file (here because a path component is occupied by a regular file
/// that blocks `create_dir_all`), `load()` must return Err and the
/// error must mention the offending path so operators can repro it.
/// First-error-wins: subsequent workers are allowed to drain quietly,
/// but the returned error must come from a real failure, not be
/// swallowed.
#[test]
fn parallel_extract_surfaces_worker_errors() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let archive = dir.path().join("snap.tar.zst");
    let restore = dir.path().join("restore");

    // Make an archive that puts files under cache/conflicted/<n>.bin
    fs::create_dir_all(&cache).unwrap();
    for n in 0..32u32 {
        write(&cache.join(format!("conflicted/file{:02}.bin", n)), b"x");
    }

    save(&SaveOptions {
        workspace: None,
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    // Pre-create `restore/conflicted` as a REGULAR FILE — any worker that
    // tries to create_dir_all underneath this path fails deterministically
    // with NotADirectory / AlreadyExists. The first failure must surface
    // via load()'s Err return.
    fs::create_dir_all(&restore).unwrap();
    fs::write(restore.join("conflicted"), b"blocker").unwrap();

    let err = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&restore),
        workspace: None,
        threads: Some(2),
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect_err("worker failure must propagate up");
    let msg = err.to_string();
    assert!(
        msg.contains("conflicted"),
        "error must reference the blocking path: {msg}",
    );
}

/// `profile_extract: true` is non-fatal and a no-op for correctness:
/// the load report and file contents must match what a normal load
/// would produce. We can't easily assert the exact stderr line in this
/// test runner (cargo captures it), but we can assert load() doesn't
/// panic / error / corrupt output when the flag is on. (#575 phase 2)
#[test]
fn profile_extract_flag_does_not_break_load() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    fs::create_dir_all(&cache).unwrap();
    let f = cache.join("entry.bin");
    fs::write(&f, b"hello-profile").unwrap();

    let archive = tmp.path().join("a.tar.zst");
    let _ = save(&SaveOptions {
        workspace: None,
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: 1,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .unwrap();

    let restore = tmp.path().join("restore");
    let report = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&restore),
        workspace: None,
        threads: None,
        mtimes_only: false,
        profile_extract: true,
        auto_defender_exclude: false,
    })
    .unwrap();

    assert_eq!(report.cache_files_restored, 1);
    assert_eq!(
        fs::read(restore.join("entry.bin")).unwrap(),
        b"hello-profile"
    );
}

// #1541 — nanosecond mtime fidelity. The manifest records cache-file
// mtimes at nanosecond precision and `load` must restore them exactly
// (tar headers only carry seconds; the manifest is the authority).
// Regression guard for the manifest-driven metadata application: the
// restored file's sub-second nanos must match the saved value, not be
// truncated to tar's second resolution.
#[test]
fn cache_mtimes_restore_nanosecond_precision() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let archive = dir.path().join("snap.tar.zst");
    let restore = dir.path().join("restore");

    let rel = "deps/nanos.rlib";
    write(&cache.join(rel), b"nanosecond-payload");
    // Odd sub-second component so second-truncation is detectable.
    let want = filetime::FileTime::from_unix_time(1_700_000_123, 123_456_789);
    filetime::set_file_times(cache.join(rel), want, want).unwrap();
    let on_disk = fs::metadata(cache.join(rel)).unwrap().modified().unwrap();
    let on_disk_nanos = on_disk.duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    // Filesystems without nanosecond mtimes can't host this assertion.
    if on_disk_nanos != 123_456_789 {
        eprintln!("skipping: filesystem lacks nanosecond mtime support ({on_disk_nanos})");
        return;
    }

    save(&SaveOptions {
        workspace: None,
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&restore),
        workspace: None,
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect("load ok");

    let restored = fs::metadata(restore.join(rel))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    assert_eq!(restored.as_secs(), 1_700_000_123, "seconds must round-trip");
    assert_eq!(
        restored.subsec_nanos(),
        123_456_789,
        "nanosecond mtime component must round-trip exactly"
    );
}

// #1541 — the manifest's per-cache-file blake3 must equal the hash of
// the bytes actually archived, so a later `save --delta-from-manifest`
// compares against reality. Guards the hash-through-tar save path.
#[test]
fn manifest_cache_hashes_match_archived_payload() {
    let (_g, ws, cache, archive) = fixture();
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    let manifest = read_manifest_from_archive(&archive).expect("manifest");
    assert_eq!(manifest.cache_files.len(), 4);

    // Walk the archive itself and hash each cache/ entry body.
    let file = fs::File::open(&archive).unwrap();
    let reader = std::io::BufReader::new(file);
    let zstd = zstd::stream::read::Decoder::new(reader).unwrap();
    let mut tar = tar::Archive::new(zstd);
    let mut seen = 0usize;
    for entry in tar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().replace('\\', "/");
        let Some(rel) = path.strip_prefix("cache/").map(str::to_owned) else {
            continue;
        };
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut body).unwrap();
        let manifest_entry = manifest
            .cache_files
            .iter()
            .find(|e| e.path == rel)
            .unwrap_or_else(|| panic!("archived entry {rel} missing from manifest"));
        assert_eq!(manifest_entry.size, body.len() as u64, "size at {rel}");
        let hash = blake3_of(&body);
        assert_eq!(
            manifest_entry.blake3, hash,
            "manifest blake3 must match archived bytes at {rel}"
        );
        seen += 1;
    }
    assert_eq!(seen, 4, "all cache payloads must be visited");
}

fn blake3_of(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = zccache::hash::StreamHasher::new();
    hasher.update(bytes);
    hasher.finalize().as_bytes().to_vec()
}

// #1541 — corruption detection: a truncated archive must fail the load
// loudly instead of silently restoring a partial cache.
#[test]
fn truncated_archive_load_fails_loudly() {
    let (_g, ws, cache, archive) = fixture();
    // Add a larger entry so truncation lands mid-payload.
    write(&cache.join("deps/large.bin"), &vec![0x5A; 512 * 1024]);
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .expect("save ok");

    let bytes = fs::read(&archive).unwrap();
    assert!(
        bytes.len() > 64,
        "archive too small to truncate meaningfully"
    );
    fs::write(&archive, &bytes[..bytes.len() * 3 / 5]).unwrap();

    let restore = _g.path().join("restore");
    let err = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&restore),
        workspace: Some(&ws),
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .expect_err("truncated archive must fail the load");
    let msg = err.to_string();
    assert!(!msg.is_empty(), "error must carry a message");
}

/// soldr#587: an executable file (0o755) restored from cache must
/// still be executable. Without the per-worker chmod that fix
/// landed, cargo build-script-build files lose +x and fail execve
/// with EACCES on the next warm build.
#[test]
fn load_restores_executable_bit_for_cache_files() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    fs::create_dir_all(&cache).unwrap();
    let exe = cache.join("build-script-build");
    fs::write(&exe, b"#!/bin/sh\necho run\n").unwrap();
    soldr_platform::fs::permissions::make_executable(&exe).unwrap();

    let archive = tmp.path().join("a.tar.zst");
    let _ = save(&SaveOptions {
        workspace: None,
        cache_dir: Some(&cache),
        out: &archive,
        zstd_level: 1,
        threads: None,
        mtimes_only: false,
        profile: SaveProfile::Full,
    })
    .unwrap();

    let restore = tmp.path().join("restore");
    let _ = load(&LoadOptions {
        archive: &archive,
        cache_dir: Some(&restore),
        workspace: None,
        threads: None,
        mtimes_only: false,
        profile_extract: false,
        auto_defender_exclude: false,
    })
    .unwrap();

    let restored = restore.join("build-script-build");
    let mode = soldr_platform::fs::permissions::mode(&restored).expect("restored mode") & 0o777;
    assert_eq!(
        mode, 0o755,
        "restored mode {:o} != 0o755 — +x bit not preserved (regression of #587)",
        mode
    );
}

// `tests/*.rs` is one test binary per file, so the split module lives in a
// subdirectory (which cargo does not treat as a target) and is reached with
// an explicit `#[path]`.
#[path = "../save_roundtrip/symlinks.rs"]
mod symlinks;
