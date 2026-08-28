//! Standalone test binary for the `SOLDR_TOOLCHAIN_CATALOGUE_URL`
//! override (soldr#988 Phase 5 — replaces the retired
//! `SOLDR_MANIFEST_URL`). Lives in its own file so it gets its own
//! cargo-generated test binary — `fetch::manifest_lookup` caches its
//! result in a process-wide `OnceLock`, so cross-test env-var
//! ordering inside a single binary would race.
//!
//! Covers:
//!
//!   * `catalogue_url_override_env_var_works` —
//!     `SOLDR_TOOLCHAIN_CATALOGUE_URL` points the fetcher at an
//!     alternate URL.

use std::sync::Arc;

use soldr_cli::fetch::manifest_lookup::get_or_fetch;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_one_shot_json_server(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{}/asset-index.json", addr);
    let body = Arc::new(body);
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            // Drain the request line + headers — we only need to
            // honor the request shape, not parse it.
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let body_bytes = body.as_bytes();
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body_bytes.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.write_all(body_bytes).await;
            let _ = sock.shutdown().await;
        }
    });
    url
}

#[test]
fn catalogue_url_override_env_var_works() {
    // SAFETY: only test in this binary, so env-var writes are
    // single-threaded.
    std::env::remove_var("SOLDR_MANIFEST_DISABLE");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async {
        let body = r#"{
            "entries": [
                {
                    "owner": "test-owner",
                    "repo": "test-repo",
                    "tag": "v0.0.1",
                    "asset": "test-asset.zip",
                    "url": "https://example.invalid/test.zip",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            ]
        }"#
        .to_string();
        let url = spawn_one_shot_json_server(body).await;
        std::env::set_var("SOLDR_TOOLCHAIN_CATALOGUE_URL", &url);

        // Drive the production fetcher end-to-end. soldr#2951: the cache is
        // keyed on the resolved catalogue URL, so this one-shot server gets
        // its own entry no matter what any sibling test in this binary
        // fetched first. The previous comment here claimed the cache was
        // "fresh because this is the binary's first call" -- true only while
        // every `tests/*.rs` file was its own binary, which soldr#2934 ended.
        let idx = get_or_fetch().await;
        assert_eq!(
            idx.entries.len(),
            1,
            "catalogue at SOLDR_TOOLCHAIN_CATALOGUE_URL should be parsed and cached"
        );
        let entry = &idx.entries[0];
        assert_eq!(entry.owner, "test-owner");
        assert_eq!(entry.repo, "test-repo");
        assert_eq!(entry.tag, "v0.0.1");
        assert_eq!(
            entry.transport.direct_url(),
            Some("https://example.invalid/test.zip")
        );

        // Second call hits the cache — no network round trip means
        // no second connection on the listener (which has already
        // shut down anyway). The pointer should equal the first
        // call's result.
        let idx2 = get_or_fetch().await;
        assert!(
            std::ptr::eq(idx, idx2),
            "the same catalogue URL must return the same cached reference"
        );

        // soldr#2951 RED: a *different* URL must not be served the first
        // one's index. Before the cache was keyed, this returned the entry
        // above and the override was silently discarded -- which is the
        // production bug, not merely a test artefact.
        let other_body = r#"{
            "schema_version": 1,
            "entries": [
                {
                    "owner": "second-owner",
                    "repo": "second-repo",
                    "tag": "v0.0.2",
                    "asset": "second-asset.zip",
                    "url": "https://example.invalid/second.zip",
                    "sha256": "1111111111111111111111111111111111111111111111111111111111111111"
                }
            ]
        }"#
        .to_string();
        let other_url = spawn_one_shot_json_server(other_body).await;
        std::env::set_var("SOLDR_TOOLCHAIN_CATALOGUE_URL", &other_url);
        let idx3 = get_or_fetch().await;
        assert_eq!(
            idx3.entries.len(),
            1,
            "a new catalogue URL must be fetched, not served from the old key"
        );
        assert_eq!(
            idx3.entries[0].owner, "second-owner",
            "the second URL's catalogue must win; serving the first one back              is the soldr#2951 bug"
        );

        std::env::remove_var("SOLDR_TOOLCHAIN_CATALOGUE_URL");
    });
}
