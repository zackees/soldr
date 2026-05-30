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

/// Stable "recipe hash" used as the authoritative key tuple component
/// for cross-repo `soldr cook` indexing (issue #578). Computed from
/// the workspace's content-defining files:
/// * `Cargo.lock`
/// * Every `Cargo.toml` under the workspace root (sorted by relative
///   path, `target/` and `.git/` excluded).
/// * `rust-toolchain.toml` `[toolchain] channel` if declared.
///
/// **Important:** PR 2's `soldr cook` records this hash via
/// `CookRecord`, and PR 3's cargo-front-door pre-flight queries by
/// it. Both sides MUST go through this single function so the
/// recorded key matches the lookup key.
///
/// Returns `None` when `Cargo.lock` is missing — pre-flight callers
/// short-circuit silently in that case.
pub fn compute_recipe_hash_proxy(manifest_dir: &Path) -> Option<[u8; 32]> {
    let lockfile = manifest_dir.join("Cargo.lock");
    let lock_bytes = std::fs::read(&lockfile).ok()?;
    let manifest_pairs = collect_manifest_hashes(manifest_dir).ok()?;

    let mut hasher = Sha256::new();
    hasher.update(b"soldr-cook-recipe-hash-v1\n");
    hasher.update(b"Cargo.lock\n");
    hasher.update((lock_bytes.len() as u64).to_le_bytes());
    hasher.update(&lock_bytes);
    for (rel, hex) in &manifest_pairs {
        hasher.update(rel.as_bytes());
        hasher.update(b"\n");
        hasher.update(hex.as_bytes());
        hasher.update(b"\n");
    }
    let channel = std::fs::read_to_string(manifest_dir.join("rust-toolchain.toml"))
        .ok()
        .and_then(|s| parse_toolchain_channel(&s));
    if let Some(channel) = channel {
        hasher.update(b"channel=");
        hasher.update(channel.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Some(out)
}

/// Walk `dir`, returning `(relative_path, sha256_hex)` for every
/// `Cargo.toml` outside of `target/`, `.git/`, `.soldr/`,
/// `node_modules/`. Sorted by relative path for determinism.
fn collect_manifest_hashes(dir: &Path) -> std::io::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    walk_for_manifests(dir, dir, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn walk_for_manifests(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, String)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if matches!(s.as_ref(), ".git" | "target" | ".soldr" | "node_modules") {
                continue;
            }
            walk_for_manifests(root, &path, out)?;
        } else if file_type.is_file() && entry.file_name() == std::ffi::OsStr::new("Cargo.toml") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path)?;
            let mut h = Sha256::new();
            h.update(&bytes);
            out.push((rel, hex_lower_short(&h.finalize().into())));
        }
    }
    Ok(())
}

/// Best-effort parse of `[toolchain] channel = "..."` from
/// `rust-toolchain.toml`. Avoids depending on `toml::from_str` so
/// the hash function stays infallible even when downstream parses
/// would fail (the daemon's view of the workspace is bytewise).
fn parse_toolchain_channel(text: &str) -> Option<String> {
    let mut in_toolchain = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_toolchain = trimmed == "[toolchain]";
            continue;
        }
        if !in_toolchain {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("channel") {
            let rest = rest.trim_start();
            let rest = rest.strip_prefix('=')?;
            let rest = rest.trim();
            let trimmed_value = rest.trim_matches(|c: char| c == '"' || c == '\'');
            if !trimmed_value.is_empty() {
                return Some(trimmed_value.to_string());
            }
        }
    }
    None
}

fn hex_lower_short(bytes: &[u8; 32]) -> String {
    hex_lower(bytes)
}

/// Re-hash the file at `path` and compare against `expected`. Used by
/// PR 3's pre-flight hydrate to integrity-check the artifact before
/// extracting (issue #578). On mismatch the caller is expected to move
/// the file to `<sha>.tar.zst.quarantine` and fall through to a normal
/// cargo build.
pub fn verify_sha256(path: &Path, expected: &[u8; 32]) -> Result<bool, RegistryError> {
    let (actual, _size) = hash_file(path)?;
    Ok(&actual == expected)
}

/// Counts returned by [`extract_skip_existing`] so the caller can
/// surface a deterministic summary.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExtractReport {
    /// Files written by this hydrate (path did not already exist).
    pub files_written: u64,
    /// Files left alone because the path already existed in `target/`.
    /// Hydration is additive — the user's build state is never
    /// overwritten.
    pub files_skipped: u64,
    /// Directory entries materialized when their parent ancestors
    /// were missing.
    pub dirs_created: u64,
}

/// Stream-extract `<sha256>.tar.zst` into `dest_root`, **skipping** any
/// file path that already exists. Hydration is additive: PR 3's
/// pre-flight runs this BEFORE cargo invokes rustc, and we must
/// never overwrite a fresher artifact the user already produced.
///
/// `dest_root` is expected to be the project's `target/` directory.
/// The archive entries (written by [`pack_cook_archive`]) start with
/// the profile name (`release/`, `debug/`, ...), so extracting at the
/// `target/` root puts files at the right paths.
pub fn extract_skip_existing(
    archive_path: &Path,
    dest_root: &Path,
) -> Result<ExtractReport, RegistryError> {
    let file = File::open(archive_path)?;
    let decoder = zstd::Decoder::new(file).map_err(io_err)?;
    let mut archive = tar::Archive::new(decoder);
    let mut report = ExtractReport::default();

    for entry in archive.entries().map_err(io_err)? {
        let mut entry = entry.map_err(io_err)?;
        let path_in_tar = entry.path().map_err(io_err)?.to_path_buf();
        if is_unsafe_tar_path(&path_in_tar) {
            // Defensive: refuse to extract above the destination root
            // even though our own packer never produces these.
            continue;
        }
        let dest = dest_root.join(&path_in_tar);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            if !dest.is_dir() {
                std::fs::create_dir_all(&dest)?;
                report.dirs_created = report.dirs_created.saturating_add(1);
            }
            continue;
        }
        if !entry_type.is_file() {
            // Skip symlinks, hardlinks, fifos, devices. Cook artifacts
            // only carry regular files + directories.
            continue;
        }
        if dest.exists() {
            report.files_skipped = report.files_skipped.saturating_add(1);
            continue;
        }
        if let Some(parent) = dest.parent() {
            if !parent.is_dir() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // tar's `unpack_in` would overwrite; we manually copy bytes
        // so the skip-existing invariant cannot be violated.
        let mut out = File::create(&dest)?;
        std::io::copy(&mut entry, &mut out)?;
        report.files_written = report.files_written.saturating_add(1);
    }
    Ok(report)
}

/// Move the corrupted artifact to `<sha>.tar.zst.quarantine` so a
/// subsequent CookLookup-driven re-cook can re-populate the slot
/// without confusing the operator. Best-effort: a rename failure is
/// surfaced but never blocks the cook flow — the on-disk garbage will
/// be GC'd by the auto-gc cook pass.
pub fn quarantine_artifact(path: &Path) -> Result<PathBuf, RegistryError> {
    let mut name = path
        .file_name()
        .map(|n| n.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("artifact"));
    name.push(".quarantine");
    let quarantine = path
        .parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(&name));
    std::fs::rename(path, &quarantine)?;
    Ok(quarantine)
}

/// Reject tar entries whose normalized path either has an absolute
/// root, walks above the destination via `..`, or is empty.
fn is_unsafe_tar_path(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return true;
    }
    if path.is_absolute() {
        return true;
    }
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => return true,
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return true,
            _ => {}
        }
    }
    false
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

    crate::timed_test!(verify_sha256_matches_packed_artifact, {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"hello\n");
        let cook = dir.path().join("cook");
        let packed = pack_cook_archive(&source, &cook).expect("pack");
        assert!(verify_sha256(&packed.path, &packed.sha256).expect("verify"));
        // Flip a byte → mismatch.
        let mut wrong = packed.sha256;
        wrong[0] ^= 0xFF;
        assert!(!verify_sha256(&packed.path, &wrong).expect("verify"));
    });

    crate::timed_test!(extract_round_trips_packed_bytes, {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"foo\n");
        write_file(&source.join("deps").join("libbar-def.rmeta"), b"bar\n");
        let cook = dir.path().join("cook");
        let packed = pack_cook_archive(&source, &cook).expect("pack");

        let dest = dir.path().join("target");
        std::fs::create_dir_all(&dest).unwrap();
        let report = extract_skip_existing(&packed.path, &dest).expect("extract");
        assert!(report.files_written >= 2, "expected files written");
        assert_eq!(report.files_skipped, 0);

        let restored_foo = std::fs::read(dest.join("release").join("deps").join("libfoo-abc.rlib"))
            .expect("read foo");
        assert_eq!(restored_foo, b"foo\n");
    });

    crate::timed_test!(extract_skips_existing_files, {
        let dir = TempDir::new().expect("tempdir");
        let source = dir.path().join("release");
        write_file(&source.join("deps").join("libfoo-abc.rlib"), b"FRESH\n");
        let cook = dir.path().join("cook");
        let packed = pack_cook_archive(&source, &cook).expect("pack");

        // Pre-populate the destination with user content that must NOT
        // be overwritten.
        let dest = dir.path().join("target");
        let user_path = dest.join("release").join("deps").join("libfoo-abc.rlib");
        write_file(&user_path, b"USER-OWNS-THIS\n");

        let report = extract_skip_existing(&packed.path, &dest).expect("extract");
        assert!(report.files_skipped >= 1, "expected skip count");
        let on_disk = std::fs::read(&user_path).expect("read");
        assert_eq!(
            on_disk, b"USER-OWNS-THIS\n",
            "must not overwrite user state"
        );
    });

    crate::timed_test!(quarantine_renames_artifact_in_place, {
        let dir = TempDir::new().expect("tempdir");
        let cook = dir.path().join("cook");
        std::fs::create_dir_all(&cook).unwrap();
        let art = cook.join("deadbeef.tar.zst");
        write_file(&art, b"corrupt bytes");
        let quarantined = quarantine_artifact(&art).expect("quarantine");
        assert!(!art.exists());
        assert!(quarantined.exists());
        assert!(quarantined
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".quarantine"));
    });

    crate::timed_test!(is_unsafe_tar_path_rejects_traversal, {
        assert!(is_unsafe_tar_path(Path::new("../etc/passwd")));
        assert!(is_unsafe_tar_path(Path::new("/etc/passwd")));
        assert!(is_unsafe_tar_path(Path::new("")));
        assert!(!is_unsafe_tar_path(Path::new("release/deps/libfoo.rlib")));
    });
}
