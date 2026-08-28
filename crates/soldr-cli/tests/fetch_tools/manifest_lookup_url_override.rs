//! `SOLDR_TOOLCHAIN_CATALOGUE_URL` override coverage.
//!
//! These process-environment cases share a lock and RAII restore scope with
//! the disable test, making them safe under plain Cargo's parallel execution
//! as well as nextest.

use std::sync::Arc;

use crate::common::manifest_env::{lock, EnvScope};
use soldr_cli::fetch::manifest_lookup::get_or_fetch;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_one_shot_json_server(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("http://{addr}/asset-index.json");
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    url
}

fn catalogue_body(owner: &str) -> String {
    format!(
        r#"{{
            "entries": [{{
                "owner": "{owner}",
                "repo": "test-repo",
                "tag": "v0.0.1",
                "asset": "test-asset.zip",
                "url": "https://example.invalid/test.zip",
                "sha256": "{}"
            }}]
        }}"#,
        "0".repeat(64)
    )
}

#[test]
fn catalogue_url_override_keeps_resolved_configurations_distinct() {
    let _lock = lock();
    let _env = EnvScope::capture();
    std::env::remove_var("SOLDR_MANIFEST_DISABLE");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let url_a = spawn_one_shot_json_server(catalogue_body("first-owner")).await;
        std::env::set_var("SOLDR_TOOLCHAIN_CATALOGUE_URL", &url_a);
        let first = get_or_fetch().await;
        assert_eq!(first.entries[0].owner, "first-owner");

        let first_again = get_or_fetch().await;
        assert!(
            Arc::ptr_eq(&first, &first_again),
            "the same resolved URL must reuse its ready Arc"
        );

        // URL A -> disabled -> URL A must never bind either configuration to
        // the other. The final call proves the ready A entry survives without
        // retrying the consumed one-shot listener.
        std::env::set_var("SOLDR_MANIFEST_DISABLE", "1");
        let disabled = get_or_fetch().await;
        assert!(
            disabled.entries.is_empty(),
            "disable must produce an empty index"
        );
        assert!(!Arc::ptr_eq(&first, &disabled));
        std::env::remove_var("SOLDR_MANIFEST_DISABLE");
        std::env::set_var("SOLDR_TOOLCHAIN_CATALOGUE_URL", &url_a);
        let restored = get_or_fetch().await;
        assert!(
            Arc::ptr_eq(&first, &restored),
            "URL A must reuse its ready entry after a disabled lookup"
        );

        let url_b = spawn_one_shot_json_server(catalogue_body("second-owner")).await;
        std::env::set_var("SOLDR_TOOLCHAIN_CATALOGUE_URL", url_b);
        let second = get_or_fetch().await;
        assert_eq!(second.entries[0].owner, "second-owner");
        assert!(
            !Arc::ptr_eq(&first, &second),
            "different resolved URLs must not share a catalogue entry"
        );
    });
}

#[test]
fn same_config_concurrent_callers_share_one_inflight_fetch() {
    let _lock = lock();
    let _env = EnvScope::capture();
    std::env::remove_var("SOLDR_MANIFEST_DISABLE");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let url = spawn_one_shot_json_server(catalogue_body("concurrent-owner")).await;
        std::env::set_var("SOLDR_TOOLCHAIN_CATALOGUE_URL", url);

        // The listener serves exactly one request. Before #2951's single-flight
        // cache, concurrent misses could both fetch and leak independent
        // indexes; the second caller then saw an empty fallback after the
        // listener closed. Both callers must now receive the same ready Arc.
        let (first, second) = tokio::join!(get_or_fetch(), get_or_fetch());
        assert_eq!(first.entries[0].owner, "concurrent-owner");
        assert!(
            Arc::ptr_eq(&first, &second),
            "same resolved configuration must share one in-flight fetch"
        );
    });
}
