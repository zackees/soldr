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
//! Resolved catalogue configurations are cached in a bounded, process-wide
//! LRU. A miss is single-flight, so concurrent callers share one fetch and a
//! configuration change cannot inherit another configuration's result. Any
//! failure degrades to an empty index so the resolver falls back to the live
//! GitHub Releases API. The integrity
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

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::catalogue_model::{
    authoritative_v2_index, bind_v2_publication_state, fail_closed_v2_index, CatalogueSource,
    ManifestIndex,
};
use super::catalogue_transport::{
    cache_busted_url, materialize_catalogue_entry, verify_catalogue_asset_sha256,
};
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
/// Canonical multipart-aware catalogue document, tried before v1.
pub const CATALOGUE_V2_DOC_NAME: &str = "catalogue.v2.json";
/// Explicit v2 endpoint override.  The legacy URL override remains v1-only.
pub const TOOLCHAIN_CATALOGUE_V2_URL_ENV_VAR: &str = "SOLDR_TOOLCHAIN_CATALOGUE_V2_URL";
/// Highest catalogue transport capability implemented by this Soldr build.
pub const CATALOGUE_CAPABILITY: u32 = 2;
pub const MAX_CATALOGUE_PARTS: usize = 4096;
pub const MAX_CATALOGUE_ASSET_BYTES: u64 = 8 * 1024 * 1024 * 1024 * 1024;
pub const MAX_CATALOGUE_PART_BYTES: u64 = 95 * 1024 * 1024;
pub const MAX_CATALOGUE_URL_BYTES: usize = 8192;

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
    let config = ResolvedCatalogueConfig::from_env();
    let initial = get_or_fetch_for(config.clone()).await;
    let entry = initial
        .lookup(owner, repo, tag, asset)
        .cloned()
        .ok_or_else(|| {
            SoldrError::Other(format!(
                "catalogue has no asset row for {owner}/{repo} {tag}/{asset}"
            ))
        })?;
    let downloaded = materialize_catalogue_entry(&entry).await?;
    if verify_catalogue_asset_sha256(&entry, downloaded.sha256()).is_ok() {
        return std::fs::read(downloaded.path()).map_err(SoldrError::from);
    }

    // The Pages asset and catalogue are updated in one assets-branch commit,
    // but CDN edges can briefly serve generations from opposite sides of that
    // commit. Refetch both objects once with cache-busters and require the new
    // catalogue digest; unverified bytes are never returned.
    let refreshed = match (&config, initial.source) {
        // Once a v2 generation supplied the pin, only a refreshed v2
        // generation may replace it.  Never turn a CDN mismatch into a v1
        // request, which could mix publication generations.
        (ResolvedCatalogueConfig::CanonicalV2 { v2_url, .. }, CatalogueSource::CanonicalV2) => {
            fetch_v2_index_from(&cache_busted_url(v2_url)).await?
        }
        (
            ResolvedCatalogueConfig::CanonicalV2 {
                fallback_v1_url, ..
            }
            | ResolvedCatalogueConfig::LegacyV1 {
                v1_url: fallback_v1_url,
            },
            CatalogueSource::LegacyV1,
        ) => fetch_v1_index_from(&cache_busted_url(fallback_v1_url)).await?,
        (ResolvedCatalogueConfig::Disabled, _) => {
            return Err(SoldrError::Other(
                "disabled catalogue cannot refresh a verified asset".to_string(),
            ));
        }
        (_, CatalogueSource::CanonicalV2) => unreachable!("only canonical config yields v2"),
    };
    let refreshed_entry = refreshed.lookup(owner, repo, tag, asset).ok_or_else(|| {
        SoldrError::Other(format!(
            "refreshed catalogue has no asset row for {owner}/{repo} {tag}/{asset}"
        ))
    })?;
    let refreshed = materialize_catalogue_entry(refreshed_entry).await?;
    verify_catalogue_asset_sha256(refreshed_entry, refreshed.sha256())?;
    std::fs::read(refreshed.path()).map_err(SoldrError::from)
}

/// Maximum number of resolved catalogue configurations retained per process.
///
/// A normal invocation uses one configuration; the small bound protects a
/// long-lived embedding from caller-selected URL growth without turning an
/// override into a one-shot setting.
const MANIFEST_CACHE_CAPACITY: usize = 8;

/// An immutable, complete catalogue decision captured from the process
/// environment once per operation.
///
/// The same value controls both cache identity and every request performed for
/// it. In particular, the canonical form keeps the v2 endpoint *and* its v1
/// fallback, so a concurrent environment change cannot make a cache entry for
/// one configuration fetch from another origin.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ResolvedCatalogueConfig {
    Disabled,
    LegacyV1 {
        v1_url: String,
    },
    CanonicalV2 {
        v2_url: String,
        fallback_v1_url: String,
    },
}

impl ResolvedCatalogueConfig {
    fn from_env() -> Self {
        if disabled_via_env() {
            Self::Disabled
        } else if has_legacy_catalogue_url_override() {
            Self::LegacyV1 {
                v1_url: resolve_catalogue_url(),
            }
        } else {
            Self::CanonicalV2 {
                v2_url: resolve_catalogue_v2_url(),
                fallback_v1_url: resolve_catalogue_url(),
            }
        }
    }
}

#[derive(Default)]
struct ManifestCache {
    /// Least-recently-used entry is at the front; most-recently-used at back.
    ready: VecDeque<(ResolvedCatalogueConfig, Arc<ManifestIndex>)>,
}

impl ManifestCache {
    fn get(&mut self, config: &ResolvedCatalogueConfig) -> Option<Arc<ManifestIndex>> {
        let position = self.ready.iter().position(|(key, _)| key == config)?;
        let entry = self
            .ready
            .remove(position)
            .expect("position came from the cache");
        let result = Arc::clone(&entry.1);
        self.ready.push_back(entry);
        Some(result)
    }

    fn insert(&mut self, config: ResolvedCatalogueConfig, index: Arc<ManifestIndex>) {
        if self.ready.len() == MANIFEST_CACHE_CAPACITY {
            self.ready.pop_front();
        }
        self.ready.push_back((config, index));
    }
}

/// Parsed catalogues, keyed by their immutable resolved configuration.
///
/// The async mutex deliberately stays held across the uncommon network fetch.
/// That makes a miss single-flight without Loading/Notify state, and the ready
/// cache is bounded so neither caller-selected URLs nor results leak for the
/// lifetime of a long-lived process.
static MANIFEST_CACHE: OnceLock<tokio::sync::Mutex<ManifestCache>> = OnceLock::new();

fn manifest_cache() -> &'static tokio::sync::Mutex<ManifestCache> {
    MANIFEST_CACHE.get_or_init(|| tokio::sync::Mutex::new(ManifestCache::default()))
}

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

fn has_legacy_catalogue_url_override() -> bool {
    std::env::var(TOOLCHAIN_CATALOGUE_URL_ENV_VAR)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

/// The canonical v2 endpoint; unlike the legacy override this must not
/// reinterpret an explicit v1 URL as a v2 document.
pub fn resolve_catalogue_v2_url() -> String {
    if let Ok(value) = std::env::var(TOOLCHAIN_CATALOGUE_V2_URL_ENV_VAR) {
        if !value.trim().is_empty() {
            return value.trim().to_string();
        }
    }
    format!("{}/{}", resolve_toolchain_origin(), CATALOGUE_V2_DOC_NAME)
}

/// Fetch the manifest body once, caching the parsed index in
/// [`MANIFEST_CACHE`]. On any error (disable env var, HTTP failure,
/// network failure, timeout, parse failure) caches an empty index so
/// the rest of the process makes no further network calls and the
/// resolver falls through to the live GitHub Releases API.
///
/// Returns the cached [`ManifestIndex`] for the configuration resolved at this
/// call boundary.
pub async fn get_or_fetch() -> Arc<ManifestIndex> {
    get_or_fetch_for(ResolvedCatalogueConfig::from_env()).await
}

async fn get_or_fetch_for(config: ResolvedCatalogueConfig) -> Arc<ManifestIndex> {
    // Hold one global async lock through the rare fetch. This intentionally
    // serializes distinct cold configurations too: it provides single-flight
    // behavior without a Loading/Notify state machine and the hot path remains
    // a bounded in-memory lookup.
    let mut cache = manifest_cache().lock().await;
    if let Some(cached) = cache.get(&config) {
        return cached;
    }

    let fetched = if matches!(config, ResolvedCatalogueConfig::Disabled) {
        ManifestIndex::empty()
    } else {
        // soldr#2132: retry before giving up. Falling back to an empty index
        // is a *permanent, process-wide* decision -- it drops the sha256 pins
        // and makes every later syslib lookup report "not yet ingested" -- so
        // a single truncated response body must not be enough to trigger it.
        // That is what failed two lanes of the v0.8.30 release build.
        let retry_config = config.clone();
        match super::retry::with_backoff("the soldr-toolchain catalogue", move || {
            let attempt_config = retry_config.clone();
            async move { fetch_once(&attempt_config).await }
        })
        .await
        {
            Ok(index) => index,
            Err(err) => {
                // soldr#2132 item 4. This used to be `.unwrap_or_else(|_|
                // empty())` -- the error was discarded and the process carried
                // on with no catalogue and no message. The consequence lands
                // much later and looks unrelated: syslib lookups report "not
                // yet ingested", and a missing sysroot surfaces as rustc's
                // `can't find crate for std`. Naming it here is the difference
                // between a two-minute diagnosis and an hour of one.
                eprintln!(
                    "soldr: warning: the soldr-toolchain catalogue is unavailable \
                     after {} attempts: {err}",
                    super::retry::FETCH_ATTEMPTS
                );
                eprintln!(
                    "soldr: warning: continuing without it. sha256 pins are \
                     unavailable for this run, and catalogue-provided sysroots \
                     and tools will not resolve -- which can surface later as an \
                     apparently unrelated compile error."
                );
                ManifestIndex::empty()
            }
        }
    };
    let index = Arc::new(fetched);
    cache.insert(config, Arc::clone(&index));
    index
}

/// One-shot fetch of the catalogue, used by [`get_or_fetch`] on a
/// cache miss. soldr#988 Phase 5: only the v1 toolchain catalogue is
/// consulted — the legacy `manifest`-branch fallback was removed
/// along with its origin and refresh workflow. Failures degrade
/// silently to an empty index so the resolver falls through to the
/// live GitHub Releases API.
async fn fetch_once(config: &ResolvedCatalogueConfig) -> Result<ManifestIndex, SoldrError> {
    // A legacy override is intentionally v1-only. The resolved URL is passed
    // in rather than read again so it cannot change after cache identity was
    // chosen.
    let (v2_url, fallback_v1_url) = match config {
        ResolvedCatalogueConfig::LegacyV1 { v1_url } => return fetch_v1_index_from(v1_url).await,
        ResolvedCatalogueConfig::CanonicalV2 {
            v2_url,
            fallback_v1_url,
        } => (v2_url, fallback_v1_url),
        ResolvedCatalogueConfig::Disabled => return Ok(ManifestIndex::empty()),
    };
    let safe_v2_url = super::stream_download::safe_asset_url(v2_url);
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, v2_url),
        &safe_v2_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    match resp.status().as_u16() {
        status if should_fallback_to_v1(status) => fetch_v1_index_from(fallback_v1_url).await,
        status if (200..300).contains(&status) => {
            let body = super::stream_download::read_control_text(
                resp,
                &safe_v2_url,
                MANIFEST_FETCH_TIMEOUT,
            )
            .await?;
            // A present v2 response is authoritative: malformed, unknown, or
            // semantically invalid data must never silently revive v1/live API.
            let parsed = ManifestIndex::from_v2_json(&body);
            let state_bound = parsed.is_some() && bind_v2_publication_state(&body).await.is_ok();
            Ok(authoritative_v2_index(parsed, state_bound))
        }
        _ => Ok(fail_closed_v2_index()),
    }
}

pub(super) fn should_fallback_to_v1(status: u16) -> bool {
    matches!(status, 404 | 410)
}

/// HTTP-GET + JSON-parse a single index URL. Shared by both the v1
/// catalogue origin and the legacy `manifest` branch URL. The
/// `ManifestIndex` deserializer ignores unknown top-level fields
/// (e.g. v1's `schema_version`, `generated_at`, `origin`), so the
/// same struct cleanly absorbs both shapes.
async fn fetch_v1_index_from(url: &str) -> Result<ManifestIndex, SoldrError> {
    let safe_url = super::stream_download::safe_asset_url(url);
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, url),
        &safe_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "manifest fetch {safe_url} returned HTTP {}",
            resp.status()
        )));
    }
    let body =
        super::stream_download::read_control_text(resp, &safe_url, MANIFEST_FETCH_TIMEOUT).await?;
    ManifestIndex::from_v1_json(&body)
        .ok_or_else(|| SoldrError::Other(format!("manifest {safe_url} did not parse as JSON")))
}

/// Fetch a canonical v2 document.  This never falls through to v1: it is
/// used after a v2-selected pin mismatch to keep catalogue and payload bound
/// to one publication generation.
async fn fetch_v2_index_from(url: &str) -> Result<ManifestIndex, SoldrError> {
    let safe_url = super::stream_download::safe_asset_url(url);
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, url),
        &safe_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "manifest fetch {safe_url} returned HTTP {}",
            resp.status()
        )));
    }
    let body =
        super::stream_download::read_control_text(resp, &safe_url, MANIFEST_FETCH_TIMEOUT).await?;
    let index = ManifestIndex::from_v2_json(&body).ok_or_else(|| {
        SoldrError::Other(format!("canonical v2 manifest {safe_url} did not validate"))
    })?;
    bind_v2_publication_state(&body).await?;
    Ok(index)
}

/// soldr#988 Phase 2 — `soldr toolchain catalogue` verb. Fetches
/// the catalogue's HEAD-equivalent metadata (one cheap GET, body
/// streamed only as needed for the entry count) and prints either a
/// human-readable summary or the stable JSON form.
pub async fn run_toolchain_catalogue(json: bool) -> Result<i32, SoldrError> {
    let url = resolve_catalogue_url();
    let safe_url = super::stream_download::safe_asset_url(&url);
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp_result = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, &url),
        &safe_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await;

    match resp_result {
        Err(e) => {
            print_catalogue_error(&safe_url, &format!("network error: {e}"), json);
            Ok(1)
        }
        Ok(resp) => {
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
                    &safe_url,
                    &format!(
                        "HTTP {} (the catalogue file may not be published yet)",
                        status
                    ),
                    json,
                );
                return Ok(1);
            }
            let body = match super::stream_download::read_control_text(
                resp,
                &safe_url,
                MANIFEST_FETCH_TIMEOUT,
            )
            .await
            {
                Ok(b) => b,
                Err(e) => {
                    print_catalogue_error(&safe_url, &format!("body read error: {e}"), json);
                    return Ok(1);
                }
            };
            let parsed = ManifestIndex::from_json(&body);
            let n_entries = parsed.as_ref().map(|i| i.entries.len()).unwrap_or(0);
            if json {
                let payload = serde_json::json!({
                    "schema_version": 1,
                    "origin": super::stream_download::safe_asset_url(&resolve_toolchain_origin()),
                    "url": safe_url,
                    "http_status": status.as_u16(),
                    "content_length": content_length,
                    "etag": etag,
                    "last_modified": last_modified,
                    "entries": n_entries,
                });
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else {
                println!("soldr-toolchain catalogue:");
                println!("  url:            {safe_url}");
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
            "origin": super::stream_download::safe_asset_url(&resolve_toolchain_origin()),
            "url": url,
            "error": reason,
        });
        eprintln!("{}", serde_json::to_string(&payload).unwrap_or_default());
    } else {
        eprintln!("soldr toolchain catalogue: {url} — {reason}");
    }
}

// Tests that mutate catalogue environment variables live in the consolidated
// fetch-tools target and serialize through a shared RAII scope. They exercise
// the production cache directly; no reset hook or process-layout assumption is
// required.

#[cfg(test)]
mod manifest_cache_tests {
    use super::*;

    fn config(number: usize) -> ResolvedCatalogueConfig {
        ResolvedCatalogueConfig::LegacyV1 {
            v1_url: format!("https://example.invalid/catalogue-{number}.json"),
        }
    }

    #[test]
    fn ready_lru_evicts_oldest_and_promotes_hits() {
        let mut cache = ManifestCache::default();
        for number in 0..=MANIFEST_CACHE_CAPACITY {
            cache.insert(config(number), Arc::new(ManifestIndex::empty()));
        }

        assert_eq!(cache.ready.len(), MANIFEST_CACHE_CAPACITY);
        assert!(
            cache.get(&config(0)).is_none(),
            "the ninth distinct configuration must evict the oldest ready entry"
        );

        // Config 1 is retained but currently oldest. A hit must promote it so
        // the next miss evicts config 2 instead.
        assert!(cache.get(&config(1)).is_some());
        cache.insert(
            config(MANIFEST_CACHE_CAPACITY + 1),
            Arc::new(ManifestIndex::empty()),
        );

        assert_eq!(cache.ready.len(), MANIFEST_CACHE_CAPACITY);
        assert!(
            cache.get(&config(1)).is_some(),
            "a ready hit must promote it"
        );
        assert!(
            cache.get(&config(2)).is_none(),
            "the untouched oldest entry must be evicted after promotion"
        );
    }
}
