//! Build script for `soldr-cli`.
//!
//! Two jobs in lockstep:
//!
//! 1. Refresh `crates/soldr-cli/embed/manifest.json` from the live
//!    asset-index on the `manifest` branch (issue #873 of meta #853).
//!    This converts the published schema-v5 flat shape into the v6
//!    nested shape that `manifest_v6::ManifestV6::lookup` consumes,
//!    so the embedded blob ships with current sha-bearing data and the
//!    runtime resolver never needs to touch the network for known tools.
//!    The refresh is best-effort — offline builds, CI sandboxes, and
//!    rate-limited fetches all silently fall back to whatever's already
//!    on disk. Set `SOLDR_SKIP_EMBED_REFRESH=1` to skip the fetch
//!    entirely (e.g. air-gapped CI that wants a reproducible build).
//!
//! 2. Compress the (possibly refreshed) `embed/manifest.json` into
//!    `${OUT_DIR}/manifest.json.zst` for `src/fetch/manifest_v6.rs` to
//!    `include_bytes!`.
//!
//! The pure v5→v6 conversion logic lives in `build_support/v5_to_v6.rs`
//! and is unit-tested via `tests/build_support_v5_to_v6.rs` so the
//! transformation can be exercised without a build-script integration
//! test.
//!
//! Re-run conditions:
//! - the JSON source changes
//! - this build script itself changes
//! - the `SOLDR_SKIP_EMBED_REFRESH` env-var changes

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[path = "build_support/v5_to_v6.rs"]
mod v5_to_v6;

/// URL of the v5 asset-index published on soldr's `manifest` branch.
/// Hardcoded — overriding the source-of-truth at build time would
/// silently fork the embedded blob from what the runtime fallback
/// fetches, which is exactly what this PR is trying to keep in sync.
const ASSET_INDEX_URL: &str =
    "https://raw.githubusercontent.com/zackees/soldr/manifest/asset-index.json";

/// Wall-clock budget for the build-time fetch. Generous enough for slow
/// networks but tight enough that a wedged fetch can't hold up `cargo
/// build` indefinitely.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Env-var escape hatch — set to a truthy value to skip the network
/// fetch entirely and use whatever `embed/manifest.json` is checked in
/// today. Designed for air-gapped CI and reproducible-build sandboxes.
const SKIP_REFRESH_ENV_VAR: &str = "SOLDR_SKIP_EMBED_REFRESH";

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

    let src = manifest_dir.join("embed").join("manifest.json");
    let dst = out_dir.join("manifest.json.zst");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_support/v5_to_v6.rs");
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo::rerun-if-env-changed={SKIP_REFRESH_ENV_VAR}");

    // Step 1: best-effort refresh of embed/manifest.json from the live
    // asset-index. Any failure logs a cargo:warning and proceeds with
    // whatever's on disk.
    if !skip_refresh_via_env() {
        match refresh_embedded_manifest(&src) {
            Ok(written) => {
                if written {
                    println!(
                        "cargo:warning=soldr-cli: refreshed embed/manifest.json from {ASSET_INDEX_URL}"
                    );
                } else {
                    println!(
                        "cargo:warning=soldr-cli: embed/manifest.json already in sync with {ASSET_INDEX_URL}"
                    );
                }
            }
            Err(e) => {
                println!(
                    "cargo:warning=soldr-cli: embed manifest refresh failed ({e}); using on-disk copy"
                );
            }
        }
    } else {
        println!(
            "cargo:warning=soldr-cli: {SKIP_REFRESH_ENV_VAR} set — skipping embed manifest refresh"
        );
    }

    // Step 2: compress the (possibly refreshed) JSON into OUT_DIR. This
    // step is mandatory — `manifest_v6.rs` `include_bytes!`s the result,
    // so a missing file is a hard build error.
    let json_bytes = std::fs::read(&src).unwrap_or_else(|e| panic!("read {}: {e}", src.display()));

    // zstd level 19 matches the level used elsewhere in the crate
    // (cache_lib::cook_archive::COOK_ZSTD_LEVEL, archive_cmd::ARCHIVE_ZSTD_LEVEL).
    // Decompression speed is what matters at runtime — the compression
    // ratio at 19 vs. 22 is negligible for sub-MB JSON.
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 19).expect("zstd encoder init");
    encoder.write_all(&json_bytes).expect("zstd compress write");
    let compressed = encoder.finish().expect("zstd compress finish");

    std::fs::write(&dst, &compressed).unwrap_or_else(|e| panic!("write {}: {e}", dst.display()));

    println!(
        "cargo:warning=soldr-cli: embedded manifest.json.zst built ({} bytes -> {} bytes)",
        json_bytes.len(),
        compressed.len()
    );
}

fn skip_refresh_via_env() -> bool {
    match std::env::var(SKIP_REFRESH_ENV_VAR) {
        Ok(v) => {
            let n = v.trim().to_ascii_lowercase();
            !n.is_empty() && n != "0" && n != "false" && n != "no"
        }
        Err(_) => false,
    }
}

/// Fetch the live v5 asset-index, convert to v6, and overwrite
/// `embed/manifest.json` if the result differs. Returns `Ok(true)` if
/// the file was rewritten, `Ok(false)` if the on-disk copy already
/// matched the refresh result. Errors are returned so the caller can
/// log + fall back gracefully — they MUST NOT propagate as a build
/// failure.
fn refresh_embedded_manifest(dst_json: &Path) -> Result<bool, String> {
    let v5_body = fetch_asset_index(ASSET_INDEX_URL)?;
    let v6_body = v5_to_v6::convert_v5_to_v6(&v5_body)?;

    let current = std::fs::read_to_string(dst_json).unwrap_or_default();
    // Compare normalized forms — `serde_json::Value`-roundtrip strips
    // incidental whitespace differences so a re-format of the on-disk
    // file doesn't trigger a rebuild loop.
    let normalize = |body: &str| -> Option<String> {
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        serde_json::to_string(&v).ok()
    };
    let normalized_current = normalize(&current);
    let normalized_new = normalize(&v6_body);
    if normalized_current.is_some() && normalized_current == normalized_new {
        return Ok(false);
    }

    if let Some(parent) = dst_json.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(dst_json, &v6_body).map_err(|e| format!("write {}: {e}", dst_json.display()))?;
    Ok(true)
}

/// Synchronous fetch via `ureq`. Returns the body as a String, or an
/// error message describing the failure mode.
fn fetch_asset_index(url: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(FETCH_TIMEOUT)
        .timeout_read(FETCH_TIMEOUT)
        .timeout_write(FETCH_TIMEOUT)
        .build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("HTTP fetch {url}: {e}"))?;
    if resp.status() < 200 || resp.status() >= 300 {
        return Err(format!("HTTP {url} returned status {}", resp.status()));
    }
    resp.into_string()
        .map_err(|e| format!("read body {url}: {e}"))
}
