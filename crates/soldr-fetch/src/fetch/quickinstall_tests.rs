//! Hermetic tests for the QuickInstall hop (soldr#3700): a loopback HTTP
//! fixture serves a crate that exists only as a QuickInstall asset.

use super::*;
use std::io::Write as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CRATE: &str = "fixture-tool";
const VERSION: &str = "1.2.3";

fn fixture_tarball(binary_name: &str) -> Vec<u8> {
    let payload = b"#!/bin/sh\necho fixture-tool 1.2.3\n";
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, binary_name, &payload[..])
            .expect("append");
        builder.finish().expect("finish");
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar_bytes).expect("gzip");
    gz.finish().expect("gzip finish")
}

/// Serve `body` at `asset_path` (HEAD + GET); every other path is 404.
async fn serve(asset_path: String, body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let asset_path = asset_path.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut parts = request.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let response = if path == asset_path {
                    let mut head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    if method == "GET" {
                        head.extend_from_slice(&body);
                    }
                    head
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_vec()
                };
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{address}/download")
}

fn asset_name(target: &TargetTriple) -> String {
    format!("{CRATE}-{VERSION}-{}.tar.gz", target.triple())
}

struct Fixture {
    _root: tempfile::TempDir,
    paths: SoldrPaths,
    target: TargetTriple,
    base: String,
    sha256: String,
}

async fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());
    let target = TargetTriple::host().expect("host triple");
    let binary = archive::desired_binary_names(&[CRATE], &target)
        .into_iter()
        .next()
        .expect("binary name");
    let body = fixture_tarball(&binary);
    let sha256 = trust::sha256_of(&body);
    let asset_path = format!("/download/{CRATE}-{VERSION}/{}", asset_name(&target));
    let base = serve(asset_path, body).await;
    Fixture {
        _root: root,
        paths,
        target,
        base,
        sha256,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn pin_store(target: &TargetTriple, sha256: &str) -> trust::PinnedChecksumStore {
    trust::PinnedChecksumStore::from_toml(&format!(
        "[[tool]]\ntool = \"{CRATE}\"\nversion = \"{VERSION}\"\nasset = \"{}\"\nsha256 = \"{sha256}\"\n",
        asset_name(target)
    ))
    .expect("pin store")
}

#[test]
fn crate_published_only_on_quickinstall_resolves() {
    runtime().block_on(async {
        let fx = fixture().await;
        let store = trust::PinnedChecksumStore::empty();
        let result = try_resolve(
            &fx.paths,
            CRATE,
            &[CRATE],
            VERSION,
            &fx.target,
            &fx.base,
            (&store, trust::TrustMode::Permissive),
        )
        .await
        .expect("quickinstall resolve")
        .expect("quickinstall-only crate must resolve");
        assert!(result.binary_path.is_file(), "{:?}", result.binary_path);
        assert_eq!(result.version, VERSION);
        assert!(!result.cached);
    });
}

#[test]
fn quickinstall_miss_returns_none() {
    runtime().block_on(async {
        let fx = fixture().await;
        let store = trust::PinnedChecksumStore::empty();
        let result = try_resolve(
            &fx.paths,
            CRATE,
            &[CRATE],
            "9.9.9",
            &fx.target,
            &fx.base,
            (&store, trust::TrustMode::Permissive),
        )
        .await
        .expect("miss is not an error");
        assert!(result.is_none());
    });
}

#[test]
fn strict_mode_refuses_unpinned_quickinstall_asset() {
    runtime().block_on(async {
        let fx = fixture().await;
        let store = trust::PinnedChecksumStore::empty();
        let err = try_resolve(
            &fx.paths,
            CRATE,
            &[CRATE],
            VERSION,
            &fx.target,
            &fx.base,
            (&store, trust::TrustMode::Strict),
        )
        .await
        .expect_err("strict mode must refuse an unpinned QuickInstall asset");
        assert!(err.to_string().contains("strict"), "{err}");
        assert!(!fx.paths.bin.join(format!("{CRATE}-{VERSION}")).exists());
    });
}

#[test]
fn pin_mismatch_is_fatal_even_in_permissive_mode() {
    runtime().block_on(async {
        let fx = fixture().await;
        let store = pin_store(&fx.target, &"0".repeat(64));
        let err = try_resolve(
            &fx.paths,
            CRATE,
            &[CRATE],
            VERSION,
            &fx.target,
            &fx.base,
            (&store, trust::TrustMode::Permissive),
        )
        .await
        .expect_err("a pin mismatch must never be bypassed");
        assert!(err.to_string().contains("pinned sha256 mismatch"), "{err}");
    });
}

#[test]
fn matching_pin_satisfies_strict_mode() {
    runtime().block_on(async {
        let fx = fixture().await;
        let store = pin_store(&fx.target, &fx.sha256);
        let result = try_resolve(
            &fx.paths,
            CRATE,
            &[CRATE],
            VERSION,
            &fx.target,
            &fx.base,
            (&store, trust::TrustMode::Strict),
        )
        .await
        .expect("pinned asset passes strict mode");
        assert!(result.is_some());
    });
}

#[test]
fn resolver_order_gates_quickinstall() {
    assert!(ResolverOrder::all().try_quickinstall);
    assert!(ResolverOrder::parse("quickinstall").try_quickinstall);
    assert!(!ResolverOrder::parse("quickinstall").try_api);
    assert!(!ResolverOrder::parse("embed,live,api").try_quickinstall);
}

#[test]
fn excluded_hop_returns_the_api_error_without_network() {
    runtime().block_on(async {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(root.path().to_path_buf());
        let target = TargetTriple::host().expect("host triple");
        let err = fallback_after_api(
            ResolverOrder::parse("api"),
            SoldrError::ToolNotFound("api miss".into()),
            &paths,
            CRATE,
            &[CRATE],
            &VersionSpec::Exact(VERSION.into()),
            None,
            &target,
        )
        .await
        .expect_err("excluded hop must not resolve");
        assert!(err.to_string().contains("api miss"), "{err}");
    });
}

#[test]
fn url_and_version_normalization() {
    assert_eq!(normalize_version("v1.2.3", None), "1.2.3");
    assert_eq!(
        normalize_version("cargo-audit/v0.21.0", Some("cargo-audit/")),
        "0.21.0"
    );
    let target = TargetTriple::host().expect("host triple");
    assert_eq!(
        asset_url("https://x/dl/", "c", "1.0.0", &target),
        format!("https://x/dl/c-1.0.0/c-1.0.0-{}.tar.gz", target.triple())
    );
}
