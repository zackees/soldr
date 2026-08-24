use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use soldr_cli::core::SoldrPaths;
use soldr_cli::fetch::{manifest_lookup, syslib_common, trust};
use soldr_cli::pyo3_detect::{resolve_policy, BuildShape, DetectedPyo3, PlanMode, PolicyInput};

fn test_bundle() -> Vec<u8> {
    let encoder = zstd::stream::Encoder::new(Vec::new(), 1).expect("zstd encoder");
    let mut archive = tar::Builder::new(encoder);
    let payload = b"python import library";
    let mut header = tar::Header::new_gnu();
    header.set_size(payload.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "package/lib/python3.lib", &payload[..])
        .expect("append fixture");
    archive
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish zstd")
}

/// Backstop so `server.join()` cannot hang forever if the code under test stops
/// fetching. It is not a service budget, and sizing it like one is what broke.
///
/// It was 10s, which the fixture spent racing *the test's own setup*: the clock
/// starts when the server is spawned, but the first request is not issued until
/// after a tokio runtime build, a tempdir, two `resolve_policy` calls and an
/// entire no-op `materialize_compatibility`. On a contended Windows target-run
/// runner that exceeds 10s, so the listener was already dropped by the time the
/// client connected -- and the client's 5s/10s/20s retry backoff then dialled a
/// closed port three more times before giving up.
///
/// Sized against nextest's budget instead: comfortably under the 120s
/// terminate-after, so a genuine regression still fails with this test's own
/// message rather than an anonymous kill.
const FIXTURE_SERVER_BACKSTOP: std::time::Duration = std::time::Duration::from_secs(60);

fn serve_fixture(bundle: Vec<u8>) -> (String, Arc<Mutex<Vec<String>>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    let origin = format!("http://{}", listener.local_addr().expect("address"));
    let catalogue = serde_json::json!({
        "entries": [{
            "owner": "zackees",
            "repo": "soldr-toolchain",
            "tag": "3.13.14",
            "asset": "bundle.tar.zst",
            "url": format!("{origin}/python/3.13.14/windows-x64/bundle.tar.zst"),
            "sha256": trust::sha256_of(&bundle),
        }]
    })
    .to_string()
    .into_bytes();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    let handle = thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let deadline = std::time::Instant::now() + FIXTURE_SERVER_BACKSTOP;
        while seen.lock().expect("request log").len() < 2 && std::time::Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("accept request: {error}"),
            };
            // On macOS, sockets accepted from a nonblocking listener inherit
            // O_NONBLOCK. The fixture serves each accepted request
            // synchronously, so restore blocking reads before consuming it.
            stream
                .set_nonblocking(false)
                .expect("blocking fixture stream");
            let mut request = [0_u8; 4096];
            let count = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..count]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .expect("request path")
                .to_string();
            seen.lock().expect("request log").push(path.clone());
            let body = if path == "/catalogue.v1.json" {
                &catalogue
            } else {
                &bundle
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write headers");
            stream.write_all(body).expect("write body");
        }
    });
    (origin, requests, handle)
}

#[test]
fn compatibility_sysroot_uses_catalogue_sha_and_target_row() {
    let bundle = test_bundle();
    let (origin, requests, server) = serve_fixture(bundle);
    let catalogue_url = format!("{origin}/catalogue.v1.json");
    std::env::set_var(
        manifest_lookup::TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
        &catalogue_url,
    );
    std::env::set_var(syslib_common::SYSLIB_ASSET_ORIGIN_ENV_VAR, &origin);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let root = tempfile::tempdir().expect("temp root");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());
    let mut no_pyo3_plan = resolve_policy(PolicyInput {
        host: "x86_64-unknown-linux-gnu".into(),
        target: "x86_64-pc-windows-msvc".into(),
        detected: None,
        caller_pyo3: BTreeMap::new(),
        compatibility_sysroot: false,
        raw_dylib_disabled: false,
    });
    runtime
        .block_on(no_pyo3_plan.materialize_compatibility(&paths))
        .expect("no-PyO3 materialization is a no-op");
    assert!(requests.lock().expect("request log").is_empty());

    let mut compatibility_plan = resolve_policy(PolicyInput {
        host: "x86_64-unknown-linux-gnu".into(),
        target: "x86_64-pc-windows-msvc".into(),
        detected: Some(DetectedPyo3 {
            shape: BuildShape::Embedding,
            versions: BTreeSet::from(["0.22.6".into()]),
            features: BTreeSet::new(),
        }),
        caller_pyo3: BTreeMap::new(),
        compatibility_sysroot: true,
        raw_dylib_disabled: false,
    });
    assert_eq!(compatibility_plan.mode, PlanMode::CompatibilitySysroot);
    let result = runtime.block_on(compatibility_plan.materialize_compatibility(&paths));

    std::env::remove_var(manifest_lookup::TOOLCHAIN_CATALOGUE_URL_ENV_VAR);
    std::env::remove_var(syslib_common::SYSLIB_ASSET_ORIGIN_ENV_VAR);
    server.join().expect("fixture server");
    result.expect("materialized sysroot");
    assert_eq!(
        compatibility_plan
            .env
            .get("PYO3_CROSS_PYTHON_VERSION")
            .map(String::as_str),
        Some("3.13")
    );
    let lib_dir = compatibility_plan
        .env
        .get("PYO3_CROSS_LIB_DIR")
        .expect("compatibility lib dir");
    assert!(std::path::Path::new(lib_dir).join("python3.lib").is_file());
    assert_eq!(
        *requests.lock().expect("request log"),
        [
            "/catalogue.v1.json",
            "/python/3.13.14/windows-x64/bundle.tar.zst",
        ]
    );
}
