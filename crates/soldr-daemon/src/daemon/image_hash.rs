//! Fingerprint-cached blake3 hashing of the daemon image (soldr#2442 / 0.9.0
//! cold-start perf).
//!
//! The broker verifies the `soldr-daemon` image before spawning it, and the
//! front door hashes the same image to register its identity. That image is a
//! large binary, and the cold path used to read it fully three-to-four times
//! per launch (`std::fs::read` + SHA-256 at every site) — ~19s under concurrent
//! cold I/O in a Docker VM. This module replaces that with:
//!
//! - **blake3** via zccache's shared, streaming hasher (`zccache::hash::hash_file`
//!   — no new dependency, and the whole binary is never loaded into RAM); and
//! - a **fingerprint cache** keyed on `(path, size, mtime)`: a prior hash of the
//!   exact same file (same size and mtime) is reused instead of re-reading the
//!   binary. A miss, a stale fingerprint, or any unreadable / malformed cache
//!   entry falls back to recomputation — the cache only ever *skips work*, it
//!   never returns a hash for content it did not read.
//!
//! The `(size, mtime)` freshness model matches how cargo and zccache already
//! decide a build artifact is unchanged. It is not an adversarial integrity
//! boundary — the daemon-image label the caller compares against remains the
//! boundary; this only accelerates producing the hash value on both sides.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// blake3 hex of `path`, streamed through zccache's shared hasher.
pub fn blake3_hex(path: &Path) -> io::Result<String> {
    Ok(zccache::hash::hash_file(path)?.to_hex())
}

/// `(size, mtime_nanos)` fingerprint used as the cache validity key.
fn fingerprint(path: &Path) -> io::Result<(u64, u128)> {
    let meta = std::fs::metadata(path)?;
    let mtime_nanos = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    Ok((meta.len(), mtime_nanos))
}

/// Cache filename for `target`: a blake3 of its absolute path, so distinct
/// binaries never collide and the name is filesystem-safe.
fn cache_entry_path(cache_dir: &Path, target: &Path) -> PathBuf {
    let abs = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let key = zccache::hash::hash_bytes(abs.to_string_lossy().as_bytes()).to_hex();
    cache_dir.join(format!("{key}.blake3"))
}

/// Return the blake3 hex digest of `target`, reusing a cached digest when the
/// file's `(size, mtime)` is unchanged since it was last hashed. Cache entries
/// live under `cache_dir` (created on demand). Any cache problem degrades to a
/// fresh [`blake3_hex`] — this never returns a digest for content it did not
/// actually hash in some run.
pub fn cached_blake3_hex(cache_dir: &Path, target: &Path) -> io::Result<String> {
    cached_blake3_hex_with_progress(cache_dir, target, |_, _| {})
}

/// [`cached_blake3_hex`] with byte-counted progress on a cache miss.
/// A cache hit performs no file scan and therefore emits no synthetic event.
pub fn cached_blake3_hex_with_progress(
    cache_dir: &Path,
    target: &Path,
    mut progress: impl FnMut(u64, u64),
) -> io::Result<String> {
    let (size, mtime) = fingerprint(target)?;
    let entry = cache_entry_path(cache_dir, target);

    if let Some(hex) = read_cache_entry(&entry, size, mtime) {
        return Ok(hex);
    }

    let mut file = File::open(target)?;
    let mut hasher = zccache::hash::StreamHasher::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut completed = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        completed = completed.saturating_add(read as u64);
        progress(completed, size);
    }
    let hex = hasher.finalize().to_hex();
    // Best-effort store; a cache write failure must never fail the hash.
    let _ = write_cache_entry(cache_dir, &entry, size, mtime, &hex);
    Ok(hex)
}

/// Parse a cache entry and return its digest iff the stored `(size, mtime)`
/// matches. Any I/O or parse problem returns `None` (recompute).
fn read_cache_entry(entry: &Path, size: u64, mtime: u128) -> Option<String> {
    let contents = std::fs::read_to_string(entry).ok()?;
    let mut parts = contents.trim().split('\t');
    let cached_size: u64 = parts.next()?.parse().ok()?;
    let cached_mtime: u128 = parts.next()?.parse().ok()?;
    let hex = parts.next()?;
    if cached_size == size
        && cached_mtime == mtime
        && hex.len() == 64
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Some(hex.to_string())
    } else {
        None
    }
}

/// Write a cache entry atomically (temp + rename) so a concurrent reader never
/// observes a half-written line.
fn write_cache_entry(
    cache_dir: &Path,
    entry: &Path,
    size: u64,
    mtime: u128,
    hex: &str,
) -> io::Result<()> {
    std::fs::create_dir_all(cache_dir)?;
    let tmp = entry.with_extension(format!("blake3.tmp.{}", std::process::id()));
    {
        let mut file = File::create(&tmp)?;
        write!(file, "{size}\t{mtime}\t{hex}")?;
        file.flush()?;
    }
    match std::fs::rename(&tmp, entry) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(blake3_hex_matches_zccache_reference, {
        let temp = tempfile::tempdir().expect("tempdir");
        let f = temp.path().join("bin");
        std::fs::write(&f, b"hello world").expect("write");
        let got = blake3_hex(&f).expect("hash");
        let expected = zccache::hash::hash_bytes(b"hello world").to_hex();
        assert_eq!(got, expected);
    });

    crate::timed_test!(cache_hit_returns_same_digest, {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = temp.path().join("cache");
        let f = temp.path().join("bin");
        std::fs::write(&f, b"payload-v1").expect("write");

        let first = cached_blake3_hex(&cache, &f).expect("first");
        let entries = std::fs::read_dir(&cache).expect("cache dir").count();
        assert_eq!(entries, 1, "one cache entry expected");
        let second = cached_blake3_hex(&cache, &f).expect("second");
        assert_eq!(first, second);
        assert_eq!(first, zccache::hash::hash_bytes(b"payload-v1").to_hex());
    });

    crate::timed_test!(cache_miss_reports_real_bytes_and_hit_stays_silent, {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = temp.path().join("cache");
        let file = temp.path().join("daemon");
        let contents = vec![0xa5_u8; 2 * 1024 * 1024 + 29];
        std::fs::write(&file, &contents).expect("write");
        let mut progress = Vec::new();

        let first = cached_blake3_hex_with_progress(&cache, &file, |done, total| {
            progress.push((done, total));
        })
        .expect("hash miss");
        assert!(progress.len() >= 2);
        assert_eq!(
            progress.last(),
            Some(&(contents.len() as u64, contents.len() as u64))
        );
        assert_eq!(first, zccache::hash::hash_bytes(&contents).to_hex());

        let mut hit_events = 0;
        let second = cached_blake3_hex_with_progress(&cache, &file, |_, _| hit_events += 1)
            .expect("hash hit");
        assert_eq!(second, first);
        assert_eq!(hit_events, 0, "a cache hit must not invent byte progress");
    });

    crate::timed_test!(changed_content_invalidates_cache, {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = temp.path().join("cache");
        let f = temp.path().join("bin");

        std::fs::write(&f, b"payload-v1").expect("write");
        let first = cached_blake3_hex(&cache, &f).expect("first");

        // Different length changes the size fingerprint; sleep so mtime also
        // advances past filesystem granularity.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, b"payload-v2-longer").expect("rewrite");
        let second = cached_blake3_hex(&cache, &f).expect("second");

        assert_ne!(first, second, "new content must produce a new digest");
        assert_eq!(
            second,
            zccache::hash::hash_bytes(b"payload-v2-longer").to_hex()
        );
    });

    crate::timed_test!(malformed_cached_digest_is_recomputed, {
        let temp = tempfile::tempdir().expect("tempdir");
        let cache = temp.path().join("cache");
        let file = temp.path().join("daemon");
        std::fs::write(&file, b"payload").expect("write");
        let (size, mtime) = fingerprint(&file).expect("fingerprint");
        let entry = cache_entry_path(&cache, &file);
        std::fs::create_dir_all(&cache).expect("cache dir");
        std::fs::write(&entry, format!("{size}\t{mtime}\tnot-a-digest")).expect("bad cache entry");

        let got = cached_blake3_hex(&cache, &file).expect("recomputed hash");
        assert_eq!(got, zccache::hash::hash_bytes(b"payload").to_hex());
    });
}
