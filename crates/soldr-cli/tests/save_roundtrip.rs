//! End-to-end save/load tests.
//!
//! Each test synthesizes a workspace + cache dir in a tempdir, calls
//! `save`, mutates the workspace to simulate a fresh `actions/checkout`
//! (mtimes pushed forward, sometimes content changed), then calls
//! `load` and asserts the right files got their mtimes restored.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use soldr_cli::cache_lib::save::{load, save, LoadOptions, SaveOptions, DEFAULT_ZSTD_LEVEL};

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
fn roundtrip_basic_mtime_restoration() {
    let (_g, ws, cache, archive) = fixture();

    // Snapshot the original mtimes so we can confirm restoration.
    let original_main_mtime = mtime_ms(&ws.join("src/main.rs"));

    let report = save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: &cache,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
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
        cache_dir: &cache,
        workspace: Some(&ws),
        threads: None,
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
fn load_skips_content_changed_files() {
    let (_g, ws, cache, archive) = fixture();
    save(&SaveOptions {
        workspace: Some(&ws),
        cache_dir: &cache,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
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
        cache_dir: &cache,
        workspace: Some(&ws),
        threads: None,
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
        cache_dir: &cache,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
    })
    .expect("save ok");

    fs::remove_file(ws.join("src/main.rs")).unwrap();

    let lreport = load(&LoadOptions {
        archive: &archive,
        cache_dir: &cache,
        workspace: Some(&ws),
        threads: None,
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
        cache_dir: &cache,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
    })
    .expect("save ok");

    // Make the file a different size from what was snapshotted.
    write(&ws.join("src/main.rs"), b"fn main() {} // appended\n");

    let lreport = load(&LoadOptions {
        archive: &archive,
        cache_dir: &cache,
        workspace: Some(&ws),
        threads: None,
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
        cache_dir: &cache,
        out: &archive,
        zstd_level: DEFAULT_ZSTD_LEVEL,
        threads: None,
    })
    .expect("save ok");
    fs::remove_dir_all(&cache).unwrap();
    let r = load(&LoadOptions {
        archive: &archive,
        cache_dir: &cache,
        workspace: None,
        threads: None,
    })
    .expect("load ok");
    assert_eq!(r.cache_files_restored, 4);
    assert_eq!(r.source_files_in_manifest, 0);
    assert_eq!(r.mtimes_applied, 0);
}
