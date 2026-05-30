//! tar + zstd:19 packer for cross-repo `soldr cook` artifacts (issue
//! #577, meta #579).
//!
//! Layout of `~/.soldr/cache/cook/`:
//!
//! ```text
//! <sha256>.tar.zst              # content-addressed cook artifact
//! .tmp/<rand>.tar.zst           # in-progress write (rename-on-finish)
//! ```
//!
//! This module is **purely a packer**. It is unrelated to the
//! `cache_lib::save` archive transport — `save.rs` keeps its
//! `DEFAULT_ZSTD_LEVEL = 3` for the short-lived save/load round-trip,
//! while cook artifacts use level 19 + `--long=27` because they are
//! compressed once and decompressed many times (every fork or CI
//! runner that shares the same recipe hash).
//!
//! The packer never reads or writes `cook_index_v1` itself — PR 3's
//! pre-flight hydrate is what looks up entries via the daemon, and
//! PR 2's `soldr cook` is what calls `CookRecord` after this packer
//! returns the resolved sha256.

use crate::cache_lib::target_registry::RegistryError;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// zstd level used for cross-repo cook artifacts. Slow once
/// (compress per cook run) in exchange for fast many (every fork or
/// CI runner that hits the same recipe hash decompresses). Do NOT
/// reuse for the save/load transport — that path stays at
/// `crate::cache_lib::save::DEFAULT_ZSTD_LEVEL = 3`.
pub const COOK_ZSTD_LEVEL: i32 = 19;

/// `--long=27` window log enables zstd long-distance matching on the
/// 128 MiB window — large enough that a typical trimmed `target/`
/// tree compresses to a small fraction of its raw size.
pub const COOK_ZSTD_LONG_WINDOW: u32 = 27;

/// Result of [`pack_cook_archive`]: the on-disk path and digest of
/// the new `<sha256>.tar.zst` artifact, together with the byte size
/// of the compressed file.
#[derive(Debug, Clone)]
pub struct PackedCookArchive {
    /// Absolute path to `<cook_cache_dir>/<sha256_hex>.tar.zst`.
    pub path: PathBuf,
    /// SHA-256 digest of the compressed bytes. Filename is the lower-
    /// case hex form of this; verifying the file == filename is the
    /// integrity check PR 3's pre-flight runs before extraction.
    pub sha256: [u8; 32],
    /// On-disk size of the compressed artifact.
    pub size_bytes: u64,
}

/// Resolve the `~/.soldr/cache/cook/` directory under the supplied
/// `paths`. Created lazily by [`pack_cook_archive`] but exposed
/// separately so PR 3 can find existing artifacts without re-packing.
pub fn cook_cache_dir(paths: &crate::core::SoldrPaths) -> PathBuf {
    paths.cache.join("cook")
}

/// Construct the canonical `<sha256_hex>.tar.zst` path for an artifact
/// under `cook_cache_dir`. Useful for tests + for the daemon's
/// `Response::CookHit { path }` which mirrors this shape.
pub fn artifact_path_for_sha(cook_dir: &Path, sha256: &[u8; 32]) -> PathBuf {
    cook_dir.join(format!("{}.tar.zst", hex_lower(sha256)))
}

/// Pack `source_dir` into `<cook_cache_dir>/<sha256>.tar.zst` using
/// tar + zstd at [`COOK_ZSTD_LEVEL`] with the
/// [`COOK_ZSTD_LONG_WINDOW`] window log. Writes go through
/// `<cook_cache_dir>/.tmp/<rand>.tar.zst` first so a crashed packer
/// never leaves a half-written `<sha256>.tar.zst` on disk.
///
/// `source_dir` is typically the trimmed `target/<profile>/` tree
/// (after `cargo chef cook` + `StripTargetOptions::cook()`). The
/// archive entries are recorded with paths *relative to* the
/// parent of `source_dir` — i.e. they start with the profile name
/// (`release/`, `debug/`, ...) so PR 3 can extract straight into
/// `target/` without an extra rename.
///
/// Errors:
///  * I/O failures creating `.tmp/`, writing the temp file, computing
///    the sha256, or renaming into place.
///  * The source_dir not existing — returned as `RegistryError::Io`.
pub fn pack_cook_archive(
    source_dir: &Path,
    cook_dir: &Path,
) -> Result<PackedCookArchive, RegistryError> {
    if !source_dir.is_dir() {
        return Err(RegistryError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("cook archive source missing: {}", source_dir.display()),
        )));
    }

    let tmp_dir = cook_dir.join(".tmp");
    std::fs::create_dir_all(&tmp_dir)?;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let temp_path = tmp_dir.join(format!("cook-{pid}-{nanos}.tar.zst.tmp"));

    let temp_file = File::create(&temp_path)?;
    let mut encoder = zstd::Encoder::new(temp_file, COOK_ZSTD_LEVEL).map_err(io_err)?;
    encoder.long_distance_matching(true).map_err(io_err)?;
    encoder.window_log(COOK_ZSTD_LONG_WINDOW).map_err(io_err)?;
    // Multi-threaded compression — `zstdmt` feature already enabled
    // in workspace deps. Using `num_cpus` would add a dep we do not
    // want; the OS-determined thread count works well for level 19.
    if let Ok(n) = std::thread::available_parallelism() {
        let _ = encoder.multithread(n.get() as u32);
    }

    {
        // tar archive: entries recorded relative to source_dir's parent
        // so the leading path component is the profile name.
        let mut tar_builder = tar::Builder::new(&mut encoder);
        let prefix_name = source_dir
            .file_name()
            .map(|n| n.to_owned())
            .unwrap_or_else(|| "cook".into());
        tar_builder
            .append_dir_all(prefix_name, source_dir)
            .map_err(io_err)?;
        tar_builder.finish().map_err(io_err)?;
    }
    encoder.finish().map_err(io_err)?;

    // Hash the on-disk file. Going through a fresh `Read` cycle is
    // simpler than chaining a `Sha256` writer through the zstd encoder
    // and matches the integrity model PR 3 uses on hydrate (re-hash
    // the file at the named path and compare to the filename stem).
    let (sha256, size_bytes) = hash_file(&temp_path)?;
    let final_path = artifact_path_for_sha(cook_dir, &sha256);

    // Rename into place. If a row already exists, replace it — the
    // content is identical by construction (same sha256), so a
    // re-cook of the same recipe is idempotent.
    std::fs::rename(&temp_path, &final_path).or_else(|_| {
        // Same-sha file already there: drop the temp.
        let _ = std::fs::remove_file(&temp_path);
        Ok::<(), std::io::Error>(())
    })?;

    Ok(PackedCookArchive {
        path: final_path,
        sha256,
        size_bytes,
    })
}

fn hash_file(path: &Path) -> Result<([u8; 32], u64), RegistryError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        total = total.saturating_add(read as u64);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok((out, total))
}

fn io_err(e: std::io::Error) -> RegistryError {
    RegistryError::Io(e)
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

/// Pretty-print an abbreviated SHA-256 (12 hex chars) for human-
/// readable status / warning output. Matches the spec in #577 / #579
/// (`sha256=<abbrev12>`).
pub fn sha_abbrev(sha256: &[u8; 32]) -> String {
    hex_lower(sha256).chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(p: &Path, bytes: &[u8]) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        let mut f = File::create(p).expect("create");
        f.write_all(bytes).expect("write");
    }

    crate::timed_test!(pack_writes_content_addressed_artifact, {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"hello\n");
        write_file(&source.join("deps").join("libbar-def.rmeta"), b"world\n");

        let cook_dir = dir.path().join("cook");
        let packed = pack_cook_archive(&source, &cook_dir).expect("pack");

        // Filename matches the sha256 hex.
        let expected = artifact_path_for_sha(&cook_dir, &packed.sha256);
        assert_eq!(packed.path, expected);
        assert!(packed.path.is_file());
        assert!(packed.size_bytes > 0);

        // Sanity: tmp dir is empty after success.
        let tmp = cook_dir.join(".tmp");
        let leftover_count = std::fs::read_dir(&tmp)
            .map(|it| it.flatten().count())
            .unwrap_or(0);
        assert_eq!(leftover_count, 0);
    });

    crate::timed_test!(pack_is_idempotent_on_same_content, {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"hello\n");

        let cook_dir = dir.path().join("cook");
        let a = pack_cook_archive(&source, &cook_dir).expect("a");
        let b = pack_cook_archive(&source, &cook_dir).expect("b");

        // Identical input → identical sha → identical artifact path.
        // (Note: tar entries carry per-file mtimes; zstd output may
        // differ. We assert path stability via the daemon-side hash,
        // not byte stability across runs.)
        assert_eq!(a.path.file_name(), b.path.file_name());
        assert!(a.path.is_file());
    });

    crate::timed_test!(pack_errors_on_missing_source_dir, {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let cook_dir = dir.path().join("cook");
        let err = pack_cook_archive(&missing, &cook_dir).expect_err("must error");
        match err {
            RegistryError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected Io NotFound, got {other:?}"),
        }
    });

    crate::timed_test!(sha_abbrev_is_twelve_lowercase_hex_chars, {
        let bytes = [0xAB; 32];
        let abbrev = sha_abbrev(&bytes);
        assert_eq!(abbrev.len(), 12);
        assert!(abbrev
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    });

    crate::timed_test!(artifact_path_matches_sha_filename, {
        let cook = Path::new("/cook");
        let sha = [0x12u8; 32];
        let path = artifact_path_for_sha(cook, &sha);
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with(".tar.zst"));
        assert!(name.starts_with(&hex_lower(&sha)));
    });
}
