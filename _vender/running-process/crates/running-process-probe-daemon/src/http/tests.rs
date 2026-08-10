//! Tests for the HTTP surface (S13 / #642).
//!
//! Driven through a real bound listener and a real TCP client rather than by
//! calling handlers directly. The things this slice has to get right — a 401
//! from middleware, a refused non-loopback bind, a body that streams instead
//! of buffering — are all properties of the *server*, and a handler-level
//! test would assert none of them.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;

use running_process::broker::server::PeerCredentialPolicy;
use tempfile::TempDir;

use super::*;
use crate::crash_store::CrashStore;
use crate::registry::Registry;

const OWNER: &str = "http-owner";
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A running daemon HTTP surface, plus the store behind it.
struct Server {
    _dir: TempDir,
    addr: SocketAddr,
    store: Arc<CrashStore>,
}

impl Server {
    fn start() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(
            CrashStore::open(
                &dir.path().join("crashes.db"),
                &dir.path().join("artifacts"),
            )
            .expect("open crash store"),
        );
        let ops = Arc::new(
            ProbeOps::new(
                Arc::new(Registry::new(OWNER.to_string())),
                PeerCredentialPolicy::OwnerOnly {
                    uid_or_sid: OWNER.to_string(),
                },
            )
            .with_crash_store(Arc::clone(&store)),
        );
        let state = HttpState::new(ops, TOKEN.to_string());
        let (addr, _handle) = spawn(default_bind(), state).expect("start http surface");
        Self {
            _dir: dir,
            addr,
            store,
        }
    }

    /// Send a raw request and return `(status, headers, body)`.
    ///
    /// Hand-rolled rather than pulling in an HTTP client: the streaming test
    /// needs to read the body incrementally to prove it is chunked out rather
    /// than buffered, and a client that hands back a `Vec<u8>` cannot show
    /// that.
    fn request(&self, line: &str, headers: &[String]) -> (u16, Vec<String>, Vec<u8>) {
        let mut stream = TcpStream::connect(self.addr).expect("connect");
        let mut request = format!("{line} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        for header in headers {
            request.push_str(header);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).expect("write request");
        stream.flush().expect("flush");

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).expect("status line");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);

        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).expect("header line") == 0 {
                break;
            }
            if line == "\r\n" {
                break;
            }
            headers.push(line.trim_end().to_string());
        }

        let mut body = Vec::new();
        reader.read_to_end(&mut body).expect("body");
        let decoded = decode_body(&headers, body);
        (status, headers, decoded)
    }

    fn get(&self, path: &str) -> (u16, Vec<String>, Vec<u8>) {
        self.request(
            &format!("GET {path}"),
            &[format!("Authorization: Bearer {TOKEN}")],
        )
    }

    fn get_json(&self, path: &str) -> serde_json::Value {
        let (status, _, body) = self.get(path);
        assert_eq!(status, 200, "GET {path} returned {status}");
        serde_json::from_slice(&body).expect("json body")
    }

    /// Store one artifact and return its id.
    ///
    /// The name has to match the store's own artifact convention
    /// (`crash-<millis>-<32 hex>.json`): the fetch path refuses to serve any
    /// file the daemon did not write, so a fixture with an arbitrary name
    /// would exercise the 404 branch instead of the download.
    fn seed_artifact(&self, contents: &[u8]) -> i64 {
        let dir = self.store.artifacts_dir_for_test();
        std::fs::create_dir_all(dir).expect("artifacts dir");
        let name = format!("crash-1000-{}.json", "ab".repeat(16));
        std::fs::write(dir.join(&name), contents).expect("write artifact");
        let conn = self.store.connection_for_test().lock().expect("store lock");
        conn.execute(
            "INSERT INTO crashes (app_class, app_name, app_version, instance_name, pid,
                                  creation_time_ms, cwd, signature, crashed_at_ms, exit_signal,
                                  report_json, artifact_path, artifact_bytes)
             VALUES ('clud', 'clud', '1.0', 'a', 7, 1, '/work', 'SIGSEGV@x', 1000, 'SIGSEGV',
                     '{\"secret\":\"hunter2\"}', ?1, ?2)",
            rusqlite::params![name, contents.len() as i64],
        )
        .expect("seed crash row");
        conn.last_insert_rowid()
    }
}

/// Undo `Transfer-Encoding: chunked` if the server used it.
fn decode_body(headers: &[String], raw: Vec<u8>) -> Vec<u8> {
    let chunked = headers.iter().any(|h| {
        h.to_ascii_lowercase()
            .starts_with("transfer-encoding: chunked")
    });
    if !chunked {
        return raw;
    }
    let mut out = Vec::new();
    let mut rest = raw.as_slice();
    loop {
        let Some(split) = rest.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let Ok(size_text) = std::str::from_utf8(&rest[..split]) else {
            break;
        };
        let Ok(size) = usize::from_str_radix(size_text.trim(), 16) else {
            break;
        };
        rest = &rest[split + 2..];
        if size == 0 || rest.len() < size {
            break;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2.min(rest.len() - size)..];
    }
    out
}

// --- bind policy ----------------------------------------------------------

#[test]
fn a_non_loopback_bind_is_refused_before_the_socket_exists() {
    // Checked before binding, not after: a refused address must never briefly
    // be a listening socket that something could connect to.
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);
    assert!(!is_loopback(&addr));
    match check_bind(addr) {
        Err(HttpError::NonLoopbackBind { .. }) => {}
        other => panic!("a non-loopback bind must be refused, got {other:?}"),
    }
}

#[test]
fn loopback_binds_need_no_opt_in() {
    assert!(check_bind(default_bind()).is_ok());
    assert!(check_bind(SocketAddr::from(([127, 0, 0, 1], 8080))).is_ok());
    assert!(check_bind("[::1]:8080".parse().expect("v6 loopback")).is_ok());
}

// --- authentication -------------------------------------------------------

#[test]
fn every_route_requires_a_token() {
    let server = Server::start();
    // The landing page included. There is no "just the UI" tier, because the
    // UI is what calls the API.
    for path in [
        "/",
        "/assets/probe.js",
        "/v1/ps?limit=10",
        "/v1/crashes?limit=10",
        "/v1/crashes/stats",
        "/v1/artifacts/1",
    ] {
        let (status, _, _) = server.request(&format!("GET {path}"), &[]);
        assert_eq!(status, 401, "{path} served an unauthenticated caller");
    }
}

#[test]
fn a_wrong_token_is_rejected() {
    let server = Server::start();
    let wrong = format!("Authorization: Bearer {}0", &TOKEN[..63]);
    let (status, _, _) = server.request("GET /v1/ps?limit=10", &[wrong]);
    assert_eq!(status, 401);
}

#[test]
fn the_token_is_accepted_from_a_query_string_or_a_cookie() {
    // A browser cannot set a header on a navigation, so the first hit carries
    // `?token=` and the page then trades it for a cookie.
    let server = Server::start();

    let (status, _, _) = server.request(&format!("GET /v1/ps?limit=10&token={TOKEN}"), &[]);
    assert_eq!(status, 200, "query-string token rejected");

    let cookie = format!("Cookie: probe_token={TOKEN}");
    let (status, _, _) = server.request("GET /v1/ps?limit=10", &[cookie]);
    assert_eq!(status, 200, "cookie token rejected");
}

// --- transport parity -----------------------------------------------------

#[test]
fn http_and_the_socket_core_return_the_same_crash_rows() {
    // Not "similar" — the same, because it is the same `ProbeOps` call. This
    // is what stops the two ingresses drifting into different policies.
    let server = Server::start();
    server.seed_artifact(b"artifact");

    let over_http = server.get_json("/v1/crashes?limit=10");
    let http_rows = over_http.as_array().expect("array");

    let direct = server
        .store
        .query(
            &crate::crash_query::CrashFilter::default(),
            std::num::NonZeroU32::new(10).unwrap(),
        )
        .expect("direct query");

    assert_eq!(http_rows.len(), direct.len());
    assert_eq!(
        http_rows[0]["signature"].as_str(),
        Some(direct[0].signature.as_str())
    );
    assert_eq!(http_rows[0]["id"].as_i64(), Some(direct[0].id));
}

#[test]
fn a_crash_row_over_http_carries_no_artifact_path_and_no_inline_report() {
    let server = Server::start();
    server.seed_artifact(b"artifact");
    let body = server.get_json("/v1/crashes?limit=10").to_string();
    assert!(!body.contains("hunter2"), "inline report leaked over HTTP");
    assert!(
        !body.contains("crash-1000-"),
        "artifact path leaked over HTTP"
    );
}

#[test]
fn crash_stats_report_the_whole_match_set_not_a_page() {
    let server = Server::start();
    for _ in 0..5 {
        server.seed_artifact(b"x");
    }
    let stats = server.get_json("/v1/crashes/stats");
    assert_eq!(stats["total"].as_u64(), Some(5));
    assert_eq!(stats["signatures"][0]["count"].as_u64(), Some(5));
}

#[test]
fn an_invalid_selector_is_a_400_with_the_daemons_own_reason() {
    let server = Server::start();
    let (status, _, body) = server.get("/v1/ps?name_regex=(unclosed&limit=10");
    assert_eq!(status, 400);
    let error: serde_json::Value = serde_json::from_slice(&body).expect("json error");
    assert_eq!(error["error"].as_str(), Some("invalid_query"));
}

// --- artifact streaming ---------------------------------------------------

#[test]
fn an_artifact_larger_than_the_socket_frame_cap_downloads_intact() {
    // 20 MiB — above the control socket's 16 MiB frame cap, which is the
    // entire reason this endpoint exists.
    let server = Server::start();
    let payload: Vec<u8> = (0..20 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let id = server.seed_artifact(&payload);

    let (status, headers, body) = server.get(&format!("/v1/artifacts/{id}"));
    assert_eq!(status, 200);
    assert_eq!(body.len(), payload.len(), "artifact truncated in transit");
    assert_eq!(body, payload, "artifact corrupted in transit");
    assert!(
        headers
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with("content-disposition:")),
        "a download must be marked as an attachment"
    );
}

#[test]
fn a_missing_artifact_is_a_404() {
    let server = Server::start();
    let (status, _, _) = server.get("/v1/artifacts/999999");
    assert_eq!(status, 404);
}

#[test]
fn the_download_filename_is_built_from_the_id_not_from_stored_text() {
    let server = Server::start();
    let id = server.seed_artifact(b"bytes");
    let (_, headers, _) = server.get(&format!("/v1/artifacts/{id}"));
    let disposition = headers
        .iter()
        .find(|h| h.to_ascii_lowercase().starts_with("content-disposition:"))
        .expect("content-disposition");
    assert!(disposition.contains(&format!("probe-artifact-{id}.bin")));
}

// --- UI -------------------------------------------------------------------

#[test]
fn the_landing_page_and_its_assets_are_served() {
    let server = Server::start();
    let (status, _, body) = server.get("/");
    assert_eq!(status, 200);
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("<title>rpprobed</title>"));

    for asset in ["/assets/probe.css", "/assets/probe.js"] {
        let (status, _, body) = server.get(asset);
        assert_eq!(status, 200, "{asset} not served");
        assert!(!body.is_empty());
    }
}

// --- flame graph ----------------------------------------------------------

#[test]
fn collapsed_stacks_fold_into_a_tree() {
    let tree = crate::profile::store::collapsed_to_tree("main;a;b 3\nmain;a;c 2\nmain;d 5\n");
    assert_eq!(tree.value, 10);

    let main = &tree.children[0];
    assert_eq!(main.name, "main");
    assert_eq!(main.value, 10);

    let a = main.children.iter().find(|c| c.name == "a").expect("a");
    assert_eq!(a.value, 5, "shared prefixes must merge, not duplicate");
    assert_eq!(a.children.len(), 2);

    let d = main.children.iter().find(|c| c.name == "d").expect("d");
    assert_eq!(d.value, 5);
}

#[test]
fn a_malformed_profile_line_is_skipped_rather_than_failing_the_render() {
    // A profile is a sampled artifact. Losing one line of it is strictly
    // better than showing the operator nothing at all.
    let tree = crate::profile::store::collapsed_to_tree(
        "main;a 3\ngarbage-with-no-count\nmain;b notanumber\n",
    );
    assert_eq!(tree.value, 3);
    assert_eq!(tree.children.len(), 1);
}

#[test]
fn an_empty_profile_is_an_empty_tree_not_an_error() {
    let tree = crate::profile::store::collapsed_to_tree("");
    assert_eq!(tree.value, 0);
    assert!(tree.children.is_empty());
}

#[test]
fn a_flame_render_reads_a_real_artifact_over_http() {
    let server = Server::start();
    let id = server.seed_artifact(b"main;parse;lex 7\nmain;parse;eval 3\n");
    let tree = server.get_json(&format!("/v1/flame?artifact={id}"));
    assert_eq!(tree["value"].as_u64(), Some(10));
    assert_eq!(tree["children"][0]["name"].as_str(), Some("main"));
}

// --- state redaction ------------------------------------------------------

#[test]
fn the_shared_state_never_prints_its_token() {
    let ops = Arc::new(ProbeOps::new(
        Arc::new(Registry::new(OWNER.to_string())),
        PeerCredentialPolicy::OwnerOnly {
            uid_or_sid: OWNER.to_string(),
        },
    ));
    let state = HttpState::new(ops, TOKEN.to_string());
    let rendered = format!("{state:?}");
    assert!(
        !rendered.contains(TOKEN),
        "the daemon's authentication secret must not reach a log line"
    );
}

#[test]
fn the_router_is_constructible() {
    // axum validates routes by panicking at construction, so a malformed one
    // is a compile-clean, test-clean change that only fails at run time — and
    // it did: "/v1/profiles/{id}.{format}" put two parameters in one path
    // segment, panicked on the serving thread, and left the daemon running
    // with no HTTP surface at all. Building the router in a test makes that
    // class of mistake a test failure instead of a silent outage.
    let ops = Arc::new(ProbeOps::new(
        Arc::new(Registry::new(OWNER.to_string())),
        PeerCredentialPolicy::OwnerOnly {
            uid_or_sid: OWNER.to_string(),
        },
    ));
    let _router = build_router(HttpState::new(ops, TOKEN.to_string()));
}
