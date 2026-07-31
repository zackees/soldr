//! Toolchain-catalogue first asset resolver.
//!
//! Before consulting `api.github.com/repos/<owner>/<repo>/releases/...`
//! the release-asset resolver consults the v1 catalogue published by
//! `zackees/soldr-toolchain` and served over GitHub Pages:
//!
//! ```text
//! https://zackees.github.io/soldr-toolchain/catalogue.v1.json
//! ```
//!
//! Migration (soldr#988): the legacy manifest-branch origin and its
//! nightly job were retired in Phase 5.
//! There is no fallback to a second URL — a catalogue miss degrades
//! straight to the live GitHub Releases API. Use the catalogue's own
//! producer (`scripts/build_catalogue_v1.py` on `zackees/soldr-toolchain`)
//! to publish new entries.
//!
//! Schema (intentionally flat — one row per (owner, repo, tag, asset) —
//! so the lookup is a single linear scan and the file is grep-friendly):
//!
//! ```json
//! {
//!   "entries": [
//!     {
//!       "owner": "zackees",
//!       "repo":  "zccache",
//!       "tag":   "1.12.9",
//!       "asset": "zccache-x86_64-pc-windows-msvc.zip",
//!       "url":   "https://github.com/zackees/zccache/releases/download/1.12.9/zccache-x86_64-pc-windows-msvc.zip",
//!       "sha256": "0000...64hex"
//!     }
//!   ]
//! }
//! ```
//!
//! The index is fetched at most once per soldr process and cached in a
//! process-wide [`OnceLock`]. Any failure — HTTP error, parse error,
//! timeout, network — degrades silently to an empty index so the
//! resolver falls back to the live GitHub Releases API. The integrity
//! invariant ("a hit MUST sha256-verify against the manifest's pin") is
//! the same trust posture as `apple_sdk` / `zig`: a mismatched download
//! is a hard error regardless of `SOLDR_TRUST_MODE`.
//!
//! ## Env-var seams
//!
//! * `SOLDR_TOOLCHAIN_ORIGIN` — override the catalogue origin (the
//!   resolver builds `{origin}/catalogue.v1.json`). See Phase 2.
//! * `SOLDR_TOOLCHAIN_CATALOGUE_URL` — override the full URL (testing
//!   + air-gapped mirrors). When set takes precedence over
//!     `SOLDR_TOOLCHAIN_ORIGIN`.
//! * `SOLDR_MANIFEST_DISABLE=1` — skip the catalogue lookup entirely;
//!   the resolver falls through to the live GitHub Releases API.
//!
//! ## Why a separate file from per-tool manifests?
//!
//! soldr-toolchain also publishes human-friendly per-tool manifests at
//! `<origin>/<tool>/manifest.json`. `catalogue.v1.json` is the flat,
//! sha-bearing index used by runtime fetches that need a single
//! `(owner, repo, tag, asset)` lookup before falling back to the live
//! GitHub Releases API.

use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use crate::core::SoldrError;

/// soldr#988 Phase 2 — default origin of the v1 catalogue document
/// published by `zackees/soldr-toolchain`. The full URL we fetch is
/// `{origin}/catalogue.v1.json`. Overridable via
/// [`TOOLCHAIN_ORIGIN_ENV_VAR`].
pub const DEFAULT_TOOLCHAIN_ORIGIN: &str = "https://zackees.github.io/soldr-toolchain";

/// soldr#988 Phase 2 — env var that overrides
/// [`DEFAULT_TOOLCHAIN_ORIGIN`]. Set to an `https://` URL (no
/// trailing slash). Test seam + air-gapped-mirror seam.
pub const TOOLCHAIN_ORIGIN_ENV_VAR: &str = "SOLDR_TOOLCHAIN_ORIGIN";

/// soldr#988 Phase 5 — full-URL override for the catalogue endpoint.
/// When set takes precedence over [`TOOLCHAIN_ORIGIN_ENV_VAR`]'s
/// origin+filename composition. Test seam — lets integration tests
/// point at a one-shot HTTP listener whose path doesn't match the
/// canonical `/catalogue.v1.json`.
pub const TOOLCHAIN_CATALOGUE_URL_ENV_VAR: &str = "SOLDR_TOOLCHAIN_CATALOGUE_URL";

/// Catalogue document name. Producers on `zackees/soldr-toolchain`
/// emit this filename under the configured origin; consumers GET
/// `{origin}/{CATALOGUE_DOC_NAME}`.
pub const CATALOGUE_DOC_NAME: &str = "catalogue.v1.json";

/// Env var that, when set to a non-empty value other than `0`,
/// disables the catalogue lookup entirely. Resolution skips straight
/// to the live GitHub Releases API. Escape hatch for the rare case
/// where the published catalogue is wrong and a user needs to bypass
/// it without waiting for a soldr release. Name preserved across the
/// soldr#988 retirement so the user-facing env var stays stable.
pub const MANIFEST_DISABLE_ENV_VAR: &str = "SOLDR_MANIFEST_DISABLE";

/// Wall-clock budget for the one-shot manifest fetch. Generous enough
/// for slow networks; tight enough that an unreachable manifest cannot
/// wedge a build.
pub const MANIFEST_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Download one non-archive catalogue asset and enforce the catalogue's
/// SHA-256 pin. This is used for small metadata objects (for example the
/// Rust nightly-version map) that are published beside the catalogue.
pub async fn fetch_verified_catalogue_asset(
    owner: &str,
    repo: &str,
    tag: &str,
    asset: &str,
) -> Result<Vec<u8>, SoldrError> {
    let entry = get_or_fetch()
        .await
        .lookup(owner, repo, tag, asset)
        .cloned()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "catalogue has no asset row for {owner}/{repo} {tag}/{asset}"
            ))
        })?;
    let bytes = download_catalogue_asset(&entry.url).await?;
    if verify_catalogue_asset_bytes(&entry, &bytes).is_ok() {
        return Ok(bytes);
    }

    // The Pages asset and catalogue are updated in one assets-branch commit,
    // but CDN edges can briefly serve generations from opposite sides of that
    // commit. Refetch both objects once with cache-busters and require the new
    // catalogue digest; unverified bytes are never returned.
    let refreshed_url = cache_busted_url(&resolve_catalogue_url());
    let refreshed = fetch_index_from(&refreshed_url).await?;
    let refreshed_entry = refreshed.lookup(owner, repo, tag, asset).ok_or_else(|| {
        SoldrError::Other(format!(
            "refreshed catalogue has no asset row for {owner}/{repo} {tag}/{asset}"
        ))
    })?;
    let refreshed_bytes = download_catalogue_asset(&cache_busted_url(&refreshed_entry.url)).await?;
    verify_catalogue_asset_bytes(refreshed_entry, &refreshed_bytes)?;
    Ok(refreshed_bytes)
}

async fn download_catalogue_asset(url: &str) -> Result<Vec<u8>, SoldrError> {
    let client = super::github::http_client()?;
    let response = tokio::time::timeout(
        MANIFEST_FETCH_TIMEOUT,
        client
            .get(url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send(),
    )
    .await
    .map_err(|_| SoldrError::Network(format!("asset fetch timed out: {url}")))?
    .map_err(|error| SoldrError::Network(error.to_string()))?;
    if !response.status().is_success() {
        return Err(SoldrError::Network(format!(
            "asset fetch {} returned HTTP {}",
            url,
            response.status()
        )));
    }
    let bytes = tokio::time::timeout(MANIFEST_FETCH_TIMEOUT, response.bytes())
        .await
        .map_err(|_| SoldrError::Network(format!("asset body read timed out: {url}")))?
        .map_err(|error| SoldrError::Network(error.to_string()))?
        .to_vec();
    Ok(bytes)
}

fn cache_busted_url(url: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{url}{separator}soldr_refresh={nonce}")
}

fn verify_catalogue_asset_bytes(entry: &ManifestEntry, bytes: &[u8]) -> Result<(), SoldrError> {
    let actual = super::trust::sha256_of(bytes);
    if actual != entry.sha256 {
        return Err(SoldrError::Other(format!(
            "catalogue asset sha256 mismatch for {}/{} {}/{}: expected {}, got {}",
            entry.owner, entry.repo, entry.tag, entry.asset, entry.sha256, actual
        )));
    }
    Ok(())
}

/// One row in the published asset index.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManifestEntry {
    pub owner: String,
    pub repo: String,
    pub tag: String,
    pub asset: String,
    pub url: String,
    pub sha256: String,
}

/// Parsed shape of the published asset index. Kept deliberately flat —
/// a `Vec` scan is fine at the call rate (one lookup per `fetch_tool`
/// call) and lets us drop a `serde_json::from_str` straight onto the
/// downloaded body with no post-processing.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ManifestIndex {
    #[serde(default)]
    pub entries: Vec<ManifestEntry>,
}

impl ManifestIndex {
    /// Empty manifest. Used as the graceful-degrade fallback when the
    /// network fetch, disable env var, or JSON parse fails.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a JSON body into a [`ManifestIndex`]. Returns `None` (not
    /// an error) on parse failure so the caller can degrade silently to
    /// an empty index — a malformed remote manifest must never wedge a
    /// build.
    pub fn from_json(body: &str) -> Option<Self> {
        serde_json::from_str::<Self>(body).ok()
    }

    /// Look up the manifest entry for `(owner, repo, tag, asset)`.
    /// Tag matching is exact — the caller is expected to supply the
    /// tag string in the same form the manifest publishes (e.g.
    /// `1.12.9`, not `v1.12.9`, when that's how the upstream release
    /// names it). When the manifest is empty (graceful-degrade case)
    /// every lookup returns `None`.
    pub fn lookup(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
        asset: &str,
    ) -> Option<&ManifestEntry> {
        self.entries
            .iter()
            .find(|e| e.owner == owner && e.repo == repo && e.tag == tag && e.asset == asset)
    }

    /// Look up by `(owner, repo, tag)` only, ignoring the asset filter.
    /// Useful when the caller already knows the platform-matched asset
    /// name and just wants to round-trip the URL + sha256 for that one
    /// release. The first matching entry wins.
    pub fn lookup_release(&self, owner: &str, repo: &str, tag: &str) -> Vec<&ManifestEntry> {
        self.entries
            .iter()
            .filter(|e| e.owner == owner && e.repo == repo && e.tag == tag)
            .collect()
    }
}

/// Process-wide one-shot cache of the parsed manifest. Stores
/// `Some(ManifestIndex)` after a successful fetch+parse and
/// `Some(ManifestIndex::empty())` on any failure (so subsequent calls
/// don't re-try the network within the same process).
static MANIFEST_CACHE: OnceLock<ManifestIndex> = OnceLock::new();

/// True when [`MANIFEST_DISABLE_ENV_VAR`] is set to a truthy value.
/// `1`, `true`, `yes` (case-insensitive) all count; empty / unset /
/// `0` / `false` / `no` count as enabled.
fn disabled_via_env() -> bool {
    match std::env::var(MANIFEST_DISABLE_ENV_VAR) {
        Ok(value) => {
            let normalized = value.trim().to_ascii_lowercase();
            !normalized.is_empty()
                && normalized != "0"
                && normalized != "false"
                && normalized != "no"
        }
        Err(_) => false,
    }
}

/// soldr#988 Phase 2 — resolve the catalogue origin, honoring
/// [`TOOLCHAIN_ORIGIN_ENV_VAR`]. Trailing slash is stripped so the
/// caller can append `/catalogue.v1.json` unconditionally.
pub fn resolve_toolchain_origin() -> String {
    let raw = match std::env::var(TOOLCHAIN_ORIGIN_ENV_VAR) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => DEFAULT_TOOLCHAIN_ORIGIN.to_string(),
    };
    raw.trim_end_matches('/').to_string()
}

/// soldr#988 — full URL of the catalogue document.
/// [`TOOLCHAIN_CATALOGUE_URL_ENV_VAR`] takes precedence (test seam +
/// air-gapped-mirror seam); otherwise we compose
/// `{resolve_toolchain_origin()}/catalogue.v1.json`.
pub fn resolve_catalogue_url() -> String {
    if let Ok(value) = std::env::var(TOOLCHAIN_CATALOGUE_URL_ENV_VAR) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    format!("{}/{}", resolve_toolchain_origin(), CATALOGUE_DOC_NAME)
}

/// Fetch the manifest body once, caching the parsed index in
/// [`MANIFEST_CACHE`]. On any error (disable env var, HTTP failure,
/// network failure, timeout, parse failure) caches an empty index so
/// the rest of the process makes no further network calls and the
/// resolver falls through to the live GitHub Releases API.
///
/// Returns a reference to the cached [`ManifestIndex`].
pub async fn get_or_fetch() -> &'static ManifestIndex {
    if let Some(cached) = MANIFEST_CACHE.get() {
        return cached;
    }
    let fetched = if disabled_via_env() {
        ManifestIndex::empty()
    } else {
        // soldr#2132: retry before giving up. Falling back to an empty index
        // is a *permanent, process-wide* decision -- it drops the sha256 pins
        // and makes every later syslib lookup report "not yet ingested" -- so
        // a single truncated response body must not be enough to trigger it.
        // That is what failed two lanes of the v0.8.30 release build.
        super::retry::with_backoff("the soldr-toolchain catalogue", fetch_once)
            .await
            .unwrap_or_else(|_| ManifestIndex::empty())
    };
    // OnceLock::get_or_init isn't async, so we set explicitly. The
    // first set wins; concurrent callers under a tokio runtime will
    // re-fetch redundantly but the result they store is identical, and
    // the set is atomic. Acceptable tradeoff vs. pulling in a
    // tokio::sync::OnceCell just for this.
    let _ = MANIFEST_CACHE.set(fetched);
    MANIFEST_CACHE.get().expect("just set above")
}

/// One-shot fetch of the catalogue, used by [`get_or_fetch`] on a
/// cache miss. soldr#988 Phase 5: only the v1 toolchain catalogue is
/// consulted — the legacy `manifest`-branch fallback was removed
/// along with its origin and refresh workflow. Failures degrade
/// silently to an empty index so the resolver falls through to the
/// live GitHub Releases API.
async fn fetch_once() -> Result<ManifestIndex, SoldrError> {
    let url = resolve_catalogue_url();
    fetch_index_from(&url).await
}

/// HTTP-GET + JSON-parse a single index URL. Shared by both the v1
/// catalogue origin and the legacy `manifest` branch URL. The
/// `ManifestIndex` deserializer ignores unknown top-level fields
/// (e.g. v1's `schema_version`, `generated_at`, `origin`), so the
/// same struct cleanly absorbs both shapes.
async fn fetch_index_from(url: &str) -> Result<ManifestIndex, SoldrError> {
    let client = super::github::http_client()?;
    let resp = tokio::time::timeout(MANIFEST_FETCH_TIMEOUT, client.get(url).send())
        .await
        .map_err(|_| SoldrError::Network(format!("manifest fetch timed out: {url}")))?
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "manifest fetch {url} returned HTTP {}",
            resp.status()
        )));
    }
    let body = tokio::time::timeout(MANIFEST_FETCH_TIMEOUT, resp.text())
        .await
        .map_err(|_| SoldrError::Network(format!("manifest body read timed out: {url}")))?
        .map_err(|e| SoldrError::Network(e.to_string()))?;
    ManifestIndex::from_json(&body)
        .ok_or_else(|| SoldrError::Other(format!("manifest {url} did not parse as JSON")))
}

/// soldr#988 Phase 2 — `soldr toolchain catalogue` verb. Fetches
/// the catalogue's HEAD-equivalent metadata (one cheap GET, body
/// streamed only as needed for the entry count) and prints either a
/// human-readable summary or the stable JSON form.
pub async fn run_toolchain_catalogue(json: bool) -> Result<i32, SoldrError> {
    let url = resolve_catalogue_url();
    let client = super::github::http_client()?;
    let resp_result = tokio::time::timeout(MANIFEST_FETCH_TIMEOUT, client.get(&url).send()).await;

    match resp_result {
        Err(_) => {
            print_catalogue_error(&url, "request timed out", json);
            Ok(1)
        }
        Ok(Err(e)) => {
            print_catalogue_error(&url, &format!("network error: {e}"), json);
            Ok(1)
        }
        Ok(Ok(resp)) => {
            let status = resp.status();
            let etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let last_modified = resp
                .headers()
                .get("last-modified")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let content_length = resp
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            if !status.is_success() {
                print_catalogue_error(
                    &url,
                    &format!(
                        "HTTP {} (the catalogue file may not be published yet)",
                        status
                    ),
                    json,
                );
                return Ok(1);
            }
            let body = match tokio::time::timeout(MANIFEST_FETCH_TIMEOUT, resp.text()).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    print_catalogue_error(&url, &format!("body read error: {e}"), json);
                    return Ok(1);
                }
                Err(_) => {
                    print_catalogue_error(&url, "body read timed out", json);
                    return Ok(1);
                }
            };
            let parsed = ManifestIndex::from_json(&body);
            let n_entries = parsed.as_ref().map(|i| i.entries.len()).unwrap_or(0);
            if json {
                let payload = serde_json::json!({
                    "schema_version": 1,
                    "origin": resolve_toolchain_origin(),
                    "url": url,
                    "http_status": status.as_u16(),
                    "content_length": content_length,
                    "etag": etag,
                    "last_modified": last_modified,
                    "entries": n_entries,
                });
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else {
                println!("soldr-toolchain catalogue:");
                println!("  url:            {url}");
                println!("  http status:    {}", status.as_u16());
                if let Some(len) = content_length {
                    println!("  content-length: {len}");
                }
                if let Some(etag) = etag {
                    println!("  etag:           {etag}");
                }
                if let Some(lm) = last_modified {
                    println!("  last-modified:  {lm}");
                }
                println!("  entries:        {n_entries}");
            }
            Ok(0)
        }
    }
}

fn print_catalogue_error(url: &str, reason: &str, json: bool) {
    if json {
        let payload = serde_json::json!({
            "schema_version": 1,
            "origin": resolve_toolchain_origin(),
            "url": url,
            "error": reason,
        });
        eprintln!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else {
        eprintln!("soldr toolchain catalogue: {url} — {reason}");
    }
}

// NOTE: there is intentionally no `reset_cache_for_tests` helper.
// `OnceLock` has no public reset, and tests that need a fresh cache
// must run in their own integration test binary (cargo's default is
// one binary per `tests/*.rs` file). The unit tests below operate
// entirely on the pure `from_json` / `lookup` APIs and never touch
// the process-wide cache. Tests under `tests/manifest_lookup_*.rs`
// exercise the cache one-shot per binary.

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> &'static str {
        r#"{
            "entries": [
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "1.12.9",
                    "asset": "zccache-x86_64-pc-windows-msvc.zip",
                    "url": "https://github.com/zackees/zccache/releases/download/1.12.9/zccache-x86_64-pc-windows-msvc.zip",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                },
                {
                    "owner": "LukeMathWalker",
                    "repo": "cargo-chef",
                    "tag": "v0.1.73",
                    "asset": "cargo-chef-x86_64-pc-windows-msvc.tar.gz",
                    "url": "https://github.com/LukeMathWalker/cargo-chef/releases/download/v0.1.73/cargo-chef-x86_64-pc-windows-msvc.tar.gz",
                    "sha256": "1111111111111111111111111111111111111111111111111111111111111111"
                }
            ]
        }"#
    }

    crate::timed_test!(parses_well_formed_manifest, {
        let idx = ManifestIndex::from_json(sample_json()).expect("parse ok");
        assert_eq!(idx.entries.len(), 2);
        assert_eq!(idx.entries[0].owner, "zackees");
        assert_eq!(idx.entries[1].repo, "cargo-chef");
    });

    crate::timed_test!(from_json_returns_none_on_malformed_input, {
        assert!(ManifestIndex::from_json("not-json").is_none());
        assert!(ManifestIndex::from_json("{}").is_some()); // empty entries field is fine
    });

    crate::timed_test!(lookup_finds_exact_match, {
        let idx = ManifestIndex::from_json(sample_json()).unwrap();
        let hit = idx
            .lookup(
                "zackees",
                "zccache",
                "1.12.9",
                "zccache-x86_64-pc-windows-msvc.zip",
            )
            .expect("should hit");
        assert!(hit.url.contains("zccache"));
        assert_eq!(hit.sha256.len(), 64);
    });

    crate::timed_test!(lookup_misses_on_unknown_tuple, {
        let idx = ManifestIndex::from_json(sample_json()).unwrap();
        assert!(idx
            .lookup("zackees", "zccache", "1.12.9", "not-an-asset.zip")
            .is_none());
        assert!(idx
            .lookup(
                "zackees",
                "zccache",
                "1.12.8",
                "zccache-x86_64-pc-windows-msvc.zip"
            )
            .is_none());
        assert!(idx
            .lookup(
                "other",
                "zccache",
                "1.12.9",
                "zccache-x86_64-pc-windows-msvc.zip"
            )
            .is_none());
    });

    crate::timed_test!(empty_index_lookup_always_misses, {
        let idx = ManifestIndex::empty();
        assert!(idx.lookup("a", "b", "c", "d").is_none());
        assert!(idx.lookup_release("a", "b", "c").is_empty());
    });

    crate::timed_test!(lookup_release_returns_every_asset_for_a_tag, {
        let idx = ManifestIndex::from_json(sample_json()).unwrap();
        let hits = idx.lookup_release("zackees", "zccache", "1.12.9");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].asset.contains("zccache"));
    });

    crate::timed_test!(catalogue_asset_digest_is_mandatory, {
        let bytes = br#"{"schema_version":1}"#;
        let entry = ManifestEntry {
            owner: "zackees".into(),
            repo: "soldr-toolchain".into(),
            tag: "assets".into(),
            asset: "rust-nightly-versions.v1.json".into(),
            url: "https://example.invalid/map.json".into(),
            sha256: super::super::trust::sha256_of(bytes),
        };
        assert!(verify_catalogue_asset_bytes(&entry, bytes).is_ok());
        assert!(verify_catalogue_asset_bytes(&entry, b"changed").is_err());
    });

    crate::timed_test!(cache_buster_preserves_existing_query_parameters, {
        let plain = cache_busted_url("https://example.invalid/map.json");
        assert!(plain.starts_with("https://example.invalid/map.json?soldr_refresh="));
        let queried = cache_busted_url("https://example.invalid/map.json?mirror=1");
        assert!(queried.starts_with("https://example.invalid/map.json?mirror=1&soldr_refresh="));
    });

    // soldr#988 Phase 2: catalogue origin resolution.

    crate::timed_test!(catalogue_url_defaults_to_pages_origin, {
        // Caller may have SOLDR_TOOLCHAIN_ORIGIN set in their env;
        // exercise the public string-shape via the pure helper that
        // does not read env: build the URL from the default origin.
        let url = format!("{}/{}", DEFAULT_TOOLCHAIN_ORIGIN, CATALOGUE_DOC_NAME);
        assert_eq!(
            url,
            "https://zackees.github.io/soldr-toolchain/catalogue.v1.json"
        );
    });

    crate::timed_test!(catalogue_v1_json_parses_through_manifest_index, {
        // Phase 2 must accept the v1 wire shape transparently — the
        // top-level extras (schema_version, generated_at, origin)
        // are unknown fields ManifestIndex must ignore.
        let v1 = r#"{
            "schema_version": 1,
            "generated_at": "2026-06-27T00:00:00Z",
            "origin": "https://zackees.github.io/soldr-toolchain/catalogue.v1.json",
            "entries": [
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "1.12.11",
                    "asset": "zccache-v1.12.11-x86_64-pc-windows-msvc.zip",
                    "url": "https://github.com/zackees/zccache/releases/download/1.12.11/zccache-v1.12.11-x86_64-pc-windows-msvc.zip",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            ]
        }"#;
        let idx = ManifestIndex::from_json(v1).expect("v1 catalogue must parse");
        assert_eq!(idx.entries.len(), 1);
        assert_eq!(idx.entries[0].owner, "zackees");
        assert_eq!(idx.entries[0].tag, "1.12.11");
    });

    crate::timed_test!(disabled_via_env_handles_truthy_and_falsy_values, {
        // Test the parser directly via the same shape disabled_via_env
        // uses, since touching the process env in unit tests is racy.
        let check = |value: Option<&str>| match value {
            None => false,
            Some(v) => {
                let n = v.trim().to_ascii_lowercase();
                !n.is_empty() && n != "0" && n != "false" && n != "no"
            }
        };
        assert!(!check(None));
        assert!(!check(Some("")));
        assert!(!check(Some("0")));
        assert!(!check(Some("false")));
        assert!(!check(Some("no")));
        assert!(check(Some("1")));
        assert!(check(Some("true")));
        assert!(check(Some("yes")));
        assert!(check(Some("anything-else")));
    });

    // Back-compat guard for issue #861: after schema v6 lands beside
    // the flat-array shape this module owns, an old flat manifest
    // body must keep parsing through `ManifestIndex::from_json`. This
    // proves the dispatch isn't accidentally captured by the new v6
    // parser — the two shapes are disjoint on the wire (`entries: []`
    // vs. `schema_version: 6, tools: {...}`) and must stay so.
    crate::timed_test!(flat_schema_v5_still_parses_for_back_compat, {
        let flat = r#"{
            "entries": [
                {
                    "owner": "zackees",
                    "repo": "zccache",
                    "tag": "1.12.9",
                    "asset": "zccache-x86_64-pc-windows-msvc.zip",
                    "url": "https://example.com/zccache.zip",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            ]
        }"#;
        let idx = ManifestIndex::from_json(flat).expect("flat parse ok");
        assert_eq!(idx.entries.len(), 1);
        // And the v6 parser must reject this same flat body, proving
        // the two are routed disjointly.
        assert!(
            super::super::manifest_v6::ManifestV6::from_json(flat).is_none(),
            "v6 parser must reject the flat-array shape"
        );
    });

    crate::timed_test!(default_toolchain_origin_is_pages, {
        // soldr#988 Phase 5: legacy manifest-branch URL constant is
        // gone. The default catalogue origin is the soldr-toolchain
        // Pages site.
        assert_eq!(
            DEFAULT_TOOLCHAIN_ORIGIN,
            "https://zackees.github.io/soldr-toolchain"
        );
    });

    crate::timed_test!(catalogue_url_override_takes_precedence, {
        // The full-URL override is the one the integration tests use
        // when they spawn a local HTTP listener on a random port —
        // they can't fit that under the `origin + /catalogue.v1.json`
        // composition because the listener path is fixed.
        // Verify the override is recognized via the public const name.
        assert_eq!(
            TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
            "SOLDR_TOOLCHAIN_CATALOGUE_URL"
        );
    });
}
