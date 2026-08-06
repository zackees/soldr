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
    mut response: reqwest::Response,
    url: &str,
    idle_timeout: Duration,
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

    loop {
        let chunk = tokio::time::timeout(idle_timeout, response.chunk())
            .await
            .map_err(|_| stalled_download_error(url, bytes, idle_timeout))?
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
