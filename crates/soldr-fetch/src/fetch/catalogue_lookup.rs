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

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
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
    let entry = get_or_fetch()
        .await
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
    let refreshed = match get_or_fetch().await.source {
        // Once a v2 generation supplied the pin, only a refreshed v2
        // generation may replace it.  Never turn a CDN mismatch into a v1
        // request, which could mix publication generations.
        CatalogueSource::CanonicalV2 => {
            fetch_v2_index_from(&cache_busted_url(&resolve_catalogue_v2_url())).await?
        }
        CatalogueSource::LegacyV1 => {
            fetch_v1_index_from(&cache_busted_url(&resolve_catalogue_url())).await?
        }
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

/// Parsed catalogues, keyed by the configuration that produced them.
///
/// soldr#2951: this used to be a bare `OnceLock<ManifestIndex>`, which cached
/// *the first catalogue any caller asked for* and then served it to every
/// later caller regardless of which URL they asked about. An explicit
/// `SOLDR_TOOLCHAIN_CATALOGUE_URL` set after the first fetch was silently
/// discarded, and so was `SOLDR_TOOLCHAIN_MANIFEST_DISABLE` -- the disable
/// check sat *after* the cache read, so a populated cache satisfied it.
/// A config knob that silently does nothing is the worst shape one can have,
/// because the caller cannot tell.
///
/// Keying on the resolved configuration fixes that, and incidentally removes
/// the reason the old design needed one process per test.
static MANIFEST_CACHE: OnceLock<Mutex<HashMap<String, &'static ManifestIndex>>> = OnceLock::new();

fn manifest_cache() -> &'static Mutex<HashMap<String, &'static ManifestIndex>> {
    MANIFEST_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The configuration a cached catalogue belongs to.
///
/// Disabled is its own key rather than a URL, because "disabled" is not a
/// place you can fetch from and must never be satisfied by a URL's entry.
fn cache_key() -> String {
    if disabled_via_env() {
        return "<disabled>".to_string();
    }
    if has_legacy_catalogue_url_override() {
        resolve_catalogue_url()
    } else {
        resolve_catalogue_v2_url()
    }
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
/// Returns a reference to the cached [`ManifestIndex`].
pub async fn get_or_fetch() -> &'static ManifestIndex {
    // soldr#2951: keyed on the resolved configuration, so a later caller with
    // a different `SOLDR_TOOLCHAIN_CATALOGUE_URL` -- or with the catalogue
    // disabled -- is not served the first caller's index.
    let key = cache_key();
    if let Some(cached) = manifest_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key.as_str())
    {
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
        match super::retry::with_backoff("the soldr-toolchain catalogue", fetch_once).await {
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
    // The index is leaked so the `&'static` return survives the lock guard.
    // Bounded by the number of *distinct* configurations a process uses,
    // which is one in every real run and a handful in tests. Concurrent
    // callers under a tokio runtime may each fetch and leak; the first
    // insertion wins and the others' copies are identical, which is the same
    // tradeoff the `OnceLock` made rather than pulling in
    // `tokio::sync::OnceCell` for this.
    let leaked: &'static ManifestIndex = Box::leak(Box::new(fetched));
    let mut cache = manifest_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache.entry(key).or_insert(leaked)
}

/// One-shot fetch of the catalogue, used by [`get_or_fetch`] on a
/// cache miss. soldr#988 Phase 5: only the v1 toolchain catalogue is
/// consulted — the legacy `manifest`-branch fallback was removed
/// along with its origin and refresh workflow. Failures degrade
/// silently to an empty index so the resolver falls through to the
/// live GitHub Releases API.
async fn fetch_once() -> Result<ManifestIndex, SoldrError> {
    // This override is intentionally *only* a v1 override.  Existing
    // air-gapped and test callers must not have their explicit URL
    // reinterpreted as a v2 endpoint.
    if has_legacy_catalogue_url_override() {
        return fetch_v1_index_from(&resolve_catalogue_url()).await;
    }
    let v2_url = resolve_catalogue_v2_url();
    let safe_v2_url = super::stream_download::safe_asset_url(&v2_url);
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, &v2_url),
        &safe_v2_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    match resp.status().as_u16() {
        status if should_fallback_to_v1(status) => {
            fetch_v1_index_from(&resolve_catalogue_url()).await
        }
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

// NOTE: there is still no `reset_cache_for_tests` helper, and it is no
// longer needed. The cache is keyed on the resolved configuration
// (soldr#2951), so a test that sets `SOLDR_TOOLCHAIN_CATALOGUE_URL` to its
// own one-shot server gets its own entry, and one that disables the
// catalogue gets the `<disabled>` entry -- in any binary, under any runner.
//
// The previous note said a fresh cache required "their own integration test
// binary (cargo's default is one binary per `tests/*.rs` file)". soldr#2934
// consolidated those per-file binaries into category targets and made that
// premise false, at which point `cargo test --test fetch_tools` began
// failing two of these tests. CI never saw it, because nextest runs every
// test in its own process. A contract that depends on the build layout is
// only as durable as the layout; this one now depends on the key instead.
//
// The unit tests below still operate entirely on the pure `from_json` /
// `lookup` APIs and never touch the process-wide cache.
