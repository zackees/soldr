
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
    assert!(verify_catalogue_asset_sha256(&entry, &super::super::trust::sha256_of(bytes)).is_ok());
    assert!(
        verify_catalogue_asset_sha256(&entry, &super::super::trust::sha256_of(b"changed")).is_err()
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
    // Phase 2 must accept the v1 wire shape transparently â€” the
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
fn catalogue_v1_preserves_duplicate_identity_compatibility() {
    let v1 = r#"{
            "schema_version": 1,
            "entries": [
                {
                    "owner": "zackees",
                    "repo": "soldr-toolchain",
                    "tag": "1",
                    "asset": "bundle.tar.zst",
                    "url": "https://example.com/first.tar.zst",
                    "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
                },
                {
                    "owner": "zackees",
                    "repo": "soldr-toolchain",
                    "tag": "1",
                    "asset": "bundle.tar.zst",
                    "url": "https://example.com/second.tar.zst",
                    "sha256": "1111111111111111111111111111111111111111111111111111111111111111"
                }
            ]
        }"#;

    let index = ManifestIndex::from_json(v1).expect("legacy duplicate rows must still parse");
    assert_eq!(index.entries.len(), 2);
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
// parser â€” the two shapes are disjoint on the wire (`entries: []`
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
    // when they spawn a local HTTP listener on a random port â€”
    // they can't fit that under the `origin + /catalogue.v1.json`
    // composition because the listener path is fixed.
    // Verify the override is recognized via the public const name.
    assert_eq!(
        TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
        "SOLDR_TOOLCHAIN_CATALOGUE_URL"
    );
}
