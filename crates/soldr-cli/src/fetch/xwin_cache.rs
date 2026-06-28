//! soldr#1012 PR 5 — xwin-cache materialization for the blessed
//! `*-pc-windows-msvc` cross-compile path.
//!
//! Loads the pre-compressed MSVC SDK bundle from the soldr-toolchain
//! catalogue (`https://zackees.github.io/soldr-toolchain/...`),
//! verifies sha256, extracts under `~/.soldr/sdk/<triple>/xwin/`, and
//! returns the local path. Cached after first call — subsequent
//! `soldr build --target *-pc-windows-msvc` invocations are
//! filesystem-only after the cache is warm.
//!
//! ## Architecture
//!
//! The catalogue ships one row per `(version, platform)`:
//!
//! * `xwin-cache/<date>/windows-x86_64-msvc/xwin-cache.tar.zst`
//! * `xwin-cache/<date>/windows-aarch64-msvc/xwin-cache.tar.zst`
//!   (soldr-toolchain PR #30 / soldr#1012 PR 3)
//!
//! Soldr's `Commands::Build` arm calls [`ensure_xwin_cache`] before
//! invoking cargo for those targets; the bundle is materialized,
//! `XWIN_CACHE_DIR` is exported so cargo-xwin (if anything still
//! invokes it under the hood) finds the cache locally instead of
//! triggering a fresh live download. cargo-xwin's documentation
//! formalizes that env var as the consumer-facing entry point.
//!
//! ## Pinned versions
//!
//! For now we ship hardcoded asset URLs + sha256s per target. This
//! mirrors the `apple_sdk.rs` pattern. A future refactor (#1010
//! Phase 4) routes everything through `catalogue.v1.json` resolution
//! at runtime, but the immediate goal of this PR is "win-arm64
//! works"; hardcoded sha256s are the minimum-blast-radius first cut.

use std::path::{Path, PathBuf};

use crate::core::{SoldrError, SoldrPaths};

use super::github::http_client;
use super::trust;

/// Pinned xwin-cache release date currently in the catalogue.
/// Bump when a refreshed bundle ships from soldr-toolchain forge
/// ingest.
pub const MANAGED_XWIN_CACHE_VERSION: &str = "2026-06-22";

const XWIN_CACHE_X86_64_URL: &str =
    "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/xwin-cache/2026-06-22/windows-x86_64-msvc/xwin-cache.tar.zst";
const XWIN_CACHE_X86_64_SHA256: &str =
    "33c04d8026d99dab4d66f39ddbd93d75f64c68063d4ba58e5450626524bf348d";

// soldr#1012 PR 3 ingests this row; sha256 is filled in by a follow-
// up commit after the forge dispatch lands the asset in the catalogue.
// Until then `ensure_xwin_cache(aarch64-pc-windows-msvc)` returns the
// not-yet-ingested error; callers (Commands::Build's blessed path)
// branch on that and fall through to the legacy cargo-xwin path.
const XWIN_CACHE_AARCH64_URL: &str =
    "https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/xwin-cache/2026-06-22/windows-aarch64-msvc/xwin-cache.tar.zst";
const XWIN_CACHE_AARCH64_SHA256: &str = "TBD-INGEST-PENDING";

/// Env var consumed by cargo-xwin to short-circuit its own download.
/// The blessed path sets this so anything else in the workflow that
/// invokes cargo-xwin transparently uses our materialized cache.
pub const XWIN_CACHE_DIR_ENV_VAR: &str = "XWIN_CACHE_DIR";

/// Resolve the local xwin-cache directory for `target`. Materializes
/// the bundle on first call; subsequent calls return the cached path
/// immediately.
pub async fn ensure_xwin_cache(
    paths: &SoldrPaths,
    target_triple: &str,
) -> Result<PathBuf, SoldrError> {
    let (url, expected_sha256) = match target_triple {
        "x86_64-pc-windows-msvc" => (XWIN_CACHE_X86_64_URL, XWIN_CACHE_X86_64_SHA256),
        "aarch64-pc-windows-msvc" => (XWIN_CACHE_AARCH64_URL, XWIN_CACHE_AARCH64_SHA256),
        _ => {
            return Err(SoldrError::UnsupportedPlatform(format!(
                "ensure_xwin_cache: no managed xwin bundle for triple {target_triple}; \
                 supported: x86_64-pc-windows-msvc, aarch64-pc-windows-msvc"
            )));
        }
    };

    if expected_sha256.starts_with("TBD") {
        return Err(SoldrError::Other(format!(
            "xwin-cache for {target_triple} is not yet ingested into the \
             soldr-toolchain catalogue (sha256 placeholder is \
             {expected_sha256:?}). The recipe was added in \
             soldr-toolchain#30 (soldr#1012 PR 3); forge dispatch + \
             ingest follow-up will fill in the real sha256. Until then \
             `Commands::Build` falls through to the legacy cargo-xwin \
             path for this target.\nExpected URL: {url}"
        )));
    }

    paths.ensure_dirs()?;
    let install_dir = paths
        .root
        .join("sdk")
        .join(target_triple)
        .join("xwin")
        .join(MANAGED_XWIN_CACHE_VERSION);
    let stamp = install_dir.join(".complete");
    let cache_dir = install_dir.join("xwin");

    if stamp.is_file() && cache_dir.is_dir() {
        return Ok(cache_dir);
    }

    eprintln!(
        "soldr: fetching xwin-cache v{MANAGED_XWIN_CACHE_VERSION} for {target_triple} from {url}..."
    );

    let client = http_client()?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "xwin-cache download failed: HTTP {}",
            resp.status()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| SoldrError::Network(e.to_string()))?;

    let digest = trust::sha256_of(&bytes);
    if digest != expected_sha256 {
        return Err(SoldrError::Other(format!(
            "xwin-cache sha256 mismatch for {target_triple}: \
             expected {expected_sha256}, got {digest} \
             (catalogue blob may have been replaced — refusing to extract)"
        )));
    }
    eprintln!(
        "soldr: trust: verified xwin-cache v{MANAGED_XWIN_CACHE_VERSION} \
         for {target_triple} sha256={digest}"
    );

    if install_dir.exists() {
        std::fs::remove_dir_all(&install_dir)?;
    }
    std::fs::create_dir_all(&install_dir)?;
    extract_tar_zst_tree(&bytes, &install_dir)?;

    if !cache_dir.is_dir() {
        return Err(SoldrError::Archive(format!(
            "xwin-cache extract did not produce expected directory {}",
            cache_dir.display()
        )));
    }

    std::fs::write(&stamp, MANAGED_XWIN_CACHE_VERSION)?;
    eprintln!(
        "soldr: extracted xwin-cache to {} (set XWIN_CACHE_DIR there)",
        cache_dir.display()
    );
    Ok(cache_dir)
}

fn extract_tar_zst_tree(data: &[u8], dest: &Path) -> Result<(), SoldrError> {
    let reader = std::io::Cursor::new(data);
    let zst = zstd::stream::read::Decoder::new(reader)
        .map_err(|e| SoldrError::Archive(format!("zstd decoder init: {e}")))?;
    let mut archive = tar::Archive::new(zst);
    archive
        .unpack(dest)
        .map_err(|e| SoldrError::Archive(format!("tar.zst unpack: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(constants_well_formed, {
        // Bare smoke: catch typos in the URLs / hex sha256s during refactor.
        for url in [XWIN_CACHE_X86_64_URL, XWIN_CACHE_AARCH64_URL] {
            assert!(url.starts_with("https://"), "URL must be HTTPS: {url}");
            assert!(
                url.ends_with(".tar.zst"),
                "URL must reference a .tar.zst bundle: {url}"
            );
            assert!(
                url.contains(MANAGED_XWIN_CACHE_VERSION),
                "URL must embed MANAGED_XWIN_CACHE_VERSION ({MANAGED_XWIN_CACHE_VERSION}): {url}"
            );
        }
        // x64 sha256 is real (catalogue row already exists).
        assert_eq!(XWIN_CACHE_X86_64_SHA256.len(), 64);
        assert!(XWIN_CACHE_X86_64_SHA256
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    });

    crate::timed_test!(aarch64_not_yet_ingested_returns_clear_error, {
        // Until the forge ingest cycle lands the arm64 row, this fetch
        // must produce a descriptive error and NOT a network fetch
        // against a 404 URL. The error message is the load-bearing
        // contract — `Commands::Build` keys on a "not yet ingested"
        // signal in the error string to decide whether to fall through
        // to the legacy path.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_xwin_cache(&paths, "aarch64-pc-windows-msvc"));
        let err = result.expect_err("arm64 must error until catalogue lands the row");
        let msg = err.to_string();
        assert!(
            msg.contains("not yet ingested") || msg.contains("TBD"),
            "expected 'not yet ingested' or 'TBD' in error, got: {msg}"
        );
    });

    crate::timed_test!(unsupported_target_yields_unsupported_platform, {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ensure_xwin_cache(&paths, "x86_64-unknown-linux-gnu"));
        let err = result.expect_err("non-msvc target must error");
        assert!(
            matches!(err, SoldrError::UnsupportedPlatform(_)),
            "expected UnsupportedPlatform, got: {err:?}"
        );
    });
}
