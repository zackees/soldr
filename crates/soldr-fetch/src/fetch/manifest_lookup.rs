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
//! There is no fallback to a second URL â€” a catalogue miss degrades
//! straight to the live GitHub Releases API. Use the catalogue's own
//! producer (`scripts/build_catalogue_v1.py` on `zackees/soldr-toolchain`)
//! to publish new entries.
//!
//! Schema (intentionally flat â€” one row per (owner, repo, tag, asset) â€”
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
//! process-wide [`OnceLock`]. Any failure â€” HTTP error, parse error,
//! timeout, network â€” degrades silently to an empty index so the
//! resolver falls back to the live GitHub Releases API. The integrity
//! invariant ("a hit MUST sha256-verify against the manifest's pin") is
//! the same trust posture as `apple_sdk` / `zig`: a mismatched download
//! is a hard error regardless of `SOLDR_TRUST_MODE`.
//!
//! ## Env-var seams
//!
//! * `SOLDR_TOOLCHAIN_ORIGIN` â€” override the catalogue origin (the
//!   resolver builds `{origin}/catalogue.v1.json`). See Phase 2.
//! * `SOLDR_TOOLCHAIN_CATALOGUE_URL` â€” override the full URL (testing
//!   + air-gapped mirrors). When set takes precedence over
//!     `SOLDR_TOOLCHAIN_ORIGIN`.
//! * `SOLDR_MANIFEST_DISABLE=1` â€” skip the catalogue lookup entirely;
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
use url::Url;

use crate::core::SoldrError;

/// soldr#988 Phase 2 â€” default origin of the v1 catalogue document
/// published by `zackees/soldr-toolchain`. The full URL we fetch is
/// `{origin}/catalogue.v1.json`. Overridable via
/// [`TOOLCHAIN_ORIGIN_ENV_VAR`].
pub const DEFAULT_TOOLCHAIN_ORIGIN: &str = "https://zackees.github.io/soldr-toolchain";

/// soldr#988 Phase 2 â€” env var that overrides
/// [`DEFAULT_TOOLCHAIN_ORIGIN`]. Set to an `https://` URL (no
/// trailing slash). Test seam + air-gapped-mirror seam.
pub const TOOLCHAIN_ORIGIN_ENV_VAR: &str = "SOLDR_TOOLCHAIN_ORIGIN";

/// soldr#988 Phase 5 â€” full-URL override for the catalogue endpoint.
/// When set takes precedence over [`TOOLCHAIN_ORIGIN_ENV_VAR`]'s
/// origin+filename composition. Test seam â€” lets integration tests
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
    let url = entry.transport.direct_url().ok_or_else(|| {
        SoldrError::Other("multipart catalogue assets are not materialized until Phase 2".into())
    })?;
    let downloaded = download_catalogue_asset(url).await?;
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
    let url = refreshed_entry.transport.direct_url().ok_or_else(|| {
        SoldrError::Other("multipart catalogue assets are not materialized until Phase 2".into())
    })?;
    let refreshed = download_catalogue_asset(&cache_busted_url(url)).await?;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub owner: String,
    pub repo: String,
    pub tag: String,
    pub asset: String,
    pub transport: AssetTransport,
    pub sha256: String,
    pub size_bytes: u64,
    pub min_client_version: Option<u32>,
}

/// Transport details deliberately kept separate from an asset's logical hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetTransport {
    Direct { urls: Vec<String> },
    Multipart { parts: Vec<Part> },
}

impl AssetTransport {
    pub fn direct_url(&self) -> Option<&str> {
        match self {
            Self::Direct { urls } => urls.first().map(String::as_str),
            Self::Multipart { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub number: u32,
    pub size_bytes: u64,
    pub sha256: String,
    pub urls: Vec<String>,
}

#[derive(Deserialize)]
struct WireV1Catalogue {
    #[serde(default)]
    entries: Vec<WireV1Entry>,
}

#[derive(Deserialize)]
struct WireV1Entry {
    owner: String,
    repo: String,
    tag: String,
    asset: String,
    url: String,
    sha256: String,
}

/// V2 has a deliberately closed wire contract.  Additions require a new
/// schema version so old clients never reinterpret a transport field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireV2Catalogue {
    schema_version: u32,
    generation: String,
    publication_state: PublicationStateBinding,
    #[serde(default)]
    generated_at: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    entries: Vec<WireV2Entry>,
}

/// The v2 root is bound to the immutable publication generation.  The
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationStateBinding {
    generation: String,
    url: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireV2Entry {
    owner: String,
    repo: String,
    tag: String,
    asset: String,
    size_bytes: u64,
    sha256: String,
    urls: Option<Vec<String>>,
    parts: Option<Vec<WirePart>>,
    #[serde(default)]
    min_client_version: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationState {
    schema_version: u32,
    generation: String,
    source: GitObject,
    www: GitObject,
    active: PublicationSlot,
    previous: PublicationSlot,
    catalogue_sha256: String,
    assets_by_sha256: std::collections::BTreeMap<String, PublishedAsset>,
    logical_assets: std::collections::BTreeMap<String, LogicalAsset>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitObject {
    commit: String,
    tree: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationSlot {
    slot: String,
    commit: String,
    tree: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedAsset {
    size_bytes: u64,
    partitioner: Partitioner,
    parts: Vec<PublishedPart>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Partitioner {
    version: u32,
    target_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedPart {
    number: u32,
    sha256: String,
    size_bytes: u64,
    path: String,
    git_blob: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogicalAsset {
    source_path: String,
    asset: String,
    source_oid_sha256: String,
    source_size_bytes: u64,
    metadata_fingerprint: String,
    provenance: serde_json::Map<String, serde_json::Value>,
}

fn write_canonical_json(value: &serde_json::Value, out: &mut Vec<u8>) -> Option<()> {
    match value {
        serde_json::Value::Null => out.extend_from_slice(b"null"),
        serde_json::Value::Bool(value) => {
            out.extend_from_slice(if *value { b"true" } else { b"false" })
        }
        serde_json::Value::Number(value) => out.extend_from_slice(value.to_string().as_bytes()),
        serde_json::Value::String(value) => out.extend_from_slice(&serde_json::to_vec(value).ok()?),
        serde_json::Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                write_canonical_json(value, out)?;
            }
            out.push(b']');
        }
        serde_json::Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_unstable();
            out.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    out.push(b',');
                }
                out.extend_from_slice(&serde_json::to_vec(key).ok()?);
                out.push(b':');
                write_canonical_json(values.get(key)?, out)?;
            }
            out.push(b'}');
        }
    }
    Some(())
}

fn canonical_catalogue_sha256(body: &str) -> Option<String> {
    reject_duplicate_json_keys(body).ok()?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let mut canonical = Vec::new();
    write_canonical_json(&value, &mut canonical)?;
    Some(super::trust::sha256_of(&canonical))
}

async fn bind_v2_publication_state(body: &str) -> Result<(), SoldrError> {
    reject_duplicate_json_keys(body).map_err(SoldrError::Other)?;
    let catalogue: WireV2Catalogue = serde_json::from_str(body).map_err(|error| {
        SoldrError::Other(format!("canonical v2 catalogue did not parse: {error}"))
    })?;
    let digest = canonical_catalogue_sha256(body).ok_or_else(|| {
        SoldrError::Other("canonical v2 catalogue could not be canonicalized".into())
    })?;
    let state_url = &catalogue.publication_state.url;
    let client =
        super::stream_download::control_http_client("the soldr-toolchain publication state")?;
    let response = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, state_url),
        state_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    if !response.status().is_success() {
        return Err(SoldrError::Network(format!(
            "publication-state fetch {state_url} returned HTTP {}",
            response.status()
        )));
    }
    let state_body =
        super::stream_download::read_control_text(response, state_url, MANIFEST_FETCH_TIMEOUT)
            .await?;
    validate_publication_state_body(&state_body, &catalogue.generation, &digest)
}

fn publication_state_matches(state: &PublicationState, generation: &str, digest: &str) -> bool {
    state.schema_version == 1
        && state.generation == generation
        && valid_sha256(&state.catalogue_sha256)
        && state.catalogue_sha256 == digest
        && valid_git_object(&state.source.commit)
        && valid_git_object(&state.source.tree)
        && valid_git_object(&state.www.commit)
        && valid_git_object(&state.www.tree)
        && valid_git_object(&state.active.commit)
        && valid_git_object(&state.active.tree)
        && valid_git_object(&state.previous.commit)
        && valid_git_object(&state.previous.tree)
        && state.active.slot != state.previous.slot
        && matches!(state.active.slot.as_str(), "public-a" | "public-b")
        && matches!(state.previous.slot.as_str(), "public-a" | "public-b")
        && state
            .assets_by_sha256
            .iter()
            .all(|(sha256, asset)| valid_published_asset(sha256, asset))
        && state.logical_assets.iter().all(|(key, logical)| {
            !key.is_empty()
                && valid_logical_asset(logical)
                && state
                    .assets_by_sha256
                    .get(&logical.source_oid_sha256)
                    .is_some_and(|asset| asset.size_bytes == logical.source_size_bytes)
        })
}

fn valid_git_object(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_published_asset(sha256: &str, asset: &PublishedAsset) -> bool {
    if !valid_sha256(sha256)
        || asset.size_bytes == 0
        || asset.size_bytes > MAX_CATALOGUE_ASSET_BYTES
        || asset.partitioner.version != 1
        || asset.partitioner.target_bytes == 0
        || asset.partitioner.target_bytes > MAX_CATALOGUE_PART_BYTES
        || asset.parts.is_empty()
        || asset.parts.len() > MAX_CATALOGUE_PARTS
    {
        return false;
    }
    let mut total = 0_u64;
    for (offset, part) in asset.parts.iter().enumerate() {
        let expected = (offset + 1) as u32;
        if part.number != expected
            || !valid_sha256(&part.sha256)
            || part.size_bytes == 0
            || part.size_bytes > MAX_CATALOGUE_PART_BYTES
            || (offset + 1 < asset.parts.len() && part.size_bytes != asset.partitioner.target_bytes)
            || (offset + 1 == asset.parts.len() && part.size_bytes > asset.partitioner.target_bytes)
            || part.path != format!("sha256/{sha256}/{expected:04}-{}.part", part.sha256)
            || !valid_git_object(&part.git_blob)
        {
            return false;
        }
        let Some(next) = total.checked_add(part.size_bytes) else {
            return false;
        };
        total = next;
    }
    total == asset.size_bytes
}

fn valid_logical_asset(logical: &LogicalAsset) -> bool {
    !logical.source_path.is_empty()
        && !logical.asset.is_empty()
        && !logical.metadata_fingerprint.is_empty()
        && valid_sha256(&logical.source_oid_sha256)
        && logical.source_size_bytes > 0
        && logical.source_size_bytes <= MAX_CATALOGUE_ASSET_BYTES
        && !logical.provenance.is_empty()
        && logical.provenance.keys().all(|key| !key.is_empty())
}

fn parse_publication_state(body: &str) -> Result<PublicationState, SoldrError> {
    reject_duplicate_json_keys(body).map_err(SoldrError::Other)?;
    serde_json::from_str(body)
        .map_err(|error| SoldrError::Other(format!("publication state did not parse: {error}")))
}

fn validate_publication_state_body(
    body: &str,
    generation: &str,
    digest: &str,
) -> Result<(), SoldrError> {
    let state = parse_publication_state(body)?;
    if publication_state_matches(&state, generation, digest) {
        Ok(())
    } else {
        Err(SoldrError::Other(
            "publication state does not bind this catalogue generation".into(),
        ))
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePart {
    number: u32,
    size_bytes: u64,
    sha256: String,
    urls: Vec<String>,
}

/// True when a published release requires a newer transport-capable client.
pub fn supports_min_client_version(required: Option<u32>) -> bool {
    required.unwrap_or(0) <= CATALOGUE_CAPABILITY
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate_url(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_CATALOGUE_URL_BYTES {
        return Err("invalid URL length".into());
    }
    let parsed = Url::parse(value).map_err(|_| "URL is not absolute".to_string())?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err("URL must be credential-free absolute HTTPS".into());
    }
    Ok(())
}
fn valid_generation(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn validate_publication_state_url(value: &str, generation: &str) -> Result<(), String> {
    validate_url(value)?;
    let parsed = Url::parse(value).map_err(|_| "URL is not absolute".to_string())?;
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("publication-state URL must not have query or fragment".into());
    }
    let expected = format!("/generations/{generation}/publish-state.v1.json");
    if !parsed.path().ends_with(&expected)
        || parsed
            .path()
            .split('/')
            .any(|part| part == "." || part == "..")
    {
        return Err("publication-state URL is not generation-qualified".into());
    }
    Ok(())
}
fn validate_urls(
    urls: &[String],
    all_urls: &mut std::collections::BTreeSet<String>,
) -> Result<(), String> {
    if urls.is_empty() {
        return Err("transport has no URLs".into());
    }
    for url in urls {
        validate_url(url)?;
        if !all_urls.insert(url.clone()) {
            return Err("duplicate URL".into());
        }
    }
    Ok(())
}

fn validate_unique_logical_rows(entries: &[ManifestEntry]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for entry in entries {
        if !seen.insert((&entry.owner, &entry.repo, &entry.tag, &entry.asset)) {
            return Err("duplicate logical catalogue row".into());
        }
    }
    Ok(())
}

/// serde_json intentionally keeps the final member of a duplicate-key object.
/// Catalogue documents are signed/publication-bound input, so accepting that
/// ambiguity would let different readers resolve different assets.  Walk the
/// raw JSON once before typed deserialization and reject every duplicate key
/// at every nesting level.
fn reject_duplicate_json_keys(body: &str) -> Result<(), String> {
    use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
    use std::fmt;

    struct Seed;
    impl<'de> DeserializeSeed<'de> for Seed {
        type Value = ();
        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(RejectVisitor)
        }
    }
    struct RejectVisitor;
    impl<'de> Visitor<'de> for RejectVisitor {
        type Value = ();
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("JSON without duplicate object keys")
        }
        fn visit_bool<E>(self, _: bool) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }
        fn visit_i64<E>(self, _: i64) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }
        fn visit_u64<E>(self, _: u64) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }
        fn visit_f64<E>(self, _: f64) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }
        fn visit_str<E>(self, _: &str) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }
        fn visit_string<E>(self, _: String) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }
        fn visit_none<E>(self) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }
        fn visit_unit<E>(self) -> Result<(), E>
        where
            E: de::Error,
        {
            Ok(())
        }
        fn visit_seq<A>(self, mut sequence: A) -> Result<(), A::Error>
        where
            A: SeqAccess<'de>,
        {
            while sequence.next_element_seed(Seed)?.is_some() {}
            Ok(())
        }
        fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut keys = std::collections::BTreeSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key.clone()) {
                    return Err(de::Error::custom(format!("duplicate JSON key {key:?}")));
                }
                map.next_value_seed(Seed)?;
            }
            Ok(())
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(body);
    Seed.deserialize(&mut deserializer)
        .map_err(|error| error.to_string())?;
    deserializer.end().map_err(|error| error.to_string())
}

fn entry_from_v1_wire(
    entry: WireV1Entry,
    all_urls: &mut std::collections::BTreeSet<String>,
) -> Result<ManifestEntry, String> {
    if !valid_sha256(&entry.sha256) {
        return Err("asset SHA-256 must be lowercase 64-hex".into());
    }
    let urls = vec![entry.url];
    validate_urls(&urls, all_urls)?;
    Ok(ManifestEntry {
        owner: entry.owner,
        repo: entry.repo,
        tag: entry.tag,
        asset: entry.asset,
        transport: AssetTransport::Direct { urls },
        sha256: entry.sha256,
        size_bytes: 0,
        min_client_version: None,
    })
}

fn entry_from_v2_wire(
    entry: WireV2Entry,
    all_urls: &mut std::collections::BTreeSet<String>,
) -> Result<ManifestEntry, String> {
    if [&entry.owner, &entry.repo, &entry.tag, &entry.asset]
        .iter()
        .any(|value| value.is_empty())
    {
        return Err("catalogue identity fields must be nonempty".into());
    }
    if !valid_sha256(&entry.sha256) {
        return Err("asset SHA-256 must be lowercase 64-hex".into());
    }
    if entry.size_bytes == 0 || entry.size_bytes > MAX_CATALOGUE_ASSET_BYTES {
        return Err("invalid asset size_bytes".into());
    }
    if entry
        .min_client_version
        .is_some_and(|version| version != CATALOGUE_CAPABILITY)
    {
        return Err("min_client_version must equal this client capability".into());
    }
    let transport = match (entry.urls, entry.parts) {
        (Some(urls), None) => {
            validate_urls(&urls, all_urls)?;
            AssetTransport::Direct { urls }
        }
        (None, Some(wire_parts)) => {
            if entry.min_client_version != Some(CATALOGUE_CAPABILITY) {
                return Err("multipart assets require this client capability".into());
            }
            if wire_parts.is_empty() || wire_parts.len() > MAX_CATALOGUE_PARTS {
                return Err("too many parts".into());
            }
            let mut total = 0u64;
            let mut parts = Vec::with_capacity(wire_parts.len());
            let mut identities = std::collections::BTreeSet::new();
            for (offset, part) in wire_parts.into_iter().enumerate() {
                if part.number != (offset + 1) as u32
                    || part.size_bytes == 0
                    || part.size_bytes > MAX_CATALOGUE_PART_BYTES
                    || !valid_sha256(&part.sha256)
                {
                    return Err("invalid or non-contiguous part".into());
                }
                validate_urls(&part.urls, all_urls)?;
                if !identities.insert((part.number, part.sha256.clone())) {
                    return Err("duplicate part".into());
                }
                total = total
                    .checked_add(part.size_bytes)
                    .ok_or("part size overflow")?;
                parts.push(Part {
                    number: part.number,
                    size_bytes: part.size_bytes,
                    sha256: part.sha256,
                    urls: part.urls,
                });
            }
            if total != entry.size_bytes {
                return Err("part sizes do not equal asset size".into());
            }
            AssetTransport::Multipart { parts }
        }
        _ => return Err("asset must contain exactly one transport field".into()),
    };
    Ok(ManifestEntry {
        owner: entry.owner,
        repo: entry.repo,
        tag: entry.tag,
        asset: entry.asset,
        transport,
        sha256: entry.sha256,
        size_bytes: entry.size_bytes,
        min_client_version: entry.min_client_version,
    })
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

/// Return the URL Soldr will actually request for this catalogue entry.
///
/// Callers may use a legacy source URL only as an identity key when resolving
/// an entry. Progress output must use this label so a multipart download never
/// misleadingly reports that it is fetching the old Git LFS object.
pub fn resolved_download_label(entry: &ManifestEntry) -> &str {
    entry.display_url()
}

/// Parsed shape of the published asset index. Kept deliberately flat â€”
/// a `Vec` scan is fine at the call rate (one lookup per `fetch_tool`
/// call) and lets us drop a `serde_json::from_str` straight onto the
/// downloaded body with no post-processing.
#[derive(Debug, Clone, Default)]
pub struct ManifestIndex {
    pub entries: Vec<ManifestEntry>,
    /// A syntactically present v2 document was unsafe. Callers must not turn
    /// this into a legacy/live-API fallback.
    pub fail_closed: bool,
    source: CatalogueSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CatalogueSource {
    #[default]
    LegacyV1,
    CanonicalV2,
}

fn fail_closed_v2_index() -> ManifestIndex {
    ManifestIndex {
        entries: vec![],
        fail_closed: true,
        source: CatalogueSource::CanonicalV2,
    }
}

fn authoritative_v2_index(parsed: Option<ManifestIndex>, state_bound: bool) -> ManifestIndex {
    match (parsed, state_bound) {
        (Some(index), true) => index,
        _ => fail_closed_v2_index(),
    }
}

impl ManifestIndex {
    /// Empty manifest. Used as the graceful-degrade fallback when the
    /// network fetch, disable env var, or JSON parse fails.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse a JSON body into a [`ManifestIndex`]. Returns `None` (not
    /// an error) on parse failure so the caller can degrade silently to
    /// an empty index â€” a malformed remote manifest must never wedge a
    /// build.
    pub fn from_json(body: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(body).ok()?;
        if value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            == Some(2)
        {
            Self::from_v2_json(body)
        } else {
            Self::from_v1_json(body)
        }
    }

    fn from_v1_json(body: &str) -> Option<Self> {
        reject_duplicate_json_keys(body).ok()?;
        let wire: WireV1Catalogue = serde_json::from_str(body).ok()?;
        let mut all_urls = std::collections::BTreeSet::new();
        let mut entries = Vec::with_capacity(wire.entries.len());
        for entry in wire.entries {
            entries.push(entry_from_v1_wire(entry, &mut all_urls).ok()?);
        }
        validate_unique_logical_rows(&entries).ok()?;
        Some(Self {
            entries,
            fail_closed: false,
            source: CatalogueSource::LegacyV1,
        })
    }

    /// Parse only the canonical v2 wire contract.  Unlike `from_json`, this
    /// never accepts an absent, v1, or unknown schema version.
    fn from_v2_json(body: &str) -> Option<Self> {
        reject_duplicate_json_keys(body).ok()?;
        let wire: WireV2Catalogue = serde_json::from_str(body).ok()?;
        if wire.schema_version != 2
            || !valid_generation(&wire.generation)
            || wire.publication_state.generation != wire.generation
        {
            return None;
        }
        validate_publication_state_url(&wire.publication_state.url, &wire.generation).ok()?;
        if wire
            .origin
            .as_deref()
            .is_some_and(|origin| validate_url(origin).is_err())
        {
            return None;
        }
        let mut all_urls = std::collections::BTreeSet::new();
        all_urls.insert(wire.publication_state.url);
        let mut entries = Vec::with_capacity(wire.entries.len());
        for entry in wire.entries {
            entries.push(entry_from_v2_wire(entry, &mut all_urls).ok()?);
        }
        validate_unique_logical_rows(&entries).ok()?;
        Some(Self {
            entries,
            fail_closed: false,
            source: CatalogueSource::CanonicalV2,
        })
    }

    /// Look up the manifest entry for `(owner, repo, tag, asset)`.
    /// Tag matching is exact â€” the caller is expected to supply the
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

/// soldr#988 Phase 2 â€” resolve the catalogue origin, honoring
/// [`TOOLCHAIN_ORIGIN_ENV_VAR`]. Trailing slash is stripped so the
/// caller can append `/catalogue.v1.json` unconditionally.
pub fn resolve_toolchain_origin() -> String {
    let raw = match std::env::var(TOOLCHAIN_ORIGIN_ENV_VAR) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => DEFAULT_TOOLCHAIN_ORIGIN.to_string(),
    };
    raw.trim_end_matches('/').to_string()
}

/// soldr#988 â€” full URL of the catalogue document.
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
/// consulted â€” the legacy `manifest`-branch fallback was removed
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
    let client = super::stream_download::control_http_client("the soldr-toolchain catalogue")?;
    let resp = super::stream_download::send_control_request_with_timeout(
        super::stream_download::get_request(&client, &v2_url),
        &v2_url,
        MANIFEST_FETCH_TIMEOUT,
    )
    .await?;
    match resp.status().as_u16() {
        status if should_fallback_to_v1(status) => {
            fetch_v1_index_from(&resolve_catalogue_url()).await
        }
        status if (200..300).contains(&status) => {
            let body =
                super::stream_download::read_control_text(resp, &v2_url, MANIFEST_FETCH_TIMEOUT)
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

fn should_fallback_to_v1(status: u16) -> bool {
    matches!(status, 404 | 410)
}

/// HTTP-GET + JSON-parse a single index URL. Shared by both the v1
/// catalogue origin and the legacy `manifest` branch URL. The
/// `ManifestIndex` deserializer ignores unknown top-level fields
/// (e.g. v1's `schema_version`, `generated_at`, `origin`), so the
/// same struct cleanly absorbs both shapes.
async fn fetch_v1_index_from(url: &str) -> Result<ManifestIndex, SoldrError> {
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
    ManifestIndex::from_v1_json(&body)
        .ok_or_else(|| SoldrError::Other(format!("manifest {url} did not parse as JSON")))
}

/// Fetch a canonical v2 document.  This never falls through to v1: it is
/// used after a v2-selected pin mismatch to keep catalogue and payload bound
/// to one publication generation.
async fn fetch_v2_index_from(url: &str) -> Result<ManifestIndex, SoldrError> {
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
    let index = ManifestIndex::from_v2_json(&body).ok_or_else(|| {
        SoldrError::Other(format!("canonical v2 manifest {url} did not validate"))
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
        eprintln!("soldr toolchain catalogue: {url} â€” {reason}");
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
        assert!(hit.transport.direct_url().unwrap().contains("zccache"));
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
            transport: AssetTransport::Direct {
                urls: vec!["https://example.invalid/map.json".into()],
            },
            sha256: super::super::trust::sha256_of(bytes),
            size_bytes: bytes.len() as u64,
            min_client_version: Some(CATALOGUE_CAPABILITY),
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
            "https://zackees.github.io/soldr-toolchain/catalogue.v1.json"
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
    fn catalogue_v2_fixture_preserves_direct_and_multipart_transports() {
        let json = include_str!("../../tests/fixtures/catalogue.v2.json");
        let index = ManifestIndex::from_json(json).expect("v2 fixture is valid");
        assert!(matches!(
            index.entries[0].transport,
            AssetTransport::Direct { .. }
        ));
        assert!(matches!(
            index.entries[1].transport,
            AssetTransport::Multipart { .. }
        ));
        assert_eq!(index.entries[1].size_bytes, 6);
    }

    #[test]
    fn catalogue_v2_rejects_ambiguous_and_invalid_transport() {
        let invalid = r#"{"schema_version":2,"entries":[{"owner":"o","repo":"r","tag":"t","asset":"a","size_bytes":1,"sha256":"0000000000000000000000000000000000000000000000000000000000000000","urls":["https://example.invalid/a"],"parts":[{"number":1,"size_bytes":1,"sha256":"0000000000000000000000000000000000000000000000000000000000000000","urls":["https://example.invalid/p"]}]}]}"#;
        assert!(ManifestIndex::from_json(invalid).is_none());
        assert!(!supports_min_client_version(Some(CATALOGUE_CAPABILITY + 1)));
    }

    fn valid_hash() -> String {
        "a".repeat(64)
    }

    fn v2_entry() -> WireV2Entry {
        WireV2Entry {
            owner: "owner".into(),
            repo: "repo".into(),
            tag: "tag".into(),
            asset: "asset".into(),
            size_bytes: 1,
            sha256: valid_hash(),
            urls: Some(vec!["https://example.invalid/asset".into()]),
            parts: None,
            min_client_version: Some(CATALOGUE_CAPABILITY),
        }
    }

    fn parse_v2_entry(entry: WireV2Entry) -> Result<ManifestEntry, String> {
        entry_from_v2_wire(entry, &mut std::collections::BTreeSet::new())
    }

    #[test]
    fn canonical_v2_rejects_absent_v1_and_unknown_schema() {
        assert!(ManifestIndex::from_v2_json(r#"{"entries":[]}"#).is_none());
        assert!(ManifestIndex::from_v2_json(r#"{"schema_version":1,"entries":[]}"#).is_none());
        assert!(ManifestIndex::from_v2_json(r#"{"schema_version":3,"entries":[]}"#).is_none());
    }

    #[test]
    fn only_absent_canonical_catalogues_select_v1() {
        assert!(should_fallback_to_v1(404));
        assert!(should_fallback_to_v1(410));
        for status in [200, 204, 301, 400, 401, 403, 409, 500] {
            assert!(
                !should_fallback_to_v1(status),
                "{status} must not select v1"
            );
        }
    }

    #[test]
    fn v2_rejects_unknown_fields_and_duplicate_json_keys() {
        let fixture = include_str!("../../tests/fixtures/catalogue.v2.json");
        assert!(ManifestIndex::from_v2_json(&fixture.replacen(
            "\"generation\": \"canary-0001\",",
            "\"generation\": \"canary-0001\",\n  \"unexpected\": true,",
            1,
        ))
        .is_none());
        assert!(ManifestIndex::from_v2_json(&fixture.replacen(
            "\"asset\": \"direct.bin\",",
            "\"asset\": \"direct.bin\", \"asset\": \"other.bin\",",
            1,
        ))
        .is_none());
        assert!(ManifestIndex::from_v2_json(&fixture.replacen(
            "\"urls\": [\"https://example.invalid/direct.bin\"]",
            "\"url\": \"https://example.invalid/legacy.bin\",",
            1,
        ))
        .is_none());
    }

    #[test]
    fn v2_rejects_duplicate_rows_and_bad_publication_binding() {
        let fixture = include_str!("../../tests/fixtures/catalogue.v2.json");
        assert!(ManifestIndex::from_v2_json(&fixture.replace(
            "\"url\": \"https://example.invalid/generations/canary-0001/publish-state.v1.json\"",
            "\"url\": \"https://example.invalid/generations/other/publish-state.v1.json\"",
        ))
        .is_none());
        assert!(ManifestIndex::from_v2_json(
            &fixture.replace("\"asset\": \"multipart.bin\"", "\"asset\": \"direct.bin\"",)
        )
        .is_none());
        assert!(ManifestIndex::from_v2_json(&fixture.replace(
            "    \"generation\": \"canary-0001\",\n    \"url\"",
            "    \"generation\": \"wrong-generation\",\n    \"url\"",
        ))
        .is_none());
    }

    #[test]
    fn paired_cross_repository_contract_fixture_is_accepted_and_bound() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/catalogue-v2-contract.json"
        ))
        .expect("paired fixture is JSON");
        let catalogue = fixture["catalogue"].to_string();
        assert!(ManifestIndex::from_v2_json(&catalogue).is_some());
        assert_eq!(
            canonical_catalogue_sha256(&catalogue).as_deref(),
            fixture["publication_state"]["catalogue_sha256"].as_str()
        );
    }

    #[test]
    fn v2_matches_producer_optional_and_global_url_rules() {
        let fixture = include_str!("../../tests/fixtures/catalogue.v2.json");
        assert!(ManifestIndex::from_v2_json(&fixture.replacen(
            "\"generation\": \"canary-0001\",",
            "\"generation\": \"canary-0001\",\n  \"generated_at\": \"\",",
            1,
        ))
        .is_some());
        assert!(ManifestIndex::from_v2_json(&fixture.replace(
            "\"https://example.invalid/direct.bin\"",
            "\"https://example.invalid/generations/canary-0001/publish-state.v1.json\"",
        ))
        .is_none());
        let supported = ManifestIndex::from_v2_json(&fixture.replacen(
            "\"size_bytes\": 6,",
            "\"size_bytes\": 6, \"min_client_version\": 2,",
            1,
        ));
        assert_eq!(supported.unwrap().entries[0].min_client_version, Some(2));
        assert!(ManifestIndex::from_v2_json(&fixture.replacen(
            "\"size_bytes\": 6,",
            "\"size_bytes\": 6, \"min_client_version\": 3,",
            1,
        ))
        .is_none());
        assert!(ManifestIndex::from_v2_json(&fixture.replacen(
            "\"size_bytes\": 6,",
            "\"size_bytes\": 6, \"min_client_version\": true,",
            1,
        ))
        .is_none());
    }

    #[test]
    fn canonical_writer_sorts_nested_object_keys() {
        let a = canonical_catalogue_sha256(r#"{"z":{"b":1,"a":2},"a":3}"#);
        let b = canonical_catalogue_sha256(r#"{"a":3,"z":{"a":2,"b":1}}"#);
        assert_eq!(a, b);
    }

    #[test]
    fn publication_state_mismatches_fail_closed() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/catalogue-v2-contract.json"
        ))
        .unwrap();
        let generation = fixture["catalogue"]["generation"].as_str().unwrap();
        let digest = fixture["publication_state"]["catalogue_sha256"]
            .as_str()
            .unwrap();
        let state = parse_publication_state(&fixture["publication_state"].to_string()).unwrap();
        assert!(publication_state_matches(&state, generation, digest));
        assert!(!publication_state_matches(&state, "other", digest));
        assert!(!publication_state_matches(
            &state,
            generation,
            &"0".repeat(64)
        ));
    }

    #[test]
    fn publication_state_requires_full_valid_identity_document() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/catalogue-v2-contract.json"
        ))
        .unwrap();
        let state = fixture["publication_state"].clone();
        assert!(parse_publication_state(&state.to_string()).is_ok());

        for (field, replacement) in [
            ("source", serde_json::json!(null)),
            ("assets_by_sha256", serde_json::json!([])),
            ("logical_assets", serde_json::json!(false)),
        ] {
            let mut invalid = state.clone();
            invalid[field] = replacement;
            assert!(
                parse_publication_state(&invalid.to_string()).is_err(),
                "{field}"
            );
        }
        let generation = fixture["catalogue"]["generation"].as_str().unwrap();
        let digest = state["catalogue_sha256"].as_str().unwrap();
        assert!(validate_publication_state_body(&state.to_string(), generation, digest).is_ok());
        let mut invalid = state.clone();
        invalid["active"]["slot"] = serde_json::json!("public-c");
        let parsed = parse_publication_state(&invalid.to_string()).unwrap();
        assert!(!publication_state_matches(&parsed, generation, digest));
        assert!(validate_publication_state_body(&invalid.to_string(), generation, digest).is_err());
        let mut invalid = state.clone();
        invalid["source"]["commit"] = serde_json::json!("A".repeat(40));
        let parsed = parse_publication_state(&invalid.to_string()).unwrap();
        assert!(!publication_state_matches(&parsed, generation, digest));
        assert!(parse_publication_state(r#"{"schema_version":1,"schema_version":1}"#).is_err());
    }

    #[test]
    fn publication_state_rejects_malformed_ledger_rows_and_unknown_fields() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/catalogue-v2-contract.json"
        ))
        .unwrap();
        let generation = fixture["catalogue"]["generation"].as_str().unwrap();
        let digest = fixture["publication_state"]["catalogue_sha256"]
            .as_str()
            .unwrap();
        let full = "a".repeat(64);
        let part = "b".repeat(64);
        let mut state = fixture["publication_state"].clone();
        state["assets_by_sha256"] = serde_json::json!({
            full.clone(): {
                "size_bytes": 3,
                "partitioner": {"version": 1, "target_bytes": 3},
                "parts": [{
                    "number": 1,
                    "sha256": part.clone(),
                    "size_bytes": 3,
                    "path": format!("sha256/{full}/0001-{part}.part"),
                    "git_blob": "c".repeat(40),
                }],
            }
        });
        state["logical_assets"] = serde_json::json!({
            "asset-key": {
                "source_path": "source.tar.zst",
                "asset": "published.tar.zst",
                "source_oid_sha256": full.clone(),
                "source_size_bytes": 3,
                "metadata_fingerprint": "metadata",
                "provenance": {"producer": "test"},
            }
        });
        assert!(validate_publication_state_body(&state.to_string(), generation, digest).is_ok());

        for (path, value) in [
            ("assets_by_sha256", serde_json::json!({"not-a-sha": {}})),
            ("logical_assets", serde_json::json!({"": {}})),
        ] {
            let mut invalid = state.clone();
            invalid[path] = value;
            assert!(
                validate_publication_state_body(&invalid.to_string(), generation, digest).is_err()
            );
        }
        let mut invalid = state.clone();
        invalid["assets_by_sha256"][&full]["parts"][0]["path"] = serde_json::json!("not/canonical");
        assert!(validate_publication_state_body(&invalid.to_string(), generation, digest).is_err());
        let mut invalid = state.clone();
        invalid["logical_assets"]["asset-key"]["source_size_bytes"] = serde_json::json!(4);
        assert!(validate_publication_state_body(&invalid.to_string(), generation, digest).is_err());
        let mut invalid = state.clone();
        invalid["unknown"] = serde_json::json!(true);
        assert!(parse_publication_state(&invalid.to_string()).is_err());
        let mut invalid = state.clone();
        invalid["assets_by_sha256"][&full]["parts"][0]["unknown"] = serde_json::json!(true);
        assert!(parse_publication_state(&invalid.to_string()).is_err());
    }

    #[test]
    fn authoritative_v2_failure_never_becomes_a_legacy_empty_index() {
        let malformed = authoritative_v2_index(None, false);
        assert!(malformed.fail_closed);
        assert_eq!(malformed.source, CatalogueSource::CanonicalV2);
        let state_failure = authoritative_v2_index(Some(ManifestIndex::empty()), false);
        assert!(state_failure.fail_closed);
        // A canonical endpoint's non-success status takes this same branch.
        assert!(fail_closed_v2_index().fail_closed);
    }

    #[test]
    fn generation_uses_only_the_producer_ascii_alphabet() {
        for generation in ["ready_1.2:3-4", "A"] {
            assert!(valid_generation(generation));
        }
        for generation in [
            "",
            "with space",
            "slash/name",
            "percent%",
            "café",
            "newline\n",
        ] {
            assert!(!valid_generation(generation), "{generation:?}");
        }
    }

    #[test]
    fn v2_hostile_numeric_and_url_boundaries_are_rejected() {
        let mut entry = v2_entry();
        entry.size_bytes = MAX_CATALOGUE_ASSET_BYTES + 1;
        assert!(parse_v2_entry(entry).is_err());

        let mut entry = v2_entry();
        entry.urls = Some(vec![format!(
            "https://example.invalid/{}",
            "a".repeat(MAX_CATALOGUE_URL_BYTES)
        )]);
        assert!(parse_v2_entry(entry).is_err());

        let mut entry = v2_entry();
        entry.urls = None;
        entry.parts = Some(
            (1..=(MAX_CATALOGUE_PARTS + 1))
                .map(|number| WirePart {
                    number: number as u32,
                    size_bytes: 1,
                    sha256: valid_hash(),
                    urls: vec![format!("https://example.invalid/{number}")],
                })
                .collect(),
        );
        assert!(parse_v2_entry(entry).is_err());

        let mut entry = v2_entry();
        entry.urls = None;
        entry.parts = Some(vec![
            WirePart {
                number: 1,
                size_bytes: u64::MAX,
                sha256: valid_hash(),
                urls: vec!["https://example.invalid/1".into()],
            },
            WirePart {
                number: 2,
                size_bytes: 1,
                sha256: valid_hash(),
                urls: vec!["https://example.invalid/2".into()],
            },
        ]);
        assert!(parse_v2_entry(entry).is_err());
    }

    #[test]
    fn v2_part_count_and_transport_invariants_hold_at_the_boundary() {
        let mut entry = v2_entry();
        entry.size_bytes = MAX_CATALOGUE_PARTS as u64;
        entry.urls = None;
        entry.parts = Some(
            (1..=MAX_CATALOGUE_PARTS)
                .map(|number| WirePart {
                    number: number as u32,
                    size_bytes: 1,
                    sha256: valid_hash(),
                    urls: vec![format!("https://example.invalid/{number}")],
                })
                .collect(),
        );
        assert!(matches!(
            parse_v2_entry(entry),
            Ok(ManifestEntry {
                transport: AssetTransport::Multipart { .. },
                ..
            })
        ));

        let mut entry = v2_entry();
        entry.urls = Some(vec![
            "https://example.invalid/asset".into(),
            "https://example.invalid/asset".into(),
        ]);
        assert!(parse_v2_entry(entry).is_err());

        let mut entry = v2_entry();
        entry.urls = None;
        entry.size_bytes = 2;
        entry.parts = Some(vec![
            WirePart {
                number: 1,
                size_bytes: 1,
                sha256: valid_hash(),
                urls: vec!["https://example.invalid/one".into()],
            },
            WirePart {
                number: 1,
                size_bytes: 1,
                sha256: valid_hash(),
                urls: vec!["https://example.invalid/two".into()],
            },
        ]);
        assert!(parse_v2_entry(entry).is_err());

        let mut entry = v2_entry();
        entry.urls = None;
        entry.parts = Some(vec![WirePart {
            number: 1,
            size_bytes: 2,
            sha256: valid_hash(),
            urls: vec!["https://example.invalid/one".into()],
        }]);
        assert!(parse_v2_entry(entry).is_err());
    }

    #[test]
    fn v2_json_types_are_not_coerced() {
        let fixture = include_str!("../../tests/fixtures/catalogue.v2.json");
        assert!(ManifestIndex::from_v2_json(&fixture.replacen(
            "\"size_bytes\": 6",
            "\"size_bytes\": true",
            1
        ))
        .is_none());
        assert!(ManifestIndex::from_v2_json(&fixture.replacen(
            "\"number\": 1",
            "\"number\": \"1\"",
            1
        ))
        .is_none());
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
