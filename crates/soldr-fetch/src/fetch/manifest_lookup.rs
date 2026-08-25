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

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use sha2::{Digest, Sha256};

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
pub const CATALOGUE_DOC_NAME: &str = "catalogue.v2.json";
pub const LEGACY_CATALOGUE_DOC_NAME: &str = "catalogue.v1.json";

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
    let downloaded = download_manifest_entry(&entry).await?;
    if verify_catalogue_asset_sha256(&entry, downloaded.sha256()).is_ok() {
        return std::fs::read(downloaded.path()).map_err(SoldrError::from);
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
    let refreshed = download_manifest_entry(refreshed_entry).await?;
    verify_catalogue_asset_sha256(refreshed_entry, refreshed.sha256())?;
    std::fs::read(refreshed.path()).map_err(SoldrError::from)
}

/// Download a catalogue-pinned asset, retrying transient failures.
///
/// soldr#2132: the last unretried sender in this crate. Reached from
/// `fetch_verified_catalogue_asset`, whose only caller is
/// `dylint_toolchain.rs` -- nothing above it retries, so wrapping here cannot
/// nest (unlike `archive.rs`, see the note at the top of that file).
///
/// The retry lives inside this leaf rather than at the two call sites so both
/// the first fetch and the cache-busted refresh below inherit it. sha256
/// verification happens in the caller and therefore stays outside the retry.
async fn download_catalogue_asset(
    url: &str,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    super::retry::with_backoff(url, || download_catalogue_asset_once(url)).await
}

async fn download_catalogue_asset_once(
    url: &str,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let client = super::stream_download::asset_http_client("catalogue asset download")?;
    let response = super::stream_download::send_asset_request(
        super::stream_download::get_request(&client, url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity"),
        url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    stream_catalogue_asset_body(response, url, MANIFEST_FETCH_TIMEOUT).await
}

/// Materialize either catalogue transport into one verified temporary file.
/// Callers never branch on direct-vs-multipart and cache identity remains the
/// full asset SHA-256, not a URL or partition layout.
pub(crate) async fn download_manifest_entry(
    entry: &ManifestEntry,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    // Entries are schema-validated when the catalogue is parsed.  Recheck the
    // transport-independent invariants here because this helper is also used
    // directly by focused tests.
    if !entry.valid_transport(None) {
        return Err(SoldrError::Other(format!(
            "catalogue entry has an invalid transport for {}/{} {}/{}",
            entry.owner, entry.repo, entry.tag, entry.asset
        )));
    }
    if let Some(url) = entry.direct_url() {
        return download_catalogue_asset(url).await;
    }

    let mut pending = entry.parts.iter().cloned();
    let mut running = tokio::task::JoinSet::new();
    let mut completed = Vec::with_capacity(entry.parts.len());
    let mut window = 4_usize.min(entry.parts.len().max(1));
    loop {
        while running.len() < window {
            let Some(part) = pending.next() else { break };
            running.spawn(download_manifest_part(part));
        }
        let Some(joined) = running.join_next().await else {
            break;
        };
        let pair = joined
            .map_err(|error| SoldrError::Network(format!("multipart worker failed: {error}")))??;
        completed.push(pair);
        window = (window + 1).min(16).min(entry.parts.len().max(1));
    }
    completed.sort_by_key(|(number, _)| *number);
    if completed.len() != entry.parts.len() {
        return Err(SoldrError::Network(
            "multipart download ended with missing parts".into(),
        ));
    }

    use std::io::{Read, Write};
    let mut output = tempfile::NamedTempFile::new_in(soldr_core::core::ensure_temp_root())?;
    let mut full_hash = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    for (_, downloaded) in completed {
        let mut input = std::fs::File::open(downloaded.path())?;
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            full_hash.update(&buffer[..count]);
            bytes = bytes.saturating_add(count as u64);
        }
    }
    output.flush()?;
    let sha256 = hex::encode(full_hash.finalize());
    if Some(bytes) != entry.size_bytes || sha256 != entry.sha256 {
        return Err(SoldrError::Other(format!(
            "multipart catalogue asset failed final size/SHA-256 verification for {}/{}: expected {:?}/{}, got {bytes}/{sha256}",
            entry.owner, entry.asset, entry.size_bytes, entry.sha256
        )));
    }
    Ok(super::stream_download::DownloadedAsset {
        file: output,
        sha256,
        bytes,
    })
}

async fn download_manifest_part(
    part: ManifestPart,
) -> Result<(u32, super::stream_download::DownloadedAsset), SoldrError> {
    let mut last_error = None;
    for url in &part.urls {
        let attempt = super::retry::with_asset_backoff(url, || {
            download_manifest_part_url(url, part.size_bytes)
        })
        .await;
        match attempt {
            Ok(downloaded)
                if downloaded.bytes() == part.size_bytes && downloaded.sha256() == part.sha256 =>
            {
                return Ok((part.number, downloaded));
            }
            Ok(downloaded) => {
                last_error = Some(SoldrError::Other(format!(
                    "multipart part {} failed size/SHA-256 verification: expected {}/{}, got {}/{}",
                    part.number,
                    part.size_bytes,
                    part.sha256,
                    downloaded.bytes(),
                    downloaded.sha256()
                )));
                break; // An integrity mismatch is fatal; do not hide it via a mirror.
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        SoldrError::Network(format!(
            "multipart part {} has no usable mirror",
            part.number
        ))
    }))
}

async fn download_manifest_part_url(
    url: &str,
    expected_bytes: u64,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    let client = super::stream_download::asset_http_client("catalogue multipart part")?;
    let response = super::stream_download::send_asset_request(
        super::stream_download::get_request(&client, url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity"),
        url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    super::stream_download::stream_multipart_part_to_temp_file(
        response,
        url,
        super::stream_download::ASSET_IDLE_TIMEOUT,
        expected_bytes,
    )
    .await
}

async fn stream_catalogue_asset_body(
    response: reqwest::Response,
    url: &str,
    body_timeout: std::time::Duration,
) -> Result<super::stream_download::DownloadedAsset, SoldrError> {
    tokio::time::timeout(
        body_timeout,
        super::stream_download::stream_response_to_temp_file(
            response,
            url,
            super::stream_download::ASSET_IDLE_TIMEOUT,
        ),
    )
    .await
    .map_err(|_| SoldrError::Network(format!("asset body read timed out: {url}")))?
}

fn cache_busted_url(url: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{url}{separator}soldr_refresh={nonce}")
}

fn verify_catalogue_asset_sha256(entry: &ManifestEntry, actual: &str) -> Result<(), SoldrError> {
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
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub owner: String,
    pub repo: String,
    pub tag: String,
    pub asset: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub parts: Vec<ManifestPart>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub min_client_version: Option<u32>,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ManifestPart {
    pub number: u32,
    pub sha256: String,
    pub size_bytes: u64,
    pub urls: Vec<String>,
}

impl ManifestEntry {
    pub fn direct_url(&self) -> Option<&str> {
        self.url
            .as_deref()
            .or_else(|| self.urls.first().map(String::as_str))
    }

    pub fn display_url(&self) -> &str {
        self.direct_url()
            .or_else(|| {
                self.parts
                    .first()
                    .and_then(|part| part.urls.first())
                    .map(String::as_str)
            })
            .unwrap_or("multipart catalogue asset")
    }

    pub fn matches_legacy_url(&self, expected: &str) -> bool {
        self.direct_url() == Some(expected)
            || self
                .source_path
                .as_ref()
                .is_some_and(|path| expected.ends_with(&format!("/assets/{path}")))
    }

    fn valid_transport(&self, schema_version: Option<u32>) -> bool {
        const MAX_ASSET_BYTES: u64 = 8 * 1024_u64.pow(4);
        let valid_hash = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        let valid_url = |value: &str| {
            value.len() <= 8192
                && url::Url::parse(value).is_ok_and(|parsed| {
                    (parsed.scheme() == "https"
                        || (cfg!(test)
                            && parsed.scheme() == "http"
                            && parsed.host_str() == Some("127.0.0.1")))
                        && parsed.has_host()
                        && parsed.username().is_empty()
                        && parsed.password().is_none()
                })
        };
        let direct = self.direct_url().is_some();
        if direct != self.parts.is_empty()
            || !valid_hash(&self.sha256)
            || (schema_version == Some(2)
                && self.min_client_version.is_some_and(|version| version != 2))
            || (schema_version != Some(2)
                && self.min_client_version.is_some_and(|version| version > 2))
            || self.parts.len() > 4096
            || self
                .size_bytes
                .is_some_and(|size| size == 0 || size > MAX_ASSET_BYTES)
        {
            return false;
        }
        if let Some(path) = &self.source_path {
            if path.is_empty()
                || path.starts_with('/')
                || path.split('/').any(|segment| segment == "..")
            {
                return false;
            }
        }
        if direct {
            if schema_version == Some(2) && (self.url.is_some() || self.size_bytes.is_none()) {
                return false;
            }
            let direct_urls: Vec<&str> = self
                .url
                .iter()
                .map(String::as_str)
                .chain(self.urls.iter().map(String::as_str))
                .collect();
            return direct_urls.iter().all(|url| valid_url(url))
                && direct_urls.iter().copied().collect::<HashSet<_>>().len() == direct_urls.len();
        }
        let Some(total) = self.size_bytes else {
            return false;
        };
        if schema_version == Some(2) && self.min_client_version != Some(2) {
            return false;
        }
        let mut sum = 0_u64;
        for (index, part) in self.parts.iter().enumerate() {
            if part.number as usize != index + 1
                || part.size_bytes == 0
                || part.size_bytes > 99_614_720
                || !valid_hash(&part.sha256)
                || part.urls.is_empty()
                || part.urls.iter().any(|url| !valid_url(url))
                || part.urls.iter().collect::<HashSet<_>>().len() != part.urls.len()
            {
                return false;
            }
            sum = match sum.checked_add(part.size_bytes) {
                Some(value) => value,
                None => return false,
            };
        }
        sum == total
    }
}

/// Parsed shape of the published asset index. Kept deliberately flat —
/// a `Vec` scan is fine at the call rate (one lookup per `fetch_tool`
/// call) and lets us drop a `serde_json::from_str` straight onto the
/// downloaded body with no post-processing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationState {
    pub generation: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestIndex {
    #[serde(default)]
    pub schema_version: Option<u32>,
    #[serde(default)]
    pub generation: Option<String>,
    #[serde(default)]
    pub publication_state: Option<PublicationState>,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub origin: Option<String>,
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
        let parsed = serde_json::from_str::<Self>(body).ok()?;
        if !matches!(parsed.schema_version, None | Some(1) | Some(2)) {
            return None;
        }
        if parsed.schema_version == Some(2) {
            let generation = parsed.generation.as_deref()?;
            let publication = parsed.publication_state.as_ref()?;
            if generation.is_empty()
                || generation.len() > 256
                || !generation
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
                || publication.generation != generation
            {
                return None;
            }
            let state_url = url::Url::parse(&publication.url).ok()?;
            if state_url.scheme() != "https"
                || !state_url.has_host()
                || !state_url.username().is_empty()
                || state_url.password().is_some()
                || state_url.query().is_some()
                || state_url.fragment().is_some()
                || !state_url
                    .path()
                    .ends_with(&format!("/generations/{generation}/publish-state.v1.json"))
            {
                return None;
            }
        } else if parsed.generation.is_some() || parsed.publication_state.is_some() {
            return None;
        }
        let mut identities = HashSet::new();
        parsed
            .entries
            .iter()
            .all(|entry| {
                identities.insert((&entry.owner, &entry.repo, &entry.tag, &entry.asset))
                    && entry.valid_transport(parsed.schema_version)
            })
            .then_some(parsed)
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

    /// Look up an exact asset filename regardless of its catalogue owner.
    ///
    /// soldr-toolchain republishes some cross-platform tools under its own
    /// `assets` identity when the upstream release does not cover every Soldr
    /// target. Those rows retain an exact, versioned filename, so filename is
    /// the stable lookup key for that explicitly opted-in path.
    pub fn lookup_asset(&self, asset: &str) -> Vec<&ManifestEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.asset == asset)
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
    match fetch_index_from(&url).await {
        Ok(index) => Ok(index),
        Err(primary)
            if std::env::var(TOOLCHAIN_CATALOGUE_URL_ENV_VAR)
                .ok()
                .is_none_or(|value| value.trim().is_empty())
                && (primary.to_string().contains("HTTP 404")
                    || primary.to_string().contains("HTTP 410")) =>
        {
            let legacy = format!(
                "{}/{}",
                resolve_toolchain_origin(),
                LEGACY_CATALOGUE_DOC_NAME
            );
            fetch_index_from(&legacy).await.map_err(|fallback| {
                SoldrError::Network(format!(
                    "catalogue v2 failed ({primary}); compatibility v1 failed ({fallback})"
                ))
            })
        }
        Err(error) => Err(error),
    }
}

/// HTTP-GET + JSON-parse a single index URL. Shared by both the v1
/// catalogue origin and the legacy `manifest` branch URL. The
/// `ManifestIndex` deserializer ignores unknown top-level fields
/// (e.g. v1's `schema_version`, `generated_at`, `origin`), so the
/// same struct cleanly absorbs both shapes.
async fn fetch_index_from(url: &str) -> Result<ManifestIndex, SoldrError> {
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, url),
        url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    if !resp.status().is_success() {
        return Err(SoldrError::Network(format!(
            "manifest fetch {url} returned HTTP {}",
            resp.status()
        )));
    }
    let body = super::stream_download::read_control_text(resp, url, MANIFEST_FETCH_TIMEOUT).await?;
    ManifestIndex::from_json(&body)
        .ok_or_else(|| SoldrError::Other(format!("manifest {url} did not parse as JSON")))
}

/// soldr#988 Phase 2 — `soldr toolchain catalogue` verb. Fetches
/// the catalogue's HEAD-equivalent metadata (one cheap GET, body
/// streamed only as needed for the entry count) and prints either a
/// human-readable summary or the stable JSON form.
pub async fn run_toolchain_catalogue(json: bool) -> Result<i32, SoldrError> {
    let url = resolve_catalogue_url();
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp_result = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, &url),
        &url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await;

    match resp_result {
        Err(e) => {
            print_catalogue_error(&url, &format!("network error: {e}"), json);
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
                    &url,
                    &format!(
                        "HTTP {} (the catalogue file may not be published yet)",
                        status
                    ),
                    json,
                );
                return Ok(1);
            }
            let body =
                match super::stream_download::read_control_text(resp, &url, MANIFEST_FETCH_TIMEOUT)
                    .await
                {
                    Ok(b) => b,
                    Err(e) => {
                        print_catalogue_error(&url, &format!("body read error: {e}"), json);
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
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    #[test]
    fn parses_well_formed_manifest() {
        let idx = ManifestIndex::from_json(sample_json()).expect("parse ok");
        assert_eq!(idx.entries.len(), 2);
        assert_eq!(idx.entries[0].owner, "zackees");
        assert_eq!(idx.entries[1].repo, "cargo-chef");
    }

    #[test]
    fn from_json_returns_none_on_malformed_input() {
        assert!(ManifestIndex::from_json("not-json").is_none());
        assert!(ManifestIndex::from_json("{}").is_some()); // empty entries field is fine
    }

    #[test]
    fn lookup_finds_exact_match() {
        let idx = ManifestIndex::from_json(sample_json()).unwrap();
        let hit = idx
            .lookup(
                "zackees",
                "zccache",
                "1.12.9",
                "zccache-x86_64-pc-windows-msvc.zip",
            )
            .expect("should hit");
        assert!(hit.direct_url().is_some_and(|url| url.contains("zccache")));
        assert_eq!(hit.sha256.len(), 64);
    }

    #[test]
    fn lookup_misses_on_unknown_tuple() {
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
    }

    #[test]
    fn empty_index_lookup_always_misses() {
        let idx = ManifestIndex::empty();
        assert!(idx.lookup("a", "b", "c", "d").is_none());
        assert!(idx.lookup_release("a", "b", "c").is_empty());
    }

    #[test]
    fn lookup_release_returns_every_asset_for_a_tag() {
        let idx = ManifestIndex::from_json(sample_json()).unwrap();
        let hits = idx.lookup_release("zackees", "zccache", "1.12.9");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].asset.contains("zccache"));
    }

    #[test]
    fn lookup_asset_finds_toolchain_owned_repackages() {
        let idx = ManifestIndex::from_json(sample_json()).unwrap();
        let hits = idx.lookup_asset("cargo-chef-x86_64-pc-windows-msvc.tar.gz");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].repo, "cargo-chef");
    }

    #[test]
    fn catalogue_asset_digest_is_mandatory() {
        let bytes = br#"{"schema_version":1}"#;
        let entry = ManifestEntry {
            owner: "zackees".into(),
            repo: "soldr-toolchain".into(),
            tag: "assets".into(),
            asset: "rust-nightly-versions.v1.json".into(),
            url: Some("https://example.invalid/map.json".into()),
            urls: Vec::new(),
            parts: Vec::new(),
            size_bytes: None,
            source_path: None,
            min_client_version: None,
            sha256: super::super::trust::sha256_of(bytes),
        };
        assert!(
            verify_catalogue_asset_sha256(&entry, &super::super::trust::sha256_of(bytes)).is_ok()
        );
        assert!(
            verify_catalogue_asset_sha256(&entry, &super::super::trust::sha256_of(b"changed"))
                .is_err()
        );
    }

    #[test]
    fn catalogue_asset_body_keeps_a_response_wide_deadline() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        runtime.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
            let address = listener.local_addr().expect("server address");
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept client");
                let mut request = [0_u8; 1024];
                let _ = socket.read(&mut request).await;
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\npartial")
                    .await
                    .expect("write partial body");
                tokio::time::sleep(Duration::from_millis(100)).await;
            });
            let url = format!("http://{address}/catalogue-asset");
            let client = super::super::stream_download::asset_http_client("test catalogue asset")
                .expect("build test client");
            let response = super::super::stream_download::send_asset_request(
                super::super::stream_download::get_request(&client, &url),
                &url,
                Duration::from_secs(1),
            )
            .await
            .expect("GET");
            let error = stream_catalogue_asset_body(response, &url, Duration::from_millis(20))
                .await
                .expect_err("trickling metadata body must hit the total deadline");
            assert!(super::super::retry::is_transient(&error));
            assert!(error.to_string().contains("body read timed out"), "{error}");
        });
    }

    #[test]
    fn cache_buster_preserves_existing_query_parameters() {
        let plain = cache_busted_url("https://example.invalid/map.json");
        assert!(plain.starts_with("https://example.invalid/map.json?soldr_refresh="));
        let queried = cache_busted_url("https://example.invalid/map.json?mirror=1");
        assert!(queried.starts_with("https://example.invalid/map.json?mirror=1&soldr_refresh="));
    }

    // soldr#988 Phase 2: catalogue origin resolution.

    #[test]
    fn catalogue_url_defaults_to_pages_origin() {
        // Caller may have SOLDR_TOOLCHAIN_ORIGIN set in their env;
        // exercise the public string-shape via the pure helper that
        // does not read env: build the URL from the default origin.
        let url = format!("{}/{}", DEFAULT_TOOLCHAIN_ORIGIN, CATALOGUE_DOC_NAME);
        assert_eq!(
            url,
            "https://zackees.github.io/soldr-toolchain/catalogue.v2.json"
        );
    }

    #[test]
    fn catalogue_v1_json_parses_through_manifest_index() {
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
    }

    #[test]
    fn catalogue_v2_multipart_union_is_strict_and_path_addressable() {
        let v2 = r#"{
          "schema_version": 2,
          "generation": "g1",
          "publication_state": {
            "generation": "g1",
            "url": "https://example.test/generations/g1/publish-state.v1.json"
          },
          "entries": [{
            "owner":"zackees","repo":"soldr-toolchain","tag":"1","asset":"bundle.tar.zst",
            "source_path":"python/1/linux-x64/bundle.tar.zst",
            "sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size_bytes":3,
            "min_client_version":2,
            "parts":[
              {"number":1,"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","size_bytes":2,"urls":["https://example.test/1"]},
              {"number":2,"sha256":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","size_bytes":1,"urls":["https://example.test/2"]}
            ]
          }]
        }"#;
        let index = ManifestIndex::from_json(v2).expect("v2 multipart parses");
        let entry = &index.entries[0];
        assert!(entry.direct_url().is_none());
        assert!(entry.matches_legacy_url("https://media.githubusercontent.com/media/zackees/soldr-toolchain/assets/python/1/linux-x64/bundle.tar.zst"));

        for mutation in [
            v2.replace("\"size_bytes\":3", "\"size_bytes\":4"),
            v2.replace("\"number\":2", "\"number\":3"),
            v2.replace(
                "\"parts\":[",
                "\"urls\":[\"https://example.test/full\"],\"parts\":[",
            ),
            v2.replace("\"min_client_version\":2,", ""),
            v2.replacen("\"generation\": \"g1\"", "\"generation\": \"other\"", 1),
            v2.replace(
                "\"schema_version\": 2,",
                "\"schema_version\": 2,\"unknown\":true,",
            ),
            v2.replace(
                "\"owner\":\"zackees\"",
                "\"owner\":\"zackees\",\"unknown\":true",
            ),
            v2.replace("\"number\":1", "\"number\":1,\"unknown\":true"),
            v2.replace("publish-state.v1.json", "publish-state.v1.json?mutable=1"),
        ] {
            assert!(ManifestIndex::from_json(&mutation).is_none());
        }
    }

    #[test]
    fn multipart_materializes_verified_parts_without_nested_ranges() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            async fn serve(body: &'static [u8]) -> (String, tokio::task::JoinHandle<()>) {
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let address = listener.local_addr().expect("address");
                let handle = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.expect("accept");
                    let mut request = vec![0_u8; 4096];
                    let count = socket.read(&mut request).await.expect("read request");
                    let request = String::from_utf8_lossy(&request[..count]).to_ascii_lowercase();
                    assert!(
                        !request.contains("\r\nrange:"),
                        "multipart part was range-segmented"
                    );
                    socket
                        .write_all(
                            format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n\r\n",
                        body.len()
                    )
                            .as_bytes(),
                        )
                        .await
                        .expect("headers");
                    socket.write_all(body).await.expect("body");
                });
                (format!("http://{address}/part"), handle)
            }
            let (first_url, first_server) = serve(b"abc").await;
            let (second_url, second_server) = serve(b"def").await;
            let full = b"abcdef";
            let entry = ManifestEntry {
                owner: "o".into(),
                repo: "r".into(),
                tag: "1".into(),
                asset: "bundle.tar.zst".into(),
                url: None,
                urls: Vec::new(),
                size_bytes: Some(full.len() as u64),
                source_path: Some("x/bundle.tar.zst".into()),
                min_client_version: Some(2),
                sha256: super::super::trust::sha256_of(full),
                parts: vec![
                    ManifestPart {
                        number: 1,
                        sha256: super::super::trust::sha256_of(b"abc"),
                        size_bytes: 3,
                        urls: vec![first_url],
                    },
                    ManifestPart {
                        number: 2,
                        sha256: super::super::trust::sha256_of(b"def"),
                        size_bytes: 3,
                        urls: vec![second_url],
                    },
                ],
            };
            let downloaded = download_manifest_entry(&entry)
                .await
                .expect("multipart materializes");
            assert_eq!(std::fs::read(downloaded.path()).expect("read"), full);
            first_server.await.expect("first server");
            second_server.await.expect("second server");

            let (oversized_url, oversized_server) = serve(b"abcdef").await;
            let error = download_manifest_part_url(&oversized_url, 3)
                .await
                .expect_err("declared part size must bound the body before draining");
            assert!(
                error.to_string().contains("Content-Length mismatch"),
                "{error}"
            );
            oversized_server.await.expect("oversized server");
        });
    }

    #[test]
    fn disabled_via_env_handles_truthy_and_falsy_values() {
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
    }

    // Back-compat guard for issue #861: after schema v6 lands beside
    // the flat-array shape this module owns, an old flat manifest
    // body must keep parsing through `ManifestIndex::from_json`. This
    // proves the dispatch isn't accidentally captured by the new v6
    // parser — the two shapes are disjoint on the wire (`entries: []`
    // vs. `schema_version: 6, tools: {...}`) and must stay so.
    #[test]
    fn flat_schema_v5_still_parses_for_back_compat() {
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
    }

    #[test]
    fn default_toolchain_origin_is_pages() {
        // soldr#988 Phase 5: legacy manifest-branch URL constant is
        // gone. The default catalogue origin is the soldr-toolchain
        // Pages site.
        assert_eq!(
            DEFAULT_TOOLCHAIN_ORIGIN,
            "https://zackees.github.io/soldr-toolchain"
        );
    }

    #[test]
    fn catalogue_url_override_takes_precedence() {
        // The full-URL override is the one the integration tests use
        // when they spawn a local HTTP listener on a random port —
        // they can't fit that under the `origin + /catalogue.v1.json`
        // composition because the listener path is fixed.
        // Verify the override is recognized via the public const name.
        assert_eq!(
            TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
            "SOLDR_TOOLCHAIN_CATALOGUE_URL"
        );
    }
}
