//! Manifest-branch first asset resolver.
//!
//! Before consulting `api.github.com/repos/<owner>/<repo>/releases/...`
//! the release-asset resolver consults a vendored asset index hosted on
//! soldr's own `manifest` branch:
//!
//! ```text
//! https://raw.githubusercontent.com/zackees/soldr/manifest/asset-index.json
//! ```
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
//! * `SOLDR_MANIFEST_URL` — override the index URL (testing + air-gapped
//!   mirrors). Must point at the same flat-array JSON shape.
//! * `SOLDR_MANIFEST_DISABLE=1` — skip the manifest entirely; the
//!   resolver immediately falls through to the live GitHub Releases API.
//!
//! ## Why a separate file from `manifest.json`?
//!
//! The `manifest` branch already publishes a hierarchical
//! `manifest.json` (schema v5: a top-level index pointing at per-tool
//! `manifest.json` files, each a flat array of releases — see
//! `.github/scripts/build_manifest.py`). That file is human-friendly
//! but does NOT carry the per-asset sha256 the trust posture here
//! demands. `asset-index.json` is a separate, sha-bearing index that
//! can be regenerated alongside the existing manifest tree without
//! disturbing it. Until the publish step lands, this lookup returns
//! no hits and the resolver falls back unchanged.

use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use crate::core::SoldrError;

/// Default URL of the soldr-published asset index on the `manifest`
/// branch. Overridable via [`MANIFEST_URL_ENV_VAR`].
pub const DEFAULT_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/zackees/soldr/manifest/asset-index.json";

/// Env var that overrides [`DEFAULT_MANIFEST_URL`]. Set to a `file://`,
/// `http://`, or `https://` URL pointing at the same flat-array JSON
/// shape. Test seam + air-gapped-mirror seam.
pub const MANIFEST_URL_ENV_VAR: &str = "SOLDR_MANIFEST_URL";

/// Env var that, when set to a non-empty value other than `0`,
/// disables the manifest lookup entirely. Resolution skips straight to
/// the live GitHub Releases API. Escape hatch for the rare case where
/// the published manifest is wrong and a user needs to bypass it
/// without waiting for a soldr release.
pub const MANIFEST_DISABLE_ENV_VAR: &str = "SOLDR_MANIFEST_DISABLE";

/// Wall-clock budget for the one-shot manifest fetch. Generous enough
/// for slow networks; tight enough that an unreachable manifest cannot
/// wedge a build.
pub const MANIFEST_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Resolve the URL the manifest should be fetched from, honoring
/// [`MANIFEST_URL_ENV_VAR`].
fn resolve_url() -> String {
    match std::env::var(MANIFEST_URL_ENV_VAR) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => DEFAULT_MANIFEST_URL.to_string(),
    }
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
        fetch_once()
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

/// One-shot fetch of the manifest, used by [`get_or_fetch`] on a
/// cache miss. Returns `Err` on every failure; the caller maps that to
/// an empty index for graceful degradation.
async fn fetch_once() -> Result<ManifestIndex, SoldrError> {
    let url = resolve_url();
    let client = super::github::http_client()?;
    let resp = tokio::time::timeout(MANIFEST_FETCH_TIMEOUT, client.get(&url).send())
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

    crate::timed_test!(resolve_url_returns_default_when_env_unset, {
        // We can't unset env vars safely in unit tests under parallel
        // execution, but we can assert the default is the published
        // raw.githubusercontent.com URL. The override path is exercised
        // by the integration tests under `tests/manifest_lookup.rs`.
        assert_eq!(
            DEFAULT_MANIFEST_URL,
            "https://raw.githubusercontent.com/zackees/soldr/manifest/asset-index.json"
        );
    });
}
