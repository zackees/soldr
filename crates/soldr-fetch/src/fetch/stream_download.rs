//! Bounded-memory response-body downloads for release and toolchain assets.
//!
//! Metadata requests intentionally retain their short response-wide deadlines.
//! Archive payloads instead use this module: a successful chunk resets the
//! idle timer, so a healthy multi-gigabyte transfer is not killed by elapsed
//! wall time, while a stalled or truncated response remains retryable.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use crate::core::SoldrError;
use sha2::{Digest, Sha256};

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
pub(crate) async fn stream_response_to_temp_file_with_safety_timeout(
    mut response: reqwest::Response,
    url: &str,
    idle_timeout: Duration,
    safety_timeout: Duration,
) -> Result<DownloadedAsset, SoldrError> {
    if !response.status().is_success() {
        return Err(SoldrError::Network(format!(
            "asset download {url} failed: HTTP {}",
            response.status()
        )));
    }

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
    tokio::time::timeout(header_timeout, request.send())
        .await
        .map_err(|_| {
            SoldrError::Network(format!(
                "control request timed out waiting for headers: {url}"
            ))
        })?
        .map_err(|error| SoldrError::Network(error.to_string()))
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
}
