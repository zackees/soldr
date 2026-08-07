//! N-way HTTP Range segmented download prototype
//! (setup-soldr `feat/segmented-download-experiment`).
//!
//! ## Why
//!
//! soldr's syslib bundle (`syslib_common.rs`) and xwin-cache
//! (`xwin_cache.rs`) downloads are single-stream HTTPS. The maintainer's
//! named pain point is the xwin MSVCRT/SDK fetch. `crates/soldr-fetch/
//! examples/dl_bench.rs` measured single-stream vs N-way Range
//! segmentation against the real CDNs soldr talks to
//! (media.githubusercontent.com LFS-style assets, GitHub release
//! signed-URL redirects) and found a large, consistent win — see the
//! experiment's PR description for the full data table. This module is
//! the prototype integration that data justified.
//!
//! ## Design
//!
//! * **Correctness first.** Any of: Range unsupported, missing/zero
//!   `Content-Range` total, a segment HTTP failure, a segment writing
//!   the wrong byte count, or any I/O error — aborts the segmented
//!   attempt and falls back to the existing single-stream path
//!   ([`super::stream_download::stream_response_to_temp_file`]).
//!   Segmentation is purely a speed optimization; it must never be a new
//!   failure mode. sha256 verification happens downstream in the caller
//!   exactly as it does today, so a corrupt segmented assembly is still
//!   caught — this module does not weaken that guarantee, it just also
//!   verifies internally *before* declaring success, to avoid silently
//!   handing a corrupt local file to the (slower) caller-side check.
//! * **Per-request redirects, not a cached resolved URL.** Every probe
//!   and segment request goes through the original `url` and lets
//!   `reqwest`'s default redirect policy re-resolve it. GitHub release
//!   assets redirect to short-lived signed `objects.githubusercontent.com`
//!   URLs; caching one resolved URL and reusing it across N segment
//!   requests risks a stale/expired token. Re-resolving per request has
//!   no such risk and cost nothing extra in measurement (redirects are a
//!   single extra round trip, dwarfed by the transfer itself for assets
//!   this size).
//! * **Optional bearer auth**, forward-prep for a private MSVC bundle
//!   origin: when [`AUTH_TOKEN_ENV_VAR`] is set, every probe and segment
//!   request carries `Authorization: Bearer <token>`.
//! * **Opt-in.** This is a first prototype, not yet battle-tested across
//!   every CDN/proxy this crate's fetches touch. Set
//!   [`SEGMENTED_DOWNLOAD_ENV_VAR`] to a truthy value to enable it;
//!   unset (the default) keeps today's single-stream behavior
//!   unconditionally, including skipping the probe round trip.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::stream_download::{
    asset_http_client, get_request, send_asset_request, stream_response_to_temp_file,
    DownloadedAsset, ASSET_HEADER_TIMEOUT,
};
use crate::core::SoldrError;

/// Set to a truthy value (`1`, `true`, `yes`, `on` — case-insensitive) to
/// enable segmented downloads. Unset/falsy (the default) preserves
/// today's single-stream behavior exactly, with zero extra round trips.
pub(crate) const SEGMENTED_DOWNLOAD_ENV_VAR: &str = "SOLDR_SEGMENTED_DOWNLOAD";

/// Optional segment-count override. Clamped to `[2, MAX_SEGMENTS]`.
/// Unset uses [`DEFAULT_SEGMENT_COUNT`].
pub(crate) const SEGMENTED_DOWNLOAD_N_ENV_VAR: &str = "SOLDR_SEGMENTED_DOWNLOAD_N";

/// Bearer token attached to both the probe and every segment request.
/// Forward-prep for a private MSVC bundle origin (none of today's public
/// catalogue/xwin-cache/release-asset origins require it).
pub(crate) const AUTH_TOKEN_ENV_VAR: &str = "SOLDR_TOOLCHAIN_AUTH_TOKEN";

/// Default segment count. `dl_bench`'s full sweep (3 URLs x 3 repeats,
/// see the experiment's PR description for the table) found single-
/// stream pinned at a strikingly consistent ~2-2.3 MB/s across every
/// origin tested (media.githubusercontent LFS assets AND a GitHub
/// release signed-URL redirect) -- consistent with a per-connection
/// rate cap on GitHub's edge, not an asset-specific limit. Segmentation
/// kept scaling with no plateau through the largest N tested (16):
/// median MB/s roughly doubled from N4 to N8 and again (a smaller step)
/// from N8 to N16, matching aria2c's own -x16 within noise at every
/// data point. 8 balances most of that win (~5-6x over single-stream)
/// against holding fewer sockets open than the untested-beyond-16
/// ceiling; N remains an override for further tuning toward 16.
const DEFAULT_SEGMENT_COUNT: u32 = 8;
const MAX_SEGMENTS: u32 = 16;

/// Below this resource size, connection setup overhead for N sockets
/// isn't worth it — fall straight through to single-stream.
const MIN_SEGMENTABLE_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn segmented_download_enabled() -> bool {
    truthy_env(SEGMENTED_DOWNLOAD_ENV_VAR)
}

fn truthy_env(var: &str) -> bool {
    match std::env::var(var) {
        Ok(raw) => {
            let trimmed = raw.trim();
            !trimmed.is_empty()
                && !trimmed.eq_ignore_ascii_case("0")
                && !trimmed.eq_ignore_ascii_case("false")
                && !trimmed.eq_ignore_ascii_case("no")
                && !trimmed.eq_ignore_ascii_case("off")
        }
        Err(_) => false,
    }
}

fn segment_count_from_env() -> u32 {
    std::env::var(SEGMENTED_DOWNLOAD_N_ENV_VAR)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|n| n.clamp(2, MAX_SEGMENTS))
        .unwrap_or(DEFAULT_SEGMENT_COUNT)
}

fn auth_token() -> Option<String> {
    std::env::var(AUTH_TOKEN_ENV_VAR)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Download `url` with N-way Range segmentation when enabled and the
/// server cooperates; otherwise (disabled, unsupported, or any failure
/// during the segmented attempt) falls back to the caller's existing
/// single-stream path. Never returns an error the fallback itself
/// wouldn't also return — a segmented-specific failure is always
/// absorbed by the fallback, not surfaced.
///
/// The fallback is a caller-supplied closure rather than a fixed
/// implementation here because call sites differ in protocol needs —
/// notably `syslib_common.rs` pins `AssetProtocol::Http1Only` for a
/// documented CDN-compatibility reason, and that pin must survive
/// unchanged when segmentation is off (the default) or falls back.
pub(crate) async fn download_with_segmentation_or<F, Fut>(
    url: &str,
    fallback: F,
) -> Result<DownloadedAsset, SoldrError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<DownloadedAsset, SoldrError>>,
{
    if segmented_download_enabled() {
        match try_segmented(url).await {
            Ok(asset) => return Ok(asset),
            Err(reason) => {
                eprintln!(
                    "soldr: segmented download for {url} fell back to single-stream: {reason}"
                );
            }
        }
    }
    fallback().await
}

/// Default single-stream fallback (negotiated HTTP version) for callers
/// with no protocol pin of their own, e.g. `xwin_cache.rs`.
pub(crate) async fn download_single_stream(
    url: &str,
    idle_timeout: Duration,
) -> Result<DownloadedAsset, SoldrError> {
    let client = asset_http_client("asset download")?;
    let resp = send_asset_request(get_request(&client, url), url, ASSET_HEADER_TIMEOUT).await?;
    stream_response_to_temp_file(resp, url, idle_timeout).await
}

/// Reason a segmented attempt did not produce a verified asset. Always
/// non-fatal to the caller — [`download_with_segmentation`] falls back
/// to single-stream on every variant.
#[derive(Debug)]
enum SegmentedError {
    RangeUnsupported,
    Io(std::io::Error),
    Network(String),
}

impl std::fmt::Display for SegmentedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SegmentedError::RangeUnsupported => {
                write!(
                    f,
                    "server does not support Range requests (or total length unknown)"
                )
            }
            SegmentedError::Io(e) => write!(f, "local I/O error: {e}"),
            SegmentedError::Network(e) => write!(f, "network error: {e}"),
        }
    }
}

impl From<std::io::Error> for SegmentedError {
    fn from(e: std::io::Error) -> Self {
        SegmentedError::Io(e)
    }
}

impl From<reqwest::Error> for SegmentedError {
    fn from(e: reqwest::Error) -> Self {
        SegmentedError::Network(e.to_string())
    }
}

async fn try_segmented(url: &str) -> Result<DownloadedAsset, SegmentedError> {
    let client = asset_http_client("segmented asset download")
        .map_err(|e| SegmentedError::Network(e.to_string()))?;

    let total = probe_total_len(&client, url).await?;
    if total < MIN_SEGMENTABLE_BYTES {
        return Err(SegmentedError::RangeUnsupported);
    }

    let n = segment_count_from_env();
    let plan = compute_segments(total, n);

    let named = tempfile::NamedTempFile::new_in(soldr_core::core::ensure_temp_root())?;
    named.as_file().set_len(total)?;
    let file = Arc::new(named.reopen()?);

    let mut tasks = Vec::with_capacity(plan.len());
    for (start, end_inclusive) in plan {
        let client = client.clone();
        let url = url.to_string();
        let file = Arc::clone(&file);
        tasks.push(tokio::spawn(async move {
            download_segment(&client, &url, start, end_inclusive, &file).await
        }));
    }

    let mut bytes = 0u64;
    for task in tasks {
        bytes += task
            .await
            .map_err(|e| SegmentedError::Network(format!("segment task panicked: {e}")))??;
    }
    file.sync_all()?;

    let sha256 = sha256_of_file(&file)?;
    // Reopen as a NamedTempFile-backed DownloadedAsset: `named` still
    // owns the path/lifetime, `file`'s Arc handles were positional
    // writers only.
    drop(file);
    Ok(DownloadedAsset::from_parts(named, sha256, bytes))
}

/// 1-byte Range probe. A `206` with `Content-Range: bytes 0-0/<total>`
/// confirms Range support and gives the full length without a separate
/// HEAD (some CDNs handle HEAD on redirected assets inconsistently; a
/// tiny ranged GET is the more reliable probe across both
/// media.githubusercontent and GitHub release signed URLs, confirmed
/// empirically in `dl_bench`).
async fn probe_total_len(client: &reqwest::Client, url: &str) -> Result<u64, SegmentedError> {
    let mut req = client.get(url).header(reqwest::header::RANGE, "bytes=0-0");
    if let Some(token) = auth_token() {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let resp = req.send().await?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(SegmentedError::RangeUnsupported);
    }
    let content_range = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .ok_or(SegmentedError::RangeUnsupported)?;
    content_range
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&total| total > 0)
        .ok_or(SegmentedError::RangeUnsupported)
}

async fn download_segment(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end_inclusive: u64,
    file: &std::fs::File,
) -> Result<u64, SegmentedError> {
    let mut req = client.get(url).header(
        reqwest::header::RANGE,
        format!("bytes={start}-{end_inclusive}"),
    );
    if let Some(token) = auth_token() {
        req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let mut resp = req.send().await?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT && !resp.status().is_success() {
        return Err(SegmentedError::Network(format!(
            "segment [{start},{end_inclusive}] HTTP {}",
            resp.status()
        )));
    }

    let mut offset = start;
    let mut written = 0u64;
    while let Some(chunk) = resp.chunk().await? {
        write_at_all(file, &chunk, offset)?;
        offset += chunk.len() as u64;
        written += chunk.len() as u64;
    }
    let expected = end_inclusive - start + 1;
    if written != expected {
        return Err(SegmentedError::Network(format!(
            "segment [{start},{end_inclusive}] wrote {written} bytes, expected {expected}"
        )));
    }
    Ok(written)
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
/// distributing the `total % n` remainder one byte at a time to the
/// first segments. Every byte in `[0, total)` is covered by exactly one
/// segment; no segment is ever empty when `total >= n`.
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
        // 1000 / 3 = 333 remainder 1 -- one segment must absorb the extra byte.
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
                // No segment count exceeds n (small totals collapse fewer).
                assert!(segments.len() as u32 <= n, "total={total} n={n}");
            }
        }
    });

    crate::timed_test!(zero_total_or_zero_n_produces_no_segments, {
        assert!(compute_segments(0, 4).is_empty());
        assert!(compute_segments(1000, 0).is_empty());
    });

    crate::timed_test!(more_segments_than_bytes_collapses_without_empty_segments, {
        // 3 bytes split into 8 segments: no segment may be empty, so this
        // must produce at most 3 non-empty segments, each covering the
        // range exactly.
        let segments = compute_segments(3, 8);
        assert_exact_coverage(3, &segments);
        assert!(segments.len() <= 3);
        for &(start, end_inclusive) in &segments {
            assert_eq!(end_inclusive - start + 1, 1);
        }
    });

    crate::timed_test!(truthy_env_recognizes_common_spellings, {
        let key = "SOLDR_SEGMENTED_DOWNLOAD_TEST_TRUTHY";
        for value in ["1", "true", "TRUE", "yes", "on"] {
            std::env::set_var(key, value);
            assert!(truthy_env(key), "{value:?} should be truthy");
        }
        for value in ["0", "false", "FALSE", "no", "off", ""] {
            std::env::set_var(key, value);
            assert!(!truthy_env(key), "{value:?} should be falsy");
        }
        std::env::remove_var(key);
        assert!(!truthy_env(key), "unset must be falsy");
    });

    crate::timed_test!(segment_count_env_override_is_clamped, {
        let key = SEGMENTED_DOWNLOAD_N_ENV_VAR;
        std::env::set_var(key, "1");
        assert_eq!(segment_count_from_env(), 2, "below-minimum clamps to 2");
        std::env::set_var(key, "9999");
        assert_eq!(
            segment_count_from_env(),
            MAX_SEGMENTS,
            "above-maximum clamps to MAX_SEGMENTS"
        );
        std::env::set_var(key, "not-a-number");
        assert_eq!(
            segment_count_from_env(),
            DEFAULT_SEGMENT_COUNT,
            "unparseable falls back to the default"
        );
        std::env::remove_var(key);
        assert_eq!(
            segment_count_from_env(),
            DEFAULT_SEGMENT_COUNT,
            "unset falls back to the default"
        );
    });

    crate::timed_test!(disabled_by_default, {
        std::env::remove_var(SEGMENTED_DOWNLOAD_ENV_VAR);
        assert!(
            !segmented_download_enabled(),
            "an unset env var must never turn on the prototype path"
        );
    });

    // ---- fallback-trigger integration tests against a local server ----

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test runtime")
    }

    /// A server that answers every request (including Range requests)
    /// with a plain `200 OK` and the whole body -- i.e. it ignores
    /// `Range` entirely, the way some misconfigured proxies do.
    async fn serve_range_unaware(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = socket.read(&mut buf).await;
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

    crate::timed_test!(range_unaware_server_falls_back_to_single_stream, {
        runtime().block_on(async {
            let body = vec![b'x'; 4096];
            let url = serve_range_unaware(body.clone()).await;

            let fallback = || download_single_stream(&url, Duration::from_secs(5));
            let asset = download_with_segmentation_or(&url, fallback)
                .await
                .expect("must succeed via single-stream fallback");
            assert_eq!(asset.bytes(), body.len() as u64);

            // Segmentation must not even be attempted in a way that
            // errors the caller when disabled (default) -- and when
            // forced on, a Range-unaware server must still resolve via
            // fallback rather than propagating an error.
            std::env::set_var(SEGMENTED_DOWNLOAD_ENV_VAR, "1");
            let fallback2 = || download_single_stream(&url, Duration::from_secs(5));
            let asset2 = download_with_segmentation_or(&url, fallback2)
                .await
                .expect("range-unaware server must fall back, not error");
            std::env::remove_var(SEGMENTED_DOWNLOAD_ENV_VAR);
            assert_eq!(asset2.bytes(), body.len() as u64);
            assert_eq!(asset.sha256(), asset2.sha256());
        });
    });

    crate::timed_test!(probe_rejects_missing_content_range, {
        // A 206 response with no Content-Range header at all must be
        // treated as Range-unsupported, not panic or misparse.
        runtime().block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = listener.local_addr().expect("addr");
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut buf = [0u8; 2048];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx")
                    .await;
                let _ = socket.shutdown().await;
            });
            let url = format!("http://{address}/asset");
            let client = reqwest::Client::new();
            let result = probe_total_len(&client, &url).await;
            assert!(
                matches!(result, Err(SegmentedError::RangeUnsupported)),
                "missing Content-Range must be treated as unsupported: {result:?}"
            );
        });
    });
}
