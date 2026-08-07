//! Bounded-memory response-body downloads for release and toolchain assets.
//!
//! Metadata requests intentionally retain their short response-wide deadlines.
//! Archive payloads instead use this module: a successful chunk resets the
//! idle timer, so a healthy multi-gigabyte transfer is not killed by elapsed
//! wall time, while a stalled or truncated response remains retryable.
//!
//! ## Segmented downloads (setup-soldr `feat/segmented-download-experiment`, soldr#2320)
//!
//! `stream_response_to_temp_file[_with_safety_timeout]` transparently tries
//! N-way HTTP Range segmentation before falling back to the single-stream
//! loop below. This is a deliberate design choice, not an afterthought: it
//! is the ONLY place in `soldr-fetch` allowed to touch `reqwest` directly
//! (`dylints/ban_raw_network_access` denies raw reqwest calls everywhere
//! else under `crates/soldr-fetch/src/`), so folding segmentation in here
//! is what lets every existing caller — `syslib_common.rs`, `xwin_cache.rs`,
//! `archive.rs`, `llvm.rs`, `zig.rs`, `apple_sdk.rs`, `rustup_init.rs`,
//! `manifest_lookup.rs` — inherit it with **zero call-site changes**. They
//! keep calling `get_request` + `send_asset_request` +
//! `stream_response_to_temp_file` exactly as before.
//!
//! The trick: by the time a caller's already-sent `response` reaches this
//! function, its headers (`Content-Length`, `Accept-Ranges`) are already
//! available for free — no extra probe request needed. If they indicate a
//! large, range-capable resource, this module abandons that response
//! (its body is never read) and issues its own N parallel `Range` requests
//! through a freshly built client. If segmentation was never attempted, or
//! it fails for any reason, the ORIGINAL `response` — never drained, still
//! perfectly valid — is what gets streamed by the existing single-stream
//! loop below. That is the whole fallback-safety argument: segmentation can
//! only ever add a path to success, never remove the one that already
//! worked.
//!
//! ### Why default-on
//!
//! `crates/soldr-fetch/examples/dl_bench.rs` measured single-stream vs
//! segmented downloads against the real CDNs this module talks to
//! (media.githubusercontent.com LFS-style assets, GitHub release
//! signed-URL redirects): single-stream was capped at a strikingly
//! consistent ~2-2.3 MB/s on every origin tested, while segmentation
//! scaled with no plateau through N=16, landing at **8.6-10.3x** the
//! single-stream throughput and matching `aria2c -x16` within noise on
//! every URL. That is not a marginal, opt-in-only win — it is the
//! difference between the maintainer's named xwin MSVCRT pain point and a
//! four-second download. Combined with the fallback-safety argument above
//! (a segmentation failure can never make a download that used to succeed
//! start failing), the default is ON, with an opt-OUT escape hatch for the
//! rare host/proxy where it turns out not to help.
//!
//! ### Protocol inheritance (Http1Only vs Negotiated)
//!
//! Several callers pin `AssetProtocol::Http1Only` for their single-stream
//! client (`syslib_common.rs`, `llvm.rs`, `zig.rs`, `apple_sdk.rs`) as a
//! defensive fix for historical large-binary-download stalls (see
//! `git log -S http1_only` starting at PR #963); `xwin_cache.rs` never
//! carried that pin and has shipped without it since PR #1016 with no
//! reported incident. Because this module builds its OWN client for the
//! segmented request fan-out (it cannot reuse the caller's client — only
//! the caller's already-completed `response` is available), it has no
//! direct way to see which protocol the caller originally pinned. Instead
//! it infers the safe choice from `response.version()`: if the caller's
//! single connection negotiated HTTP/1.x (either because it was pinned, or
//! because that is what the server offered), the segmented fan-out also
//! pins HTTP/1-only; if it negotiated HTTP/2+, the segmented fan-out lets
//! reqwest negotiate freely too. This preserves every existing pin's
//! intent without any caller needing to pass its protocol choice through —
//! "whatever pin stream_download applies stays owned by stream_download."
//! `dl_bench.rs` separately confirmed empirically that Negotiated works
//! fine against all three benchmarked origins (including the exact mingw
//! bundle whose PR introduced `syslib_common.rs`'s pin), so this inference
//! is conservative-by-construction rather than load-bearing for
//! correctness today.
//!
//! ### Three clocks per segment attempt
//!
//! 1. **Connect/TTFB** (`SOLDR_DOWNLOAD_CONNECT_TIMEOUT_SECS`, default 10s):
//!    request issuance to first response byte (i.e. until headers are
//!    available). Re-armed fresh on every redirect hop — reqwest's own
//!    auto-redirect would share one `.send()` future across every hop,
//!    making a per-hop timeout impossible, so the segmented client
//!    disables auto-redirect (`redirect::Policy::none()`) and follows
//!    hops manually (`send_with_hop_timeout`), each with its own
//!    `tokio::time::timeout`. A connect/TTFB expiry is not fatal to the
//!    segment — it goes through the normal per-segment retry path.
//! 2. **Stall watchdog** (`SOLDR_DOWNLOAD_STALL_TIMEOUT_SECS`, default
//!    30s): arms only once the body-chunk loop starts (i.e. after clock 1
//!    already ended successfully) and resets on every chunk. A slow TLS
//!    handshake or a server that takes a while to start streaming is
//!    clock 1's problem, never clock 2's.
//! 3. **Whole-operation deadline** (`SOLDR_DOWNLOAD_TIMEOUT_SECS`,
//!    disabled by default): spans every phase of one `stream_response_to_
//!    temp_file` call, segmented attempt and single-stream fallback alike.
//!
//! Permit-wait time (see "Two connection pools" below) counts against
//! NONE of these: a segment acquires its pool permit FIRST, and only then
//! starts clock 1. Otherwise a busy pool would manufacture spurious
//! connect-timeout failures purely from FIFO queueing delay.
//!
//! ### Two connection pools (process-wide, soldr#2320 addendum 3)
//!
//! Two independent, non-borrowing pools bound total concurrent payload
//! connections:
//!
//! * **Bulk** (`SOLDR_DOWNLOAD_MAX_SOCKETS`, default 16): every segment of
//!   a segmented download, plus the single-stream fallback for anything
//!   ABOVE the quick threshold. Clocks, retries, and preemption all live
//!   here.
//! * **Quick** (`SOLDR_DOWNLOAD_QUICK_POOL`, default 4): plain FIFO, no
//!   preemption. Used for (a) every control-plane request (catalogue
//!   JSON, API probes, redirect-resolution HEADs) and (b) whole downloads
//!   whose `Content-Length` is known and at or below
//!   `SOLDR_DOWNLOAD_QUICK_THRESHOLD_BYTES` (default 4 MiB) — the same
//!   threshold also doubles as the segmentation minimum, so anything
//!   small enough to route through Quick is also never segmented; there
//!   is exactly one size boundary, not two independently-tunable ones.
//!   A response with unknown size (no `Content-Length`) always routes to
//!   Bulk — conservative, since Quick's small pool would otherwise be an
//!   easy target for an unbounded transfer to starve.
//!
//! There is no cross-pool borrowing; the hard ceiling on total concurrent
//! payload+control connections is `bulk + quick`. The motivating scenario
//! this exists for: `soldr prepare` materializing several large managed
//! bundles (cmake, ninja, LLVM, xwin) saturates Bulk with long-lived
//! segment streams, while a small catalogue/manifest lookup on Quick must
//! still complete promptly instead of queueing behind them.
//!
//! ### Permit preemption ("work stealing", soldr#2320 addendum 2, Bulk only)
//!
//! A Bulk permit is PENDING from acquisition until the holder's connect
//! phase (clock 1, across every redirect hop) finishes and the first
//! payload byte is about to be read; from there it is STREAMING and can
//! never be preempted. Quick permits skip this distinction entirely —
//! they are marked STREAMING immediately on acquisition, since Quick
//! never preempts. A background scheduler tick (~1s) steals a PENDING
//! Bulk permit for a FIFO waiter only when: a waiter exists right now,
//! the victim has been PENDING for at least a ~2s grace period, and the
//! victim has been preempted fewer than 2 times (the anti-livelock guard
//! — past that it becomes permanently non-preemptible so a uniformly slow
//! network still converges instead of cycling forever). Preemption is not
//! a failure: the victim's segment re-queues FIFO for a fresh permit, its
//! retry budget is untouched, and preemptions are counted separately from
//! retries. Tail hedging (duplicate the slowest pending segment onto an
//! idle permit, first byte wins) is explicitly out of scope here — it did
//! not fall out cleanly from this state model without materially more
//! machinery, so it is left as a documented follow-up rather than rushed.
//!
//! ### Knobs (defaults, env vars, units — all in one place)
//!
//! | Knob | Env var | Default | Unit |
//! |---|---|---|---|
//! | Enable/disable (opt-out) | `SOLDR_SEGMENTED_DOWNLOAD` | enabled (`off`/`0`/`false`/`no` disables, case-insensitive) | bool-ish |
//! | Segment count | `SOLDR_SEGMENTED_DOWNLOAD_N` | 16 | count, clamped to `[2, 16]` |
//! | Connect/TTFB timeout (clock 1) | `SOLDR_DOWNLOAD_CONNECT_TIMEOUT_SECS` | 10 | seconds |
//! | Stall watchdog (clock 2) | `SOLDR_DOWNLOAD_STALL_TIMEOUT_SECS` | 30 | seconds |
//! | Per-segment retry limit | `SOLDR_DOWNLOAD_SEGMENT_RETRIES` | 3 | count, clamped to `[0, 10]` |
//! | Whole-operation deadline (clock 3, opt-in) | `SOLDR_DOWNLOAD_TIMEOUT_SECS` | disabled | seconds |
//! | Bulk pool size | `SOLDR_DOWNLOAD_MAX_SOCKETS` | 16 | count |
//! | Quick pool size | `SOLDR_DOWNLOAD_QUICK_POOL` | 4 | count |
//! | Quick/segmentation threshold | `SOLDR_DOWNLOAD_QUICK_THRESHOLD_BYTES` | 4 MiB (4194304) | bytes |
//! | Bearer auth (forward-prep) | `SOLDR_TOOLCHAIN_AUTH_TOKEN` | unset | token string |
//!
//! Every env var fails safe to its documented default on unset, empty, or
//! unparseable input — never panics, never silently picks an unintended
//! extreme.
//!
//! ### Retry composition with `fetch::retry::with_backoff`
//!
//! Per-segment retries here are a NEW, innermost layer, entirely distinct
//! from — and never multiplying with — the existing outer
//! `with_asset_backoff`/`with_backoff` retry that callers wrap around their
//! whole `ensure_*` operation (e.g. `syslib_common::ensure_syslib_bundle`).
//! A stalled or failed segment retries its OWN byte range, resuming from
//! bytes already durably written, entirely within a single call to this
//! module — it never returns an error for that. Preemptions are cheaper
//! still: they never touch the retry counter at all (see "Permit
//! preemption"). The outer retry loop is only ever reached if the WHOLE
//! operation still fails after: segmentation was attempted and exhausted
//! all per-segment retries AND the subsequent single-stream fallback also
//! failed (its pre-existing idle/safety-timeout errors are unchanged by
//! any of this). So the layers nest without multiplying: segment-level
//! retries handle a single-connection blip cheaply and invisibly; the
//! outer loop still only re-runs the same number of times it always did,
//! for the same terminal reasons it always did.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::core::SoldrError;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio::task::JoinSet;

pub(crate) const ASSET_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const ASSET_HEADER_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const CONTROL_HEADER_TIMEOUT: Duration = Duration::from_secs(120);
/// A final circuit breaker for an otherwise-progressing asset download.
///
/// An idle watchdog alone would permit a server to trickle bytes forever. The
/// caller retries this transient failure from a freshly-created temporary file;
/// partial files are never exposed as completed artifacts.
pub(crate) const ASSET_SAFETY_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// Transport compatibility policy for a remote asset host.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum AssetProtocol {
    /// Let reqwest negotiate the best available HTTP version.
    #[default]
    Negotiated,
    /// Retain the HTTP/1-only compatibility mode required by selected SDK CDNs.
    Http1Only,
}

/// Construct the sole HTTP client for bounded control-plane requests.
pub(crate) fn control_http_client(purpose: &str) -> Result<reqwest::Client, SoldrError> {
    super::net_guard::ensure_network_allowed(purpose)?;
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(CONTROL_HEADER_TIMEOUT)
        .user_agent(format!("soldr/{}", crate::core::version()))
        .build()
        .map_err(|error| SoldrError::Network(error.to_string()))
}

/// Construct the sole HTTP client for streamed asset requests.
pub(crate) fn asset_http_client(purpose: &str) -> Result<reqwest::Client, SoldrError> {
    asset_http_client_with_protocol(purpose, AssetProtocol::Negotiated)
}

/// Construct the sole asset client, optionally retaining a documented
/// compatibility restriction for a particular host.
pub(crate) fn asset_http_client_with_protocol(
    purpose: &str,
    protocol: AssetProtocol,
) -> Result<reqwest::Client, SoldrError> {
    super::net_guard::ensure_network_allowed(purpose)?;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(format!("soldr/{}", crate::core::version()));
    if matches!(protocol, AssetProtocol::Http1Only) {
        builder = builder.http1_only();
    }
    builder
        .build()
        .map_err(|error| SoldrError::Network(error.to_string()))
}

/// Build a GET request through the fetch boundary.
pub(crate) fn get_request(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client.get(url)
}

/// Build a POST request through the fetch boundary.
pub(crate) fn post_request(client: &reqwest::Client, url: &str) -> reqwest::RequestBuilder {
    client.post(url)
}

/// Attach a serialized JSON request body through the fetch boundary.
pub(crate) fn with_json_body<T: serde::Serialize>(
    request: reqwest::RequestBuilder,
    body: &T,
) -> reqwest::RequestBuilder {
    request.json(body)
}

#[derive(Debug)]
pub(crate) struct DownloadedAsset {
    file: tempfile::NamedTempFile,
    sha256: String,
    bytes: u64,
}

impl DownloadedAsset {
    pub(crate) fn path(&self) -> &Path {
        self.file.path()
    }

    pub(crate) fn sha256(&self) -> &str {
        &self.sha256
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

pub(crate) async fn stream_response_to_temp_file(
    response: reqwest::Response,
    url: &str,
    idle_timeout: Duration,
) -> Result<DownloadedAsset, SoldrError> {
    stream_response_to_temp_file_with_safety_timeout(
        response,
        url,
        idle_timeout,
        ASSET_SAFETY_TIMEOUT,
    )
    .await
}

/// Stream an archive response to a temporary file, incrementally hashing every
/// chunk while enforcing independent idle-progress and total-safety deadlines.
///
/// Before doing so, tries N-way Range segmentation (see module docs) using
/// only the headers of the already-sent `response` — no extra probe request.
/// `response`'s body is never touched unless/until segmentation is skipped
/// or fails, so the single-stream path below is always exactly as safe as
/// it was before this feature existed.
pub(crate) async fn stream_response_to_temp_file_with_safety_timeout(
    response: reqwest::Response,
    url: &str,
    idle_timeout: Duration,
    safety_timeout: Duration,
) -> Result<DownloadedAsset, SoldrError> {
    stream_response_to_temp_file_inner(
        response,
        url,
        idle_timeout,
        safety_timeout,
        bulk_pool(),
        quick_pool(),
    )
    .await
}

/// Test-only entry point that plumbs a caller-supplied pool through instead
/// of the process-wide singletons for both bulk and quick roles, so pool-
/// bounding and preemption behavior can be exercised deterministically
/// without depending on (or polluting) global state shared with every
/// other test in the binary.
#[cfg(test)]
pub(crate) async fn stream_response_to_temp_file_with_pool(
    response: reqwest::Response,
    url: &str,
    idle_timeout: Duration,
    safety_timeout: Duration,
    pool: Arc<SocketPool>,
) -> Result<DownloadedAsset, SoldrError> {
    stream_response_to_temp_file_inner(
        response,
        url,
        idle_timeout,
        safety_timeout,
        Arc::clone(&pool),
        pool,
    )
    .await
}

async fn stream_response_to_temp_file_inner(
    mut response: reqwest::Response,
    url: &str,
    idle_timeout: Duration,
    safety_timeout: Duration,
    bulk: Arc<SocketPool>,
    quick: Arc<SocketPool>,
) -> Result<DownloadedAsset, SoldrError> {
    if !response.status().is_success() {
        return Err(SoldrError::Network(format!(
            "asset download {url} failed: HTTP {}",
            response.status()
        )));
    }

    let threshold = parse_quick_threshold();
    let mut effective_safety_timeout = safety_timeout;
    let config = SegmentedDownloadConfig::from_env();
    if config.enabled {
        if let Some(total) = segmentable_total_len(&response, threshold) {
            match try_segmented_download(&response, url, total, config, Arc::clone(&bulk)).await {
                Ok(asset) => return Ok(asset),
                Err(SegmentedFailure::Fallback {
                    reason,
                    safety_timeout_override,
                }) => {
                    eprintln!(
                        "soldr: segmented download for {url} fell back to single-stream: {reason}"
                    );
                    if let Some(bound) = safety_timeout_override {
                        effective_safety_timeout = effective_safety_timeout.min(bound);
                    }
                }
                Err(SegmentedFailure::Timeout(message)) => {
                    return Err(SoldrError::Network(message));
                }
            }
        }
    }
    let safety_timeout = effective_safety_timeout;

    // Pool routing for the single-stream drain (module docs "Two
    // connection pools"): known-small -> Quick (never segmented, no
    // preemption needed); unknown or large -> Bulk, conservative. The
    // connect phase for this response already happened in the caller's
    // own `send_asset_request` before we ever saw it, so there is no
    // PENDING/preemptible phase left for us to model -- acquire and go
    // straight to STREAMING for the duration of the drain loop below.
    let pool = match response.content_length() {
        Some(total) if total <= threshold => quick,
        _ => bulk,
    };
    let mut permit = pool.acquire().await;
    permit.mark_streaming();

    let mut file = tempfile::NamedTempFile::new_in(soldr_core::core::ensure_temp_root())?;
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let started = tokio::time::Instant::now();

    loop {
        if started.elapsed() >= safety_timeout {
            return Err(SoldrError::Network(format!(
                "asset download exceeded its global safety ceiling of {safety_timeout:?} after {bytes} bytes: {url}"
            )));
        }
        let remaining = safety_timeout.saturating_sub(started.elapsed());
        let wait = idle_timeout.min(remaining);
        let chunk = tokio::time::timeout(wait, response.chunk())
            .await
            .map_err(|_| {
                if wait == remaining {
                    SoldrError::Network(format!(
                        "asset download exceeded its global safety ceiling of {safety_timeout:?} after {bytes} bytes: {url}"
                    ))
                } else {
                    stalled_download_error(url, bytes, idle_timeout)
                }
            })?
            .map_err(|error| interrupted_download_error(url, bytes, error))?;
        let Some(chunk) = chunk else {
            break;
        };
        file.write_all(&chunk)?;
        hasher.update(&chunk);
        bytes = bytes.saturating_add(chunk.len() as u64);
    }
    file.flush()?;

    Ok(DownloadedAsset {
        file,
        sha256: hex::encode(hasher.finalize()),
        bytes,
    })
}

pub(crate) async fn send_asset_request(
    request: reqwest::RequestBuilder,
    url: &str,
    header_timeout: Duration,
) -> Result<reqwest::Response, SoldrError> {
    tokio::time::timeout(header_timeout, request.send())
        .await
        .map_err(|_| {
            SoldrError::Network(format!(
                "asset request timed out waiting for headers: {url}"
            ))
        })?
        .map_err(|error| SoldrError::Network(error.to_string()))
}

/// Send a small metadata/API request with the control-plane header deadline.
///
/// Routes through the Quick pool (module docs "Two connection pools") --
/// FIFO, no preemption, sized independently of Bulk so a small catalogue
/// fetch never queues behind busy segment streams.
pub(crate) async fn send_control_request(
    request: reqwest::RequestBuilder,
    url: &str,
) -> Result<reqwest::Response, SoldrError> {
    send_control_request_with_timeout(request, url, CONTROL_HEADER_TIMEOUT).await
}

/// Send a control request with a caller's narrower operation-specific budget.
pub(crate) async fn send_control_request_with_timeout(
    request: reqwest::RequestBuilder,
    url: &str,
    header_timeout: Duration,
) -> Result<reqwest::Response, SoldrError> {
    send_control_request_with_pool_inner(request, url, header_timeout, quick_pool()).await
}

/// Test-only entry point that plumbs a caller-supplied pool through.
#[cfg(test)]
pub(crate) async fn send_control_request_with_pool(
    request: reqwest::RequestBuilder,
    url: &str,
    header_timeout: Duration,
    pool: Arc<SocketPool>,
) -> Result<reqwest::Response, SoldrError> {
    send_control_request_with_pool_inner(request, url, header_timeout, pool).await
}

async fn send_control_request_with_pool_inner(
    request: reqwest::RequestBuilder,
    url: &str,
    header_timeout: Duration,
    pool: Arc<SocketPool>,
) -> Result<reqwest::Response, SoldrError> {
    // Quick permits skip PENDING/preemption entirely -- see module docs.
    let mut permit = pool.acquire().await;
    permit.mark_streaming();
    tokio::time::timeout(header_timeout, request.send())
        .await
        .map_err(|_| {
            SoldrError::Network(format!(
                "control request timed out waiting for headers: {url}"
            ))
        })?
        .map_err(|error| SoldrError::Network(error.to_string()))
    // `permit` drops once headers are back; the (small, fast) body read
    // that follows via `read_control_text` is intentionally ungated.
}

/// Read a small control-plane response through the fetch boundary.
pub(crate) async fn read_control_text(
    response: reqwest::Response,
    url: &str,
    body_timeout: Duration,
) -> Result<String, SoldrError> {
    tokio::time::timeout(body_timeout, response.text())
        .await
        .map_err(|_| SoldrError::Network(format!("control response body timed out: {url}")))?
        .map_err(|error| SoldrError::Network(error.to_string()))
}

fn stalled_download_error(url: &str, bytes: u64, idle_timeout: Duration) -> SoldrError {
    SoldrError::Network(format!(
        "asset download stalled after {bytes} bytes with no progress for {idle_timeout:?}: {url}"
    ))
}

fn interrupted_download_error(url: &str, bytes: u64, error: reqwest::Error) -> SoldrError {
    SoldrError::Network(format!(
        "asset download interrupted after {bytes} bytes: {url}: {error}"
    ))
}

// =============================================================================
// Segmented download (setup-soldr feat/segmented-download-experiment, soldr#2320)
// =============================================================================

pub(crate) const SEGMENTED_DOWNLOAD_ENV_VAR: &str = "SOLDR_SEGMENTED_DOWNLOAD";
pub(crate) const SEGMENTED_DOWNLOAD_N_ENV_VAR: &str = "SOLDR_SEGMENTED_DOWNLOAD_N";
pub(crate) const CONNECT_TIMEOUT_ENV_VAR: &str = "SOLDR_DOWNLOAD_CONNECT_TIMEOUT_SECS";
pub(crate) const STALL_TIMEOUT_ENV_VAR: &str = "SOLDR_DOWNLOAD_STALL_TIMEOUT_SECS";
pub(crate) const SEGMENT_RETRIES_ENV_VAR: &str = "SOLDR_DOWNLOAD_SEGMENT_RETRIES";
pub(crate) const GLOBAL_TIMEOUT_ENV_VAR: &str = "SOLDR_DOWNLOAD_TIMEOUT_SECS";
pub(crate) const MAX_SOCKETS_ENV_VAR: &str = "SOLDR_DOWNLOAD_MAX_SOCKETS";
pub(crate) const QUICK_POOL_ENV_VAR: &str = "SOLDR_DOWNLOAD_QUICK_POOL";
pub(crate) const QUICK_THRESHOLD_ENV_VAR: &str = "SOLDR_DOWNLOAD_QUICK_THRESHOLD_BYTES";
/// Bearer token attached to every probe-free segment request. Forward-prep
/// for a future private MSVC bundle origin; none of today's public
/// catalogue/xwin-cache/release-asset origins require it.
pub(crate) const AUTH_TOKEN_ENV_VAR: &str = "SOLDR_TOOLCHAIN_AUTH_TOKEN";

const MAX_SEGMENTS: u32 = 16;
const DEFAULT_SEGMENT_COUNT: u32 = 16;
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
const DEFAULT_STALL_TIMEOUT_SECS: u64 = 30;
const DEFAULT_SEGMENT_RETRIES: u32 = 3;
const MAX_SEGMENT_RETRIES: u32 = 10;
const DEFAULT_MAX_SOCKETS: usize = 16;
const DEFAULT_QUICK_POOL_SIZE: usize = 4;
/// Same threshold doubles as the Quick-pool routing cutoff AND the
/// segmentation minimum -- see module docs "Two connection pools". Lowered
/// under `cfg(test)` so unit tests can exercise the segmented path without
/// transferring megabytes over localhost; production default is 4 MiB.
#[cfg(not(test))]
const DEFAULT_QUICK_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(test)]
const DEFAULT_QUICK_THRESHOLD_BYTES: u64 = 256;
/// Redirect-hop ceiling for the segmented client's manual redirect
/// following (see module docs "Three clocks"). Matches reqwest's own
/// default auto-redirect limit.
const MAX_REDIRECT_HOPS: u32 = 10;
/// If a whole-operation deadline (`SOLDR_DOWNLOAD_TIMEOUT_SECS`) expires
/// with less than this much budget left, a single-stream fallback attempt
/// is not "meaningful" -- surface a clear timeout error instead of
/// starting an attempt that has almost no chance to finish.
const MEANINGFUL_FALLBACK_MIN: Duration = Duration::from_secs(5);

/// Resolved segmented-download configuration. See the module docs' knob
/// table for env var names, defaults, and units.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SegmentedDownloadConfig {
    pub(crate) enabled: bool,
    pub(crate) segment_count: u32,
    pub(crate) connect_timeout: Duration,
    pub(crate) stall_timeout: Duration,
    pub(crate) segment_retries: u32,
    pub(crate) global_timeout: Option<Duration>,
}

impl SegmentedDownloadConfig {
    pub(crate) fn from_env() -> Self {
        Self {
            enabled: !opted_out(SEGMENTED_DOWNLOAD_ENV_VAR),
            segment_count: parse_segment_count(),
            connect_timeout: parse_connect_timeout(),
            stall_timeout: parse_stall_timeout(),
            segment_retries: parse_segment_retries(),
            global_timeout: parse_global_timeout(),
        }
    }
}

/// `off`/`0`/`false`/`no` (case-insensitive, trimmed) opt out; anything
/// else -- including unset -- leaves the feature on. Mirrors
/// `msvc_host::opted_out`'s value parsing (`SOLDR_MSVC_DISCOVERY=off`).
fn opted_out(var: &str) -> bool {
    match std::env::var(var) {
        Ok(v) => {
            let lower = v.trim().to_lowercase();
            matches!(lower.as_str(), "off" | "0" | "false" | "no")
        }
        Err(_) => false,
    }
}

fn auth_token() -> Option<String> {
    std::env::var(AUTH_TOKEN_ENV_VAR)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn parse_segment_count() -> u32 {
    std::env::var(SEGMENTED_DOWNLOAD_N_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n >= 2)
        .map(|n| n.min(MAX_SEGMENTS))
        .unwrap_or(DEFAULT_SEGMENT_COUNT)
}

fn parse_connect_timeout() -> Duration {
    std::env::var(CONNECT_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS))
}

fn parse_stall_timeout() -> Duration {
    std::env::var(STALL_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_STALL_TIMEOUT_SECS))
}

fn parse_segment_retries() -> u32 {
    std::env::var(SEGMENT_RETRIES_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|n| n.min(MAX_SEGMENT_RETRIES))
        .unwrap_or(DEFAULT_SEGMENT_RETRIES)
}

/// `None` (disabled) unless a valid positive integer is set -- junk values
/// fail safe to disabled, never to some arbitrary enabled duration.
fn parse_global_timeout() -> Option<Duration> {
    std::env::var(GLOBAL_TIMEOUT_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
}

fn parse_max_sockets() -> usize {
    std::env::var(MAX_SOCKETS_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_SOCKETS)
}

fn parse_quick_pool_size() -> usize {
    std::env::var(QUICK_POOL_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_QUICK_POOL_SIZE)
}

fn parse_quick_threshold() -> u64 {
    std::env::var(QUICK_THRESHOLD_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_QUICK_THRESHOLD_BYTES)
}

/// Whether `response`'s headers alone (no extra request) indicate a
/// resource worth segmenting: the server advertises `Accept-Ranges: bytes`
/// and a `Content-Length` strictly above `threshold` (at/below threshold
/// routes to the Quick pool instead and is never segmented -- module docs
/// "Two connection pools").
fn segmentable_total_len(response: &reqwest::Response, threshold: u64) -> Option<u64> {
    let accept_ranges = response
        .headers()
        .get(reqwest::header::ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("bytes"))
        .unwrap_or(false);
    if !accept_ranges {
        return None;
    }
    let total = response.content_length()?;
    (total > threshold).then_some(total)
}

// -----------------------------------------------------------------------
// Connection pools: process-wide permits with PENDING/STREAMING
// preemption for Bulk; plain FIFO for Quick.
// -----------------------------------------------------------------------

const PREEMPT_GRACE: Duration = Duration::from_secs(2);
const MAX_PREEMPTIONS: u32 = 2;
const SCHEDULER_TICK: Duration = Duration::from_secs(1);

static BULK_POOL: OnceLock<Arc<SocketPool>> = OnceLock::new();
static QUICK_POOL: OnceLock<Arc<SocketPool>> = OnceLock::new();

/// The process-wide Bulk payload-transfer pool. See module docs "Two
/// connection pools" and "Permit preemption".
pub(crate) fn bulk_pool() -> Arc<SocketPool> {
    Arc::clone(BULK_POOL.get_or_init(|| SocketPool::new(parse_max_sockets())))
}

/// The process-wide Quick pool for control-plane requests and small
/// downloads. See module docs "Two connection pools".
pub(crate) fn quick_pool() -> Arc<SocketPool> {
    Arc::clone(QUICK_POOL.get_or_init(|| SocketPool::new(parse_quick_pool_size())))
}

struct PendingEntry {
    since: tokio::time::Instant,
    preemptions: u32,
    preempt: Arc<Notify>,
}

#[derive(Default)]
struct PoolInner {
    used: usize,
    waiting: usize,
    pending: HashMap<u64, PendingEntry>,
}

/// A connection-slot pool. Unlike a plain semaphore, each held slot
/// tracks whether it is PENDING (acquired, pre-first-byte) or STREAMING
/// (first payload byte received, permanently non-preemptible) so a
/// background scheduler tick can steal a PENDING slot for a FIFO waiter
/// under contention -- Bulk uses this; Quick holders just mark themselves
/// STREAMING immediately and are never preemption candidates. See module
/// docs "Permit preemption" for the full policy.
pub(crate) struct SocketPool {
    capacity: usize,
    inner: Mutex<PoolInner>,
    wake: Notify,
    next_id: AtomicU64,
}

impl SocketPool {
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        let pool = Arc::new(Self {
            capacity: capacity.max(1),
            inner: Mutex::new(PoolInner::default()),
            wake: Notify::new(),
            next_id: AtomicU64::new(0),
        });
        let scheduler_pool = Arc::clone(&pool);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SCHEDULER_TICK).await;
                scheduler_pool.maybe_preempt();
            }
        });
        pool
    }

    /// One scheduler tick: if a FIFO waiter exists, steal the oldest
    /// PENDING slot that has cleared the grace period and has not already
    /// been preempted [`MAX_PREEMPTIONS`] times. Freeing the slot itself
    /// happens when the victim's own `fetch_segment_once` observes the
    /// preempt signal and drops its [`PoolPermit`] -- this only decides
    /// *who*, then wakes it up. A pool with no PENDING holders (e.g.
    /// Quick, which never has any) is a no-op every tick.
    fn maybe_preempt(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.waiting == 0 {
            return;
        }
        let now = tokio::time::Instant::now();
        let victim = inner
            .pending
            .iter()
            .filter(|(_, e)| {
                now.saturating_duration_since(e.since) >= PREEMPT_GRACE
                    && e.preemptions < MAX_PREEMPTIONS
            })
            .min_by_key(|(_, e)| e.since)
            .map(|(&id, _)| id);
        if let Some(id) = victim {
            if let Some(entry) = inner.pending.get_mut(&id) {
                entry.preemptions += 1;
                entry.preempt.notify_one();
            }
        }
        drop(inner);
        self.wake.notify_waiters();
    }

    /// Acquire a slot, blocking FIFO-fashion while the pool is at
    /// capacity. The returned permit starts PENDING.
    pub(crate) async fn acquire(self: &Arc<Self>) -> PoolPermit {
        loop {
            {
                let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if inner.used < self.capacity {
                    inner.used += 1;
                    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    let preempt = Arc::new(Notify::new());
                    inner.pending.insert(
                        id,
                        PendingEntry {
                            since: tokio::time::Instant::now(),
                            preemptions: 0,
                            preempt: Arc::clone(&preempt),
                        },
                    );
                    return PoolPermit {
                        pool: Arc::clone(self),
                        id,
                        preempt,
                        streaming: false,
                    };
                }
                inner.waiting += 1;
            }
            self.wake.notified().await;
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.waiting = inner.waiting.saturating_sub(1);
        }
    }

    #[cfg(test)]
    fn available(&self) -> usize {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.capacity - inner.used
    }
}

/// A held connection-pool slot. Dropping it (any return path: success,
/// failure, stall-watchdog kill, or preemption) frees the slot -- this is
/// also the permit-leak guarantee: RAII means a killed/aborted segment
/// task can never hold a slot forever.
pub(crate) struct PoolPermit {
    pool: Arc<SocketPool>,
    id: u64,
    preempt: Arc<Notify>,
    streaming: bool,
}

impl PoolPermit {
    /// Transition PENDING -> STREAMING: the first payload byte is about
    /// to be read (Bulk), or this permit will never have a PENDING phase
    /// at all (Quick, single-stream drain). After this call the permit
    /// can never be preempted. Idempotent.
    fn mark_streaming(&mut self) {
        if self.streaming {
            return;
        }
        self.streaming = true;
        let mut inner = self.pool.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.pending.remove(&self.id);
    }

    /// Race `fut` (a PENDING-phase operation, i.e. connect/TTFB) against
    /// this permit's preemption signal. `Err(())` means preempted --
    /// callers must not call this again after [`mark_streaming`].
    async fn race_preemption<F: std::future::Future>(&self, fut: F) -> Result<F::Output, ()> {
        tokio::select! {
            biased;
            _ = self.preempt.notified() => Err(()),
            out = fut => Ok(out),
        }
    }
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        let mut inner = self.pool.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.used = inner.used.saturating_sub(1);
        inner.pending.remove(&self.id);
        drop(inner);
        self.pool.wake.notify_waiters();
    }
}

/// Why a segmented attempt did not produce a verified asset.
enum SegmentedFailure {
    /// Recoverable: the caller should drain the original, still-untouched
    /// `response` exactly as it would have before this feature existed.
    /// `safety_timeout_override`, when set, tightens the caller's
    /// remaining safety-timeout budget (only ever set when a whole-
    /// operation deadline ate into it).
    Fallback {
        reason: String,
        safety_timeout_override: Option<Duration>,
    },
    /// Fatal: a whole-operation deadline (`SOLDR_DOWNLOAD_TIMEOUT_SECS`)
    /// expired with too little budget left to attempt anything else.
    Timeout(String),
}

impl SegmentedFailure {
    fn fallback(reason: impl Into<String>) -> Self {
        SegmentedFailure::Fallback {
            reason: reason.into(),
            safety_timeout_override: None,
        }
    }
}

/// The segmented client disables reqwest's own auto-redirect
/// (`redirect::Policy::none()`) so [`send_with_hop_timeout`] can give
/// each hop its own connect/TTFB clock -- see module docs "Three clocks".
/// Small, deliberate duplication of [`asset_http_client_with_protocol`]
/// rather than a shared parameterized builder, so that function's
/// existing (auto-redirecting) behavior for every other caller stays
/// untouched.
fn segmented_http_client(protocol: AssetProtocol) -> Result<reqwest::Client, SoldrError> {
    super::net_guard::ensure_network_allowed("segmented asset download")?;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("soldr/{}", crate::core::version()));
    if matches!(protocol, AssetProtocol::Http1Only) {
        builder = builder.http1_only();
    }
    builder
        .build()
        .map_err(|error| SoldrError::Network(error.to_string()))
}

fn resolve_redirect_url(current: &str, location: &str) -> Result<String, String> {
    let base = url::Url::parse(current).map_err(|e| format!("bad current url {current:?}: {e}"))?;
    let next = base
        .join(location)
        .map_err(|e| format!("bad redirect location {location:?}: {e}"))?;
    Ok(next.to_string())
}

/// Send one logical GET, following redirects manually so each hop gets
/// its own connect/TTFB clock (clock 1). `build_request` is called fresh
/// for every hop since a `RequestBuilder` is consumed by `.send()`.
async fn send_with_hop_timeout(
    client: &reqwest::Client,
    initial_url: &str,
    build_request: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    connect_timeout: Duration,
) -> Result<reqwest::Response, String> {
    let mut url = initial_url.to_string();
    for _hop in 0..MAX_REDIRECT_HOPS {
        let req = build_request(client, &url);
        let resp = match tokio::time::timeout(connect_timeout, req.send()).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(format!("connect/send failed: {e}")),
            Err(_) => return Err(format!("connect/TTFB exceeded {connect_timeout:?}")),
        };
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            match location {
                Some(loc) => {
                    url = resolve_redirect_url(&url, &loc)?;
                    continue; // fresh clock next iteration -- hop resets it.
                }
                None => {
                    return Err(format!(
                        "redirect status {} with no Location header",
                        resp.status()
                    ))
                }
            }
        }
        return Ok(resp);
    }
    Err(format!("exceeded {MAX_REDIRECT_HOPS} redirect hops"))
}

/// Attempt N-way Range segmentation. `response` is used only for its
/// already-received headers/version (see module docs) -- its body is
/// never read here.
async fn try_segmented_download(
    response: &reqwest::Response,
    url: &str,
    total: u64,
    config: SegmentedDownloadConfig,
    pool: Arc<SocketPool>,
) -> Result<DownloadedAsset, SegmentedFailure> {
    // Protocol inheritance -- see module docs "Protocol inheritance".
    let protocol = if response.version() >= reqwest::Version::HTTP_2 {
        AssetProtocol::Negotiated
    } else {
        AssetProtocol::Http1Only
    };
    let client = segmented_http_client(protocol)
        .map_err(|e| SegmentedFailure::fallback(format!("client build failed: {e}")))?;

    let plan = compute_segments(total, config.segment_count);
    if plan.is_empty() {
        return Err(SegmentedFailure::fallback("empty segment plan"));
    }

    let named = tempfile::NamedTempFile::new_in(soldr_core::core::ensure_temp_root())
        .map_err(|e| SegmentedFailure::fallback(format!("tempfile create failed: {e}")))?;
    named
        .as_file()
        .set_len(total)
        .map_err(|e| SegmentedFailure::fallback(format!("preallocate failed: {e}")))?;
    let file = Arc::new(
        named
            .reopen()
            .map_err(|e| SegmentedFailure::fallback(format!("reopen failed: {e}")))?,
    );

    let run = run_all_segments(
        client,
        url.to_string(),
        plan,
        Arc::clone(&file),
        config,
        pool,
    );
    let outcome = match config.global_timeout {
        Some(gt) => {
            let deadline = tokio::time::Instant::now() + gt;
            match tokio::time::timeout(gt, run).await {
                Ok(inner) => inner,
                Err(_elapsed) => {
                    // `run` (and its JoinSet) is dropped here, which aborts
                    // every still-running segment task -- their PoolPermit
                    // Drop impls release their slots.
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    return if remaining >= MEANINGFUL_FALLBACK_MIN {
                        Err(SegmentedFailure::Fallback {
                            reason: format!(
                                "{GLOBAL_TIMEOUT_ENV_VAR}={gt:?} exceeded during segmented transfer"
                            ),
                            safety_timeout_override: Some(remaining),
                        })
                    } else {
                        Err(SegmentedFailure::Timeout(format!(
                            "asset download exceeded {GLOBAL_TIMEOUT_ENV_VAR}={gt:?} with no \
                             meaningful budget left to fall back to single-stream: {url}"
                        )))
                    };
                }
            }
        }
        None => run.await,
    };

    let bytes = outcome.map_err(SegmentedFailure::fallback)?;
    file.sync_all()
        .map_err(|e| SegmentedFailure::fallback(format!("sync failed: {e}")))?;
    let sha256 = sha256_of_file(&file)
        .map_err(|e| SegmentedFailure::fallback(format!("hash failed: {e}")))?;
    Ok(DownloadedAsset {
        file: named,
        sha256,
        bytes,
    })
}

/// Run every planned segment concurrently. On the first segment that
/// exhausts its retry budget, aborts every other still-running segment
/// (via `JoinSet::abort_all`) and returns its failure reason -- "the
/// segmented attempt aborts and falls through to single-stream".
async fn run_all_segments(
    client: reqwest::Client,
    url: String,
    plan: Vec<(u64, u64)>,
    file: Arc<std::fs::File>,
    config: SegmentedDownloadConfig,
    pool: Arc<SocketPool>,
) -> Result<u64, String> {
    let mut set = JoinSet::new();
    for (start, end_inclusive) in plan {
        let client = client.clone();
        let url = url.clone();
        let file = Arc::clone(&file);
        let pool = Arc::clone(&pool);
        set.spawn(async move {
            fetch_segment_with_retries(&client, &url, start, end_inclusive, &file, config, pool)
                .await
        });
    }

    let mut total_bytes = 0u64;
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(n)) => total_bytes += n,
            Ok(Err(reason)) => {
                set.abort_all();
                return Err(reason);
            }
            Err(join_err) => {
                set.abort_all();
                return Err(format!("segment task panicked: {join_err}"));
            }
        }
    }
    Ok(total_bytes)
}

/// Outcome of one attempt at a segment's byte range.
enum SegmentAttemptOutcome {
    /// Fully delivered `n` bytes this attempt.
    Completed(u64),
    /// Preempted during the PENDING (connect) phase -- zero bytes were
    /// ever at risk. Not a failure: the caller re-queues for a fresh
    /// permit without touching its retry budget.
    Preempted,
    /// Failed after writing `.0` bytes this attempt, for reason `.1`.
    Failed(u64, String),
}

/// Fetch one `[start, end_inclusive]` byte range, retrying up to
/// `config.segment_retries` additional times on any failure or stall.
/// Each retry resumes from `start + <bytes already durably written>`,
/// never re-requesting bytes already on disk. Preemptions loop back
/// immediately without incrementing the retry counter.
async fn fetch_segment_with_retries(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end_inclusive: u64,
    file: &std::fs::File,
    config: SegmentedDownloadConfig,
    pool: Arc<SocketPool>,
) -> Result<u64, String> {
    let expected = end_inclusive - start + 1;
    let mut written = 0u64;
    let mut attempt = 0u32;
    loop {
        let resume_start = start + written;
        let outcome = fetch_segment_once(
            client,
            url,
            resume_start,
            end_inclusive,
            file,
            config.stall_timeout,
            config.connect_timeout,
            &pool,
        )
        .await;
        match outcome {
            SegmentAttemptOutcome::Completed(n) => {
                written += n;
                if written >= expected {
                    return Ok(written);
                }
                // Stream ended before delivering the full range -- treat
                // like any other failure for retry-counting purposes.
                attempt += 1;
                if attempt > config.segment_retries {
                    return Err(format!(
                        "segment [{start},{end_inclusive}] incomplete after {attempt} attempt(s): {written}/{expected} bytes"
                    ));
                }
                eprintln!(
                    "soldr: segment [{start},{end_inclusive}] short read, retry {attempt}/{} (resuming at byte {})",
                    config.segment_retries,
                    start + written
                );
            }
            SegmentAttemptOutcome::Preempted => {
                eprintln!(
                    "soldr: segment [{start},{end_inclusive}] preempted before its first byte; re-queueing (retry budget untouched)"
                );
                continue;
            }
            SegmentAttemptOutcome::Failed(n, reason) => {
                written += n;
                attempt += 1;
                if attempt > config.segment_retries {
                    return Err(format!(
                        "segment [{start},{end_inclusive}] failed after {attempt} attempt(s), {written}/{expected} bytes written: {reason}"
                    ));
                }
                eprintln!(
                    "soldr: segment [{start},{end_inclusive}] retry {attempt}/{} (resuming at byte {}): {reason}",
                    config.segment_retries,
                    start + written
                );
            }
        }
    }
}

/// One attempt at fetching `[start, end_inclusive]`. Acquires a Bulk-pool
/// permit first (so pool-wait time counts against no clock), races the
/// connect phase against preemption while PENDING, transitions to
/// STREAMING before the body loop (never preemptible from there), and
/// applies the stall watchdog (clock 2) to every chunk read.
///
/// Eight narrow, independently-meaningful parameters (byte range, two
/// timeouts, the pool, etc.) rather than a bundling struct: every caller
/// (production `fetch_segment_with_retries` and several direct unit
/// tests exercising one clock/pool behavior at a time) wants to vary a
/// different subset, and a struct would just move the same argument
/// count to construction call sites instead of removing it.
#[allow(clippy::too_many_arguments)]
async fn fetch_segment_once(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end_inclusive: u64,
    file: &std::fs::File,
    stall_timeout: Duration,
    connect_timeout: Duration,
    pool: &Arc<SocketPool>,
) -> SegmentAttemptOutcome {
    let mut permit = pool.acquire().await;

    let range_value = format!("bytes={start}-{end_inclusive}");
    let token = auth_token();
    let build_request = |client: &reqwest::Client, url: &str| {
        let mut req = client
            .get(url)
            .header(reqwest::header::RANGE, range_value.clone());
        if let Some(t) = &token {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
        }
        req
    };

    let send_fut = send_with_hop_timeout(client, url, build_request, connect_timeout);
    let mut resp = match permit.race_preemption(send_fut).await {
        Err(()) => return SegmentAttemptOutcome::Preempted,
        Ok(Err(reason)) => return SegmentAttemptOutcome::Failed(0, reason),
        Ok(Ok(r)) => r,
    };
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT && !resp.status().is_success() {
        return SegmentAttemptOutcome::Failed(0, format!("HTTP {}", resp.status()));
    }

    // First payload byte is imminent: PENDING -> STREAMING, permanently
    // non-preemptible from here (module docs "Permit preemption").
    permit.mark_streaming();

    let mut offset = start;
    let mut written = 0u64;
    loop {
        let chunk = match tokio::time::timeout(stall_timeout, resp.chunk()).await {
            Ok(Ok(Some(c))) => c,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                return SegmentAttemptOutcome::Failed(written, format!("read failed: {e}"))
            }
            Err(_) => {
                return SegmentAttemptOutcome::Failed(
                    written,
                    format!("stalled: no progress for {stall_timeout:?}"),
                )
            }
        };
        if let Err(e) = write_at_all(file, &chunk, offset) {
            return SegmentAttemptOutcome::Failed(written, format!("write failed: {e}"));
        }
        offset += chunk.len() as u64;
        written += chunk.len() as u64;
    }
    SegmentAttemptOutcome::Completed(written)
    // `permit` drops here on every path (RAII) -- this is the permit-leak
    // guarantee for stall-watchdog kills, preemption, and normal returns
    // alike.
}

fn sha256_of_file(file: &std::fs::File) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut reader = file.try_clone()?;
    reader.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

// ---- positional (pwrite/seek_write) file I/O so N concurrent segment
// ---- tasks can share one file handle without a shared cursor. ----

#[cfg(unix)]
fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(windows)]
fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

fn write_at_all(file: &std::fs::File, mut buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        let n = write_at(file, buf, offset)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "write_at wrote 0 bytes",
            ));
        }
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

/// Split `[0, total)` into `n` non-overlapping, contiguous segments,
/// distributing the `total % n` remainder one byte at a time to the first
/// segments. Every byte in `[0, total)` is covered by exactly one segment;
/// no segment is ever empty when `total >= n`.
fn compute_segments(total: u64, n: u32) -> Vec<(u64, u64)> {
    if total == 0 || n == 0 {
        return Vec::new();
    }
    let n = n as u64;
    let base = total / n;
    let remainder = total % n;
    let mut segments = Vec::with_capacity(n as usize);
    let mut cursor = 0u64;
    for i in 0..n {
        let len = base + if i < remainder { 1 } else { 0 };
        if len == 0 {
            continue;
        }
        let start = cursor;
        let end_inclusive = start + len - 1;
        segments.push((start, end_inclusive));
        cursor += len;
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve_chunks(chunks: Vec<(Vec<u8>, Duration)>, content_length: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
        let address = listener.local_addr().expect("server address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept client");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("write headers");
            for (chunk, pause_after) in chunks {
                socket.write_all(&chunk).await.expect("write chunk");
                tokio::time::sleep(pause_after).await;
            }
            let _ = socket.shutdown().await;
        });
        format!("http://{address}/asset")
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime")
    }

    /// Serialised because these tests mutate process env (segmented-
    /// download knobs).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_segmented_env() {
        for var in [
            SEGMENTED_DOWNLOAD_ENV_VAR,
            SEGMENTED_DOWNLOAD_N_ENV_VAR,
            CONNECT_TIMEOUT_ENV_VAR,
            STALL_TIMEOUT_ENV_VAR,
            SEGMENT_RETRIES_ENV_VAR,
            GLOBAL_TIMEOUT_ENV_VAR,
            MAX_SOCKETS_ENV_VAR,
            QUICK_POOL_ENV_VAR,
            QUICK_THRESHOLD_ENV_VAR,
            AUTH_TOKEN_ENV_VAR,
        ] {
            std::env::remove_var(var);
        }
    }

    crate::timed_test!(
        healthy_chunks_reset_the_idle_watchdog,
        Duration::from_secs(5),
        {
            runtime().block_on(async {
                let idle = Duration::from_millis(100);
                let url = serve_chunks(
                    vec![
                        (b"a".to_vec(), Duration::from_millis(55)),
                        (b"b".to_vec(), Duration::from_millis(55)),
                        (b"c".to_vec(), Duration::from_millis(55)),
                        (b"d".to_vec(), Duration::from_millis(55)),
                    ],
                    4,
                )
                .await;
                let started = Instant::now();
                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                let asset = stream_response_to_temp_file(response, &url, idle)
                    .await
                    .expect("progressing transfer succeeds");
                assert!(
                    started.elapsed() > idle,
                    "transfer must outlive one idle interval"
                );
                assert_eq!(asset.bytes(), 4);
                assert_eq!(asset.sha256(), super::super::trust::sha256_of(b"abcd"));
            });
        }
    );

    crate::timed_test!(
        idle_pause_reports_bytes_and_is_transient,
        Duration::from_secs(5),
        {
            runtime().block_on(async {
                let idle = Duration::from_millis(40);
                let url =
                    serve_chunks(vec![(b"partial".to_vec(), Duration::from_millis(120))], 12).await;
                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                let error = stream_response_to_temp_file(response, &url, idle)
                    .await
                    .expect_err("paused body must fail");
                assert!(super::super::retry::is_transient(&error));
                assert!(error.to_string().contains("7 bytes"), "{error}");
                assert!(error.to_string().contains("no progress"), "{error}");
            });
        }
    );

    crate::timed_test!(truncated_body_is_transient, Duration::from_secs(5), {
        runtime().block_on(async {
            let url = serve_chunks(vec![(b"short".to_vec(), Duration::ZERO)], 12).await;
            let response = reqwest::Client::new().get(&url).send().await.expect("GET");
            let error = stream_response_to_temp_file(response, &url, Duration::from_secs(1))
                .await
                .expect_err("truncated body must fail");
            assert!(super::super::retry::is_transient(&error));
            assert!(error.to_string().contains("5 bytes"), "{error}");
        });
    });

    crate::timed_test!(
        global_safety_ceiling_stops_a_slow_but_progressing_transfer,
        Duration::from_secs(5),
        {
            runtime().block_on(async {
                let url = serve_chunks(
                    vec![
                        (b"a".to_vec(), Duration::from_millis(30)),
                        (b"b".to_vec(), Duration::from_millis(30)),
                        (b"c".to_vec(), Duration::from_millis(30)),
                    ],
                    3,
                )
                .await;
                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                let error = stream_response_to_temp_file_with_safety_timeout(
                    response,
                    &url,
                    Duration::from_millis(100),
                    Duration::from_millis(50),
                )
                .await
                .expect_err("global ceiling must stop the transfer");
                assert!(super::super::retry::is_transient(&error));
                assert!(
                    error.to_string().contains("global safety ceiling"),
                    "{error}"
                );
            });
        }
    );

    crate::timed_test!(
        header_timeout_is_separate_from_body_idle_timeout,
        Duration::from_secs(5),
        {
            runtime().block_on(async {
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
                let address = listener.local_addr().expect("server address");
                tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.expect("accept client");
                    let mut request = [0_u8; 1024];
                    let _ = socket.read(&mut request).await;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await;
                });
                let url = format!("http://{address}/slow-headers");
                let error = send_asset_request(
                    reqwest::Client::new().get(&url),
                    &url,
                    Duration::from_millis(20),
                )
                .await
                .expect_err("slow headers must fail before the body starts");
                assert!(super::super::retry::is_transient(&error));
                assert!(error.to_string().contains("waiting for headers"), "{error}");
            });
        }
    );

    // ---- segment-plan math ----

    fn assert_exact_coverage(total: u64, segments: &[(u64, u64)]) {
        assert!(!segments.is_empty(), "must produce at least one segment");
        let mut expected_start = 0u64;
        for &(start, end_inclusive) in segments {
            assert_eq!(
                start, expected_start,
                "segment must start where the previous ended"
            );
            assert!(end_inclusive >= start, "segment must be non-empty");
            expected_start = end_inclusive + 1;
        }
        assert_eq!(
            expected_start, total,
            "segments must cover exactly [0, total) with no gap and no overrun"
        );
    }

    crate::timed_test!(segments_cover_exact_range_evenly_divisible, {
        let segments = compute_segments(1000, 4);
        assert_eq!(segments.len(), 4);
        assert_exact_coverage(1000, &segments);
        for &(start, end_inclusive) in &segments {
            assert_eq!(end_inclusive - start + 1, 250);
        }
    });

    crate::timed_test!(segments_cover_exact_range_with_remainder, {
        let segments = compute_segments(1000, 3);
        assert_eq!(segments.len(), 3);
        assert_exact_coverage(1000, &segments);
        let lens: Vec<u64> = segments.iter().map(|&(s, e)| e - s + 1).collect();
        assert_eq!(lens, vec![334, 333, 333]);
    });

    crate::timed_test!(segments_never_overlap_across_many_n, {
        for total in [1u64, 2, 7, 4096, 84_664_072, 108_209_048, 192_470_485] {
            for n in [2u32, 3, 4, 8, 16] {
                let segments = compute_segments(total, n);
                assert_exact_coverage(total, &segments);
                assert!(segments.len() as u32 <= n, "total={total} n={n}");
            }
        }
    });

    crate::timed_test!(zero_total_or_zero_n_produces_no_segments, {
        assert!(compute_segments(0, 4).is_empty());
        assert!(compute_segments(1000, 0).is_empty());
    });

    crate::timed_test!(more_segments_than_bytes_collapses_without_empty_segments, {
        let segments = compute_segments(3, 8);
        assert_exact_coverage(3, &segments);
        assert!(segments.len() <= 3);
        for &(start, end_inclusive) in &segments {
            assert_eq!(end_inclusive - start + 1, 1);
        }
    });

    // ---- config parsing: defaults, overrides, junk-fails-safe ----

    crate::timed_test!(opt_out_recognizes_common_spellings_default_is_enabled, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();

        assert!(
            !opted_out(SEGMENTED_DOWNLOAD_ENV_VAR),
            "unset must leave segmentation enabled (default-on posture)"
        );
        for falsy in ["off", "0", "false", "no", "OFF", "False", " off "] {
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, falsy);
            assert!(
                opted_out(SEGMENTED_DOWNLOAD_ENV_VAR),
                "{falsy:?} must opt out"
            );
        }
        for other in ["1", "true", "yes", "on", "garbage"] {
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, other);
            assert!(
                !opted_out(SEGMENTED_DOWNLOAD_ENV_VAR),
                "{other:?} must NOT opt out -- only the documented falsy spellings do"
            );
        }
        clear_segmented_env();
    });

    crate::timed_test!(default_segment_count_is_sixteen, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();
        assert_eq!(
            parse_segment_count(),
            16,
            "default N must be 16 per the maintainer's plateau-not-found decision"
        );
        assert_eq!(DEFAULT_SEGMENT_COUNT, 16);
        assert_eq!(MAX_SEGMENTS, 16);
        clear_segmented_env();
    });

    crate::timed_test!(segment_count_env_override_is_clamped_and_fails_safe, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();

        std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "1");
        assert_eq!(
            parse_segment_count(),
            DEFAULT_SEGMENT_COUNT,
            "below-minimum falls back to default"
        );
        std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "4");
        assert_eq!(parse_segment_count(), 4);
        std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "9999");
        assert_eq!(
            parse_segment_count(),
            MAX_SEGMENTS,
            "above-maximum clamps to MAX_SEGMENTS"
        );
        std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "not-a-number");
        assert_eq!(
            parse_segment_count(),
            DEFAULT_SEGMENT_COUNT,
            "junk falls back to default"
        );
        clear_segmented_env();
        assert_eq!(
            parse_segment_count(),
            DEFAULT_SEGMENT_COUNT,
            "unset falls back to default"
        );
    });

    crate::timed_test!(connect_timeout_defaults_to_ten_seconds_and_fails_safe, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();
        assert_eq!(parse_connect_timeout(), Duration::from_secs(10));

        std::env::set_var(CONNECT_TIMEOUT_ENV_VAR, "3");
        assert_eq!(parse_connect_timeout(), Duration::from_secs(3));
        std::env::set_var(CONNECT_TIMEOUT_ENV_VAR, "0");
        assert_eq!(
            parse_connect_timeout(),
            Duration::from_secs(10),
            "0 is not meaningful -- fails safe to default"
        );
        std::env::set_var(CONNECT_TIMEOUT_ENV_VAR, "nope");
        assert_eq!(parse_connect_timeout(), Duration::from_secs(10));
        clear_segmented_env();
    });

    crate::timed_test!(stall_timeout_defaults_to_thirty_seconds_and_fails_safe, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();
        assert_eq!(parse_stall_timeout(), Duration::from_secs(30));

        std::env::set_var(STALL_TIMEOUT_ENV_VAR, "5");
        assert_eq!(parse_stall_timeout(), Duration::from_secs(5));
        std::env::set_var(STALL_TIMEOUT_ENV_VAR, "0");
        assert_eq!(
            parse_stall_timeout(),
            Duration::from_secs(30),
            "an explicit 0 is not a meaningful watchdog -- fails safe to default"
        );
        std::env::set_var(STALL_TIMEOUT_ENV_VAR, "banana");
        assert_eq!(parse_stall_timeout(), Duration::from_secs(30));
        clear_segmented_env();
    });

    crate::timed_test!(segment_retries_defaults_to_three_and_fails_safe, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();
        assert_eq!(parse_segment_retries(), 3);

        std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "0");
        assert_eq!(
            parse_segment_retries(),
            0,
            "0 is a legitimate 'no retries' value, not junk"
        );
        std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "9999");
        assert_eq!(parse_segment_retries(), MAX_SEGMENT_RETRIES);
        std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "nope");
        assert_eq!(parse_segment_retries(), 3);
        clear_segmented_env();
    });

    crate::timed_test!(global_timeout_disabled_by_default_and_fails_safe, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();
        assert_eq!(parse_global_timeout(), None, "disabled by default");

        std::env::set_var(GLOBAL_TIMEOUT_ENV_VAR, "0");
        assert_eq!(parse_global_timeout(), None, "0 fails safe to disabled");
        std::env::set_var(GLOBAL_TIMEOUT_ENV_VAR, "junk");
        assert_eq!(parse_global_timeout(), None, "junk fails safe to disabled");
        std::env::set_var(GLOBAL_TIMEOUT_ENV_VAR, "45");
        assert_eq!(parse_global_timeout(), Some(Duration::from_secs(45)));
        clear_segmented_env();
    });

    crate::timed_test!(max_sockets_defaults_to_sixteen_and_fails_safe, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();
        assert_eq!(parse_max_sockets(), 16);

        std::env::set_var(MAX_SOCKETS_ENV_VAR, "4");
        assert_eq!(parse_max_sockets(), 4);
        std::env::set_var(MAX_SOCKETS_ENV_VAR, "0");
        assert_eq!(
            parse_max_sockets(),
            16,
            "0 is not meaningful -- fails safe to default"
        );
        std::env::set_var(MAX_SOCKETS_ENV_VAR, "banana");
        assert_eq!(parse_max_sockets(), 16);
        clear_segmented_env();
    });

    crate::timed_test!(quick_pool_size_defaults_to_four_and_fails_safe, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();
        assert_eq!(parse_quick_pool_size(), 4);

        std::env::set_var(QUICK_POOL_ENV_VAR, "8");
        assert_eq!(parse_quick_pool_size(), 8);
        std::env::set_var(QUICK_POOL_ENV_VAR, "0");
        assert_eq!(parse_quick_pool_size(), 4, "0 fails safe to default");
        std::env::set_var(QUICK_POOL_ENV_VAR, "nope");
        assert_eq!(parse_quick_pool_size(), 4);
        clear_segmented_env();
    });

    crate::timed_test!(quick_threshold_defaults_and_fails_safe, {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_segmented_env();
        // cfg(test) default is intentionally small -- see the constant's docs.
        assert_eq!(parse_quick_threshold(), DEFAULT_QUICK_THRESHOLD_BYTES);

        std::env::set_var(QUICK_THRESHOLD_ENV_VAR, "1024");
        assert_eq!(parse_quick_threshold(), 1024);
        std::env::set_var(QUICK_THRESHOLD_ENV_VAR, "0");
        assert_eq!(
            parse_quick_threshold(),
            DEFAULT_QUICK_THRESHOLD_BYTES,
            "0 fails safe to default"
        );
        std::env::set_var(QUICK_THRESHOLD_ENV_VAR, "not-a-number");
        assert_eq!(parse_quick_threshold(), DEFAULT_QUICK_THRESHOLD_BYTES);
        clear_segmented_env();
    });

    // ---- end-to-end segmented behavior against a local mock server ----

    fn parse_range_header(request: &str) -> Option<(u64, u64)> {
        for line in request.lines() {
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("range: bytes=") {
                let mut parts = rest.trim().splitn(2, '-');
                let start: u64 = parts.next()?.parse().ok()?;
                let end: u64 = parts.next()?.parse().ok()?;
                return Some((start, end));
            }
        }
        None
    }

    /// Serves `body` for segmented-download tests:
    /// - a plain (no-Range) GET gets `Accept-Ranges: bytes` + the full
    ///   correct body, so the caller's `response` both triggers
    ///   segmentation AND remains a valid single-stream fallback source.
    /// - a Range GET for the FIRST attempt at `bytes=0-*` (i.e. the very
    ///   start of segment 0) delivers 2 bytes then hangs forever without
    ///   closing -- this must trip the stall watchdog. Any Range GET NOT
    ///   starting at byte 0 (i.e. a resume after that stall) is served
    ///   normally, proving retries resume from the correct offset instead
    ///   of re-requesting the whole segment.
    /// - every other Range GET is served normally and immediately.
    async fn serve_stalling_then_recovering(
        body: Vec<u8>,
    ) -> (String, Arc<Mutex<Vec<(u64, u64)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let seen_ranges: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let body = Arc::new(body);
        let stalled_zero_start_once = Arc::new(Mutex::new(false));

        let seen_for_task = Arc::clone(&seen_ranges);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = Arc::clone(&body);
                let seen = Arc::clone(&seen_for_task);
                let stalled_zero_start_once = Arc::clone(&stalled_zero_start_once);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let range = parse_range_header(&request);

                    let Some((s, e)) = range else {
                        // Plain GET: the caller's initial request.
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                        let _ = socket.shutdown().await;
                        return;
                    };

                    seen.lock().unwrap().push((s, e));
                    let total = body.len();
                    let slice = &body[s as usize..=(e as usize).min(total - 1)];

                    let should_stall = {
                        let mut stalled_guard = stalled_zero_start_once.lock().unwrap();
                        let should_stall = s == 0 && !*stalled_guard;
                        if should_stall {
                            *stalled_guard = true;
                        }
                        should_stall
                    };
                    if should_stall {
                        let header = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(&slice[..2.min(slice.len())]).await;
                        // Hang well past the test's stall timeout without
                        // closing -- this is what must trip the watchdog.
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                        return;
                    }

                    let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        slice.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(slice).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        (format!("http://{address}/asset"), seen_ranges)
    }

    crate::timed_test!(
        stalling_segment_trips_watchdog_recovers_and_resumes_from_offset,
        Duration::from_secs(10),
        {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            clear_segmented_env();
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
            std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "2");
            std::env::set_var(STALL_TIMEOUT_ENV_VAR, "1");
            std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "2");

            runtime().block_on(async {
                let body: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
                let (url, seen_ranges) = serve_stalling_then_recovering(body.clone()).await;

                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                let asset = stream_response_to_temp_file(response, &url, Duration::from_secs(5))
                    .await
                    .expect("stalled segment must recover via retry, not fail the download");

                let expected_sha = super::super::trust::sha256_of(&body);
                assert_eq!(asset.sha256(), expected_sha, "assembled file must match the source exactly");
                assert_eq!(asset.bytes(), body.len() as u64);

                let ranges = seen_ranges.lock().unwrap().clone();
                assert!(
                    ranges.iter().any(|&(s, _)| s == 0),
                    "the initial (stalling) request for segment 0 must have been observed: {ranges:?}"
                );
                assert!(
                    ranges.iter().any(|&(s, _)| s == 2),
                    "the retry must resume at byte 2 (only the missing tail), not restart at 0: {ranges:?}"
                );
                assert_eq!(
                    ranges.iter().filter(|&&(s, _)| s == 0).count(),
                    1,
                    "segment 0 must be requested from byte 0 exactly once (the stalling attempt); \
                     every subsequent request must resume, never restart from 0: {ranges:?}"
                );
            });

            clear_segmented_env();
        }
    );

    /// A server whose plain GET advertises Range support (and serves the
    /// correct full body, for the fallback path) but whose EVERY Range GET
    /// fails outright -- this must exhaust the per-segment retry budget
    /// and fall all the way through to draining the original response.
    async fn serve_range_always_failing(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let body = Arc::new(body);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = Arc::clone(&body);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    if parse_range_header(&request).is_some() {
                        let _ = socket
                            .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        let _ = socket.shutdown().await;
                        return;
                    }
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{address}/asset")
    }

    crate::timed_test!(
        segment_retry_exhaustion_falls_back_to_single_stream,
        Duration::from_secs(10),
        {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            clear_segmented_env();
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
            std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "2");
            std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "1");
            std::env::set_var(STALL_TIMEOUT_ENV_VAR, "2");

            runtime().block_on(async {
                let body: Vec<u8> = (0..1024u32).map(|i| (i % 191) as u8).collect();
                let url = serve_range_always_failing(body.clone()).await;

                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                let asset = stream_response_to_temp_file(response, &url, Duration::from_secs(5))
                    .await
                    .expect(
                        "every Range request failing must still resolve via single-stream fallback",
                    );

                assert_eq!(asset.bytes(), body.len() as u64);
                assert_eq!(asset.sha256(), super::super::trust::sha256_of(&body));
            });

            clear_segmented_env();
        }
    );

    crate::timed_test!(
        global_timeout_expiry_with_no_budget_surfaces_a_clear_error,
        Duration::from_secs(10),
        {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            clear_segmented_env();
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
            std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "2");
            std::env::set_var(STALL_TIMEOUT_ENV_VAR, "30");
            std::env::set_var(SEGMENT_RETRIES_ENV_VAR, "0");
            // Smaller than MEANINGFUL_FALLBACK_MIN (5s), so expiry must
            // surface the hard timeout error, not attempt a fallback.
            std::env::set_var(GLOBAL_TIMEOUT_ENV_VAR, "1");

            runtime().block_on(async {
                // Every segment stalls forever (server never responds to
                // Range requests at all -- just accepts and hangs), so the
                // 1s global timeout is what ends the attempt.
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let address = listener.local_addr().expect("addr");
                let body = vec![7u8; 1024];
                let body_for_task = body.clone();
                tokio::spawn(async move {
                    loop {
                        let Ok((mut socket, _)) = listener.accept().await else {
                            return;
                        };
                        let body = body_for_task.clone();
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 4096];
                            let n = socket.read(&mut buf).await.unwrap_or(0);
                            let request = String::from_utf8_lossy(&buf[..n]).to_string();
                            if parse_range_header(&request).is_some() {
                                // Accept the connection but never respond --
                                // simulates a fully stalled segment.
                                tokio::time::sleep(Duration::from_secs(3600)).await;
                                return;
                            }
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = socket.write_all(header.as_bytes()).await;
                            let _ = socket.write_all(&body).await;
                            let _ = socket.shutdown().await;
                        });
                    }
                });
                let url = format!("http://{address}/asset");

                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                let error = stream_response_to_temp_file(response, &url, Duration::from_secs(30))
                    .await
                    .expect_err("global timeout with no remaining budget must be a hard error");
                let message = error.to_string();
                assert!(
                    message.contains(GLOBAL_TIMEOUT_ENV_VAR),
                    "error must name the env var that controls this deadline: {message}"
                );
            });

            clear_segmented_env();
        }
    );

    crate::timed_test!(
        segmentation_never_attempted_when_response_lacks_accept_ranges,
        {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            clear_segmented_env();
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");

            runtime().block_on(async {
                // Reuses the plain `serve_chunks` helper, whose response has
                // no Accept-Ranges header -- segmentation must be skipped
                // entirely and the existing single-stream path must run.
                let url = serve_chunks(vec![(b"abcd".to_vec(), Duration::ZERO)], 4).await;
                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                let asset = stream_response_to_temp_file(response, &url, Duration::from_secs(5))
                    .await
                    .expect("must succeed via the untouched single-stream path");
                assert_eq!(asset.bytes(), 4);
            });

            clear_segmented_env();
        }
    );

    // ---- threshold routing: quick vs bulk, quick == never segmented ----

    fn response_headers(accept_ranges: bool, content_length: Option<u64>) -> String {
        let mut headers = String::from("HTTP/1.1 200 OK\r\n");
        if accept_ranges {
            headers.push_str("Accept-Ranges: bytes\r\n");
        }
        if let Some(len) = content_length {
            headers.push_str(&format!("Content-Length: {len}\r\n"));
        }
        headers.push_str("Connection: close\r\n\r\n");
        headers
    }

    async fn serve_fixed_response(status_and_headers: String, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(status_and_headers.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{address}/asset")
    }

    crate::timed_test!(
        response_at_or_below_threshold_is_never_segmentable,
        Duration::from_secs(5),
        {
            runtime().block_on(async {
                let threshold = DEFAULT_QUICK_THRESHOLD_BYTES;
                let body = vec![1u8; threshold as usize];
                let url = serve_fixed_response(response_headers(true, Some(threshold)), body).await;
                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                assert!(
                    segmentable_total_len(&response, threshold).is_none(),
                    "Content-Length exactly at the threshold must NOT be segmented"
                );
            });
        }
    );

    crate::timed_test!(
        response_above_threshold_is_segmentable,
        Duration::from_secs(5),
        {
            runtime().block_on(async {
                let threshold = DEFAULT_QUICK_THRESHOLD_BYTES;
                let total = threshold + 1;
                let body = vec![1u8; total as usize];
                let url = serve_fixed_response(response_headers(true, Some(total)), body).await;
                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                assert_eq!(
                    segmentable_total_len(&response, threshold),
                    Some(total),
                    "Content-Length just above the threshold must be segmentable"
                );
            });
        }
    );

    crate::timed_test!(
        unknown_size_response_is_never_segmentable,
        Duration::from_secs(5),
        {
            runtime().block_on(async {
                // No Content-Length header at all (server signals end via
                // connection close instead).
                let url = serve_fixed_response(
                    "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                        .to_string(),
                    vec![1u8; 64],
                )
                .await;
                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                assert!(
                    segmentable_total_len(&response, DEFAULT_QUICK_THRESHOLD_BYTES).is_none(),
                    "unknown size must never be treated as segmentable"
                );
            });
        }
    );

    // ---- socket pools: bounded concurrency, sharing, permit-leak safety ----

    /// A server that answers any Range GET after `body` is served for a
    /// plain GET, tracking concurrent-connection high-water-mark via an
    /// atomic counter incremented on accept and decremented once the
    /// response is fully written.
    async fn serve_range_tracking_concurrency(
        body: Vec<u8>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let body = Arc::new(body);
        let current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let high_water = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hw_for_task = Arc::clone(&high_water);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = Arc::clone(&body);
                let current = Arc::clone(&current);
                let high_water = Arc::clone(&hw_for_task);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();

                    let Some((s, e)) = parse_range_header(&request) else {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                        let _ = socket.shutdown().await;
                        return;
                    };

                    let now = current.fetch_add(1, Ordering::SeqCst) + 1;
                    high_water.fetch_max(now, Ordering::SeqCst);

                    // Hold the connection open briefly so overlapping
                    // segment requests actually overlap in wall-clock time.
                    tokio::time::sleep(Duration::from_millis(80)).await;

                    let total = body.len();
                    let slice = &body[s as usize..=(e as usize).min(total - 1)];
                    let header = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {s}-{e}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        slice.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(slice).await;
                    let _ = socket.shutdown().await;
                    current.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        (format!("http://{address}/asset"), high_water)
    }

    crate::timed_test!(
        pool_bounds_concurrent_segment_connections,
        Duration::from_secs(15),
        {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            clear_segmented_env();
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
            std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "4");

            runtime().block_on(async {
                let body: Vec<u8> = (0..4096u32).map(|i| (i % 233) as u8).collect();
                let (url, high_water) = serve_range_tracking_concurrency(body.clone()).await;
                let pool = SocketPool::new(2);

                let response = reqwest::Client::new().get(&url).send().await.expect("GET");
                let asset = stream_response_to_temp_file_with_pool(
                    response,
                    &url,
                    Duration::from_secs(5),
                    ASSET_SAFETY_TIMEOUT,
                    pool,
                )
                .await
                .expect("4-segment plan against a size-2 pool must still complete");

                assert_eq!(asset.sha256(), super::super::trust::sha256_of(&body));
                assert!(
                    high_water.load(Ordering::SeqCst) <= 2,
                    "observed concurrency {} must never exceed the pool size 2",
                    high_water.load(Ordering::SeqCst)
                );
            });

            clear_segmented_env();
        }
    );

    crate::timed_test!(
        two_concurrent_downloads_share_one_pool,
        Duration::from_secs(15),
        {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            clear_segmented_env();
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
            std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "2");

            runtime().block_on(async {
                let body_a: Vec<u8> = (0..2048u32).map(|i| (i % 197) as u8).collect();
                let body_b: Vec<u8> = (0..2048u32).map(|i| (i % 199) as u8).collect();
                let (url_a, hw_a) = serve_range_tracking_concurrency(body_a.clone()).await;
                let (url_b, hw_b) = serve_range_tracking_concurrency(body_b.clone()).await;
                let pool = SocketPool::new(2);

                let resp_a = reqwest::Client::new()
                    .get(&url_a)
                    .send()
                    .await
                    .expect("GET a");
                let resp_b = reqwest::Client::new()
                    .get(&url_b)
                    .send()
                    .await
                    .expect("GET b");

                let fut_a = stream_response_to_temp_file_with_pool(
                    resp_a,
                    &url_a,
                    Duration::from_secs(5),
                    ASSET_SAFETY_TIMEOUT,
                    Arc::clone(&pool),
                );
                let fut_b = stream_response_to_temp_file_with_pool(
                    resp_b,
                    &url_b,
                    Duration::from_secs(5),
                    ASSET_SAFETY_TIMEOUT,
                    Arc::clone(&pool),
                );
                let (asset_a, asset_b) = tokio::join!(fut_a, fut_b);
                let asset_a = asset_a.expect("download a must complete");
                let asset_b = asset_b.expect("download b must complete");

                assert_eq!(asset_a.sha256(), super::super::trust::sha256_of(&body_a));
                assert_eq!(asset_b.sha256(), super::super::trust::sha256_of(&body_b));
                assert!(
                    hw_a.load(Ordering::SeqCst) <= 2 && hw_b.load(Ordering::SeqCst) <= 2,
                    "neither server should ever see more than the pool's total capacity in flight"
                );
            });

            clear_segmented_env();
        }
    );

    /// A server that never responds to a Range GET at all (accepts, then
    /// hangs), so `fetch_segment_once` must trip the stall watchdog and
    /// return a failure -- exercising the RAII permit-drop path.
    async fn serve_range_hangs_forever() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                });
            }
        });
        format!("http://{address}/asset")
    }

    crate::timed_test!(
        stalled_segment_retry_does_not_leak_pool_permits,
        Duration::from_secs(10),
        {
            runtime().block_on(async {
                let url = serve_range_hangs_forever().await;
                let pool = SocketPool::new(1);
                let client = reqwest::Client::new();

                assert_eq!(pool.available(), 1);
                let result = fetch_segment_with_retries(
                    &client,
                    &url,
                    0,
                    9,
                    &tempfile::tempfile().expect("tempfile"),
                    SegmentedDownloadConfig {
                        enabled: true,
                        segment_count: 1,
                        connect_timeout: Duration::from_millis(300),
                        stall_timeout: Duration::from_millis(300),
                        segment_retries: 2,
                        global_timeout: None,
                    },
                    Arc::clone(&pool),
                )
                .await;

                assert!(result.is_err(), "a segment that always stalls must exhaust its retries");
                assert_eq!(
                    pool.available(),
                    1,
                    "every stall+retry cycle must release its permit -- the pool must not shrink over retries"
                );
            });
        }
    );

    // ---- three clocks: connect/TTFB, redirect-hop reset, slow-header/fast-body ----

    crate::timed_test!(hung_connect_trips_connect_timeout_and_releases_permit, {
        runtime().block_on(async {
            let url = serve_range_hangs_forever().await;
            let pool = SocketPool::new(1);
            let client = reqwest::Client::new();

            assert_eq!(pool.available(), 1);
            let outcome = fetch_segment_once(
                &client,
                &url,
                0,
                9,
                &tempfile::tempfile().expect("tempfile"),
                Duration::from_secs(30),
                Duration::from_millis(300),
                &pool,
            )
            .await;

            match outcome {
                SegmentAttemptOutcome::Failed(0, reason) => {
                    assert!(
                        reason.contains("TTFB") || reason.contains("connect"),
                        "reason should name the connect/TTFB phase: {reason}"
                    );
                }
                _ => panic!("a server that never responds must trip the connect/TTFB clock"),
            }
            assert_eq!(
                pool.available(),
                1,
                "the permit must be released after the connect timeout"
            );
        });
    });

    crate::timed_test!(slow_header_fast_body_succeeds, Duration::from_secs(10), {
        runtime().block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = listener.local_addr().expect("addr");
            let body = vec![9u8; 64];
            let body_for_task = body.clone();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(400)).await;
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-63/64\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_for_task.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body_for_task).await;
                let _ = socket.shutdown().await;
            });
            let url = format!("http://{address}/asset");
            let pool = SocketPool::new(1);
            let client = reqwest::Client::new();

            let outcome = fetch_segment_once(
                &client,
                &url,
                0,
                63,
                &tempfile::tempfile().expect("tempfile"),
                Duration::from_secs(5),
                Duration::from_secs(2),
                &pool,
            )
            .await;
            match outcome {
                SegmentAttemptOutcome::Completed(n) => assert_eq!(n, 64),
                _ => panic!("slow headers within budget followed by a fast body must succeed"),
            }
        });
    });

    crate::timed_test!(
        redirect_hop_resets_connect_clock,
        Duration::from_secs(15),
        {
            runtime().block_on(async {
            let listener2 = TcpListener::bind("127.0.0.1:0").await.expect("bind hop2");
            let address2 = listener2.local_addr().expect("addr2");
            let body = vec![5u8; 32];
            let body_for_task = body.clone();
            tokio::spawn(async move {
                let (mut socket, _) = listener2.accept().await.expect("accept hop2");
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(1200)).await;
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-31/32\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_for_task.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body_for_task).await;
                let _ = socket.shutdown().await;
            });
            let hop2_url = format!("http://{address2}/asset");

            let listener1 = TcpListener::bind("127.0.0.1:0").await.expect("bind hop1");
            let address1 = listener1.local_addr().expect("addr1");
            tokio::spawn(async move {
                let (mut socket, _) = listener1.accept().await.expect("accept hop1");
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(1200)).await;
                let header = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {hop2_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
            let url = format!("http://{address1}/asset");
            let pool = SocketPool::new(1);
            // A default reqwest::Client auto-follows redirects, which would
            // swallow both hops into a single `.send()` call and defeat the
            // point of this test (send_with_hop_timeout's manual redirect
            // handling only matters when the client itself does NOT
            // auto-follow -- exactly the client segmented_http_client
            // builds in production).
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build no-redirect client");

            let outcome = fetch_segment_once(
                &client,
                &url,
                0,
                31,
                &tempfile::tempfile().expect("tempfile"),
                Duration::from_secs(5),
                Duration::from_secs(2),
                &pool,
            )
            .await;
            match outcome {
                SegmentAttemptOutcome::Completed(n) => assert_eq!(n, 32),
                SegmentAttemptOutcome::Failed(_, reason) => {
                    panic!("each redirect hop must get its own fresh connect clock: {reason}")
                }
                SegmentAttemptOutcome::Preempted => panic!("no contention expected in this test"),
            }
        });
        }
    );

    // ---- permit preemption ----

    crate::timed_test!(
        preempted_segment_requeues_without_spending_retry_budget,
        Duration::from_secs(20),
        {
            runtime().block_on(async {
                let listener_a = TcpListener::bind("127.0.0.1:0").await.expect("bind a");
                let address_a = listener_a.local_addr().expect("addr a");
                tokio::spawn(async move {
                    loop {
                        let Ok((mut socket, _)) = listener_a.accept().await else {
                            return;
                        };
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 4096];
                            let _ = socket.read(&mut buf).await;
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        });
                    }
                });

                let body_b = vec![3u8; 16];
                let (url_b, _hw_b) = serve_range_tracking_concurrency(body_b.clone()).await;

                let pool = SocketPool::new(1);
                let client = reqwest::Client::new();
                let url_a = format!("http://{address_a}/asset");

                let pool_a = Arc::clone(&pool);
                let client_a = client.clone();
                let file_a = tempfile::tempfile().expect("tempfile a");
                let seg_a = tokio::spawn(async move {
                    fetch_segment_once(
                        &client_a,
                        &url_a,
                        0,
                        9,
                        &file_a,
                        Duration::from_secs(30),
                        Duration::from_secs(30),
                        &pool_a,
                    )
                    .await
                });

                tokio::time::sleep(Duration::from_millis(100)).await;

                let config = SegmentedDownloadConfig {
                    enabled: true,
                    segment_count: 1,
                    connect_timeout: Duration::from_secs(5),
                    stall_timeout: Duration::from_secs(5),
                    segment_retries: 0,
                    global_timeout: None,
                };
                let file_b = tempfile::tempfile().expect("tempfile b");
                let result_b = fetch_segment_with_retries(
                    &client, &url_b, 0, 15, &file_b, config, Arc::clone(&pool),
                )
                .await;

                assert!(
                    result_b.is_ok(),
                    "B must complete via preemption of A despite zero configured retries: {result_b:?}"
                );

                seg_a.abort();
            });
        }
    );

    crate::timed_test!(
        uniformly_slow_contention_still_completes_not_spins,
        Duration::from_secs(30),
        {
            // Three equally-slow-connecting segments contend for a pool of
            // 1. The anti-livelock guard (max 2 preemptions per PENDING
            // holder) must let the whole batch converge within a bounded
            // time rather than cycling forever. This is a coarse
            // "completes at all" check, not a precise preemption-count
            // assertion -- see the report for why.
            runtime().block_on(async {
                let body: Vec<u8> = (0..600u32).map(|i| (i % 173) as u8).collect();
                let (url, _hw) = serve_range_tracking_concurrency(body.clone()).await;

                let pool = SocketPool::new(1);
                let config = SegmentedDownloadConfig {
                    enabled: true,
                    segment_count: 3,
                    connect_timeout: Duration::from_secs(10),
                    stall_timeout: Duration::from_secs(10),
                    segment_retries: 5,
                    global_timeout: None,
                };
                let client = reqwest::Client::new();
                let file = Arc::new(tempfile::tempfile().expect("tempfile"));

                let plan = compute_segments(body.len() as u64, 3);
                let started = Instant::now();
                let result = run_all_segments(client, url, plan, file, config, pool).await;
                assert!(
                    result.is_ok(),
                    "contended-but-uniformly-slow segments must still converge: {result:?}"
                );
                assert!(
                    started.elapsed() < Duration::from_secs(25),
                    "must complete well within the test's own budget, not hang: {:?}",
                    started.elapsed()
                );
            });
        }
    );

    // ---- two pools: the motivating scenario + mixed workload ----

    crate::timed_test!(
        quick_pool_serves_control_requests_while_bulk_pool_is_saturated,
        Duration::from_secs(10),
        {
            runtime().block_on(async {
                let bulk = SocketPool::new(1);
                let quick = SocketPool::new(4);

                // Saturate Bulk with a long-lived STREAMING holder,
                // simulating an in-flight segment that will not finish
                // for the duration of this test.
                let mut bulk_permit = bulk.acquire().await;
                bulk_permit.mark_streaming();

                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let address = listener.local_addr().expect("addr");
                tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.expect("accept");
                    let mut buf = vec![0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = socket.shutdown().await;
                });
                let url = format!("http://{address}/manifest.json");

                let started = Instant::now();
                let resp = send_control_request_with_pool(
                    reqwest::Client::new().get(&url),
                    &url,
                    Duration::from_secs(5),
                    quick,
                )
                .await
                .expect("control request must complete despite bulk saturation");
                assert!(resp.status().is_success());
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "must complete promptly, not queue behind a fully-saturated bulk pool: {:?}",
                    started.elapsed()
                );

                drop(bulk_permit);
                assert_eq!(bulk.available(), 1);
            });
        }
    );

    crate::timed_test!(
        mixed_workload_never_exceeds_bulk_plus_quick_total_capacity,
        Duration::from_secs(15),
        {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            clear_segmented_env();
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
            std::env::set_var(SEGMENTED_DOWNLOAD_N_ENV_VAR, "3");

            runtime().block_on(async {
                // One large (segmentable) download against Bulk, plus
                // several small control-style requests against Quick, all
                // concurrent, hitting servers that each track their own
                // concurrency high-water-mark. The combined observed
                // concurrency on each side must never exceed that pool's
                // own size.
                let bulk = SocketPool::new(2);
                let quick = SocketPool::new(2);

                let big_body: Vec<u8> = (0..3072u32).map(|i| (i % 241) as u8).collect();
                let (big_url, big_hw) = serve_range_tracking_concurrency(big_body.clone()).await;
                let big_resp = reqwest::Client::new()
                    .get(&big_url)
                    .send()
                    .await
                    .expect("GET big");
                let big_fut = stream_response_to_temp_file_with_pool(
                    big_resp,
                    &big_url,
                    Duration::from_secs(5),
                    ASSET_SAFETY_TIMEOUT,
                    Arc::clone(&bulk),
                );

                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind small");
                let address = listener.local_addr().expect("addr small");
                let small_hw = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let small_current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let hw_task = Arc::clone(&small_hw);
                let cur_task = Arc::clone(&small_current);
                tokio::spawn(async move {
                    loop {
                        let Ok((mut socket, _)) = listener.accept().await else {
                            return;
                        };
                        let hw = Arc::clone(&hw_task);
                        let cur = Arc::clone(&cur_task);
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 1024];
                            let _ = socket.read(&mut buf).await;
                            let now = cur.fetch_add(1, Ordering::SeqCst) + 1;
                            hw.fetch_max(now, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(60)).await;
                            let _ = socket
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                                .await;
                            let _ = socket.shutdown().await;
                            cur.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                });
                let small_url = format!("http://{address}/manifest.json");

                let small_handles: Vec<_> = (0..3)
                    .map(|_| {
                        let quick = Arc::clone(&quick);
                        let url = small_url.clone();
                        tokio::spawn(async move {
                            let client = reqwest::Client::new();
                            send_control_request_with_pool(
                                client.get(&url),
                                &url,
                                Duration::from_secs(5),
                                quick,
                            )
                            .await
                        })
                    })
                    .collect();

                let big_result = big_fut.await;
                big_result.expect("big download must complete");
                for handle in small_handles {
                    handle
                        .await
                        .expect("small task must not panic")
                        .expect("small control request must complete");
                }

                assert!(
                    big_hw.load(Ordering::SeqCst) <= 2,
                    "bulk-side concurrency must stay <= bulk pool size"
                );
                assert!(
                    small_hw.load(Ordering::SeqCst) <= 2,
                    "quick-side concurrency must stay <= quick pool size"
                );
            });

            clear_segmented_env();
        }
    );
}
