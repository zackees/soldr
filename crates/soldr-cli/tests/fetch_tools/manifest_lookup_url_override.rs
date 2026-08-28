//! End-to-end coverage of the catalogue *configuration* contract: which
//! document a `get_or_fetch` call resolves to, and how many times the origin
//! is asked for it.
//!
//! soldr#2951 reopened: the first fix keyed the process cache on a string
//! derived from the environment, but re-read the environment after dropping
//! the lock to decide what to fetch. These tests pin the properties that
//! window violated.
//!
//!   * `catalogue_url_override_env_var_works` —
//!     `SOLDR_TOOLCHAIN_CATALOGUE_URL` points the fetcher at an alternate URL,
//!     and a second call is served from the cache rather than the network.
//!   * `url_then_disabled_then_url_is_deterministic` — "disabled" is its own
//!     identity, never satisfied by a URL's cached entry, and the URL's entry
//!     survives the round trip.
//!   * `a_second_url_gets_its_own_index` — a different URL is fetched, not
//!     served from the first URL's entry.
//!   * `concurrent_callers_share_one_fetch` — N parallel callers on one
//!     configuration produce exactly ONE origin request.
//!   * `distinct_configurations_stay_bounded` — more configurations than the
//!     cache holds do not grow it.
//!
//! Every test takes `common::catalogue_env::CatalogueEnvGuard`: they mutate
//! process environment, and since soldr#2934 they share a process with every
//! other module in the `fetch_tools` binary.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::catalogue_env::CatalogueEnvGuard;
use soldr_cli::fetch::manifest_lookup::{
    cached_catalogue_config_count, get_or_fetch, MANIFEST_DISABLE_ENV_VAR, MAX_CACHED_CONFIGS,
    TOOLCHAIN_CATALOGUE_URL_ENV_VAR,
};

/// Backstop so a server thread cannot outlive a wedged test forever. It is not
/// a service budget: `Drop` stops each server as soon as its test is done, and
/// this only bounds the case where that never happens. Sized comfortably under
/// nextest's 120s terminate-after so a genuine regression still fails with the
/// test's own message.
const SERVER_BACKSTOP: Duration = Duration::from_secs(60);

/// A loopback HTTP server that answers every request with the same JSON body
/// and counts how many it answered.
///
/// The request count is the whole point: "the second call was cached" and "the
/// concurrent callers shared one fetch" are both claims about the *origin*,
/// and only the origin can testify. Asserting on returned entries alone cannot
/// tell a cache hit from a silent refetch.
///
/// Deliberately a `std` listener on its own thread rather than a tokio task,
/// so counting works identically under a current-thread and a multi-thread
/// runtime, and so it can be created before the runtime exists.
struct CountingJsonServer {
    url: String,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl CountingJsonServer {
    fn spawn(body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("http://{addr}/asset-index.json");
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let served = Arc::clone(&hits);
        let halt = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("nonblocking fixture listener");
            let deadline = Instant::now() + SERVER_BACKSTOP;
            while !halt.load(Ordering::SeqCst) && Instant::now() < deadline {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                // On macOS a socket accepted from a nonblocking listener
                // inherits O_NONBLOCK; this fixture serves synchronously.
                stream
                    .set_nonblocking(false)
                    .expect("blocking fixture stream");
                let mut request = [0_u8; 4096];
                if stream.read(&mut request).is_err() {
                    continue;
                }
                served.fetch_add(1, Ordering::SeqCst);
                let bytes = body.as_bytes();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    bytes.len()
                );
                let _ = stream.write_all(bytes);
                let _ = stream.flush();
            }
        });
        Self {
            url,
            hits,
            stop,
            handle: Some(handle),
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for CountingJsonServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A one-row v1 catalogue. `sha256` is derived from `nonce` only so distinct
/// fixtures carry distinct, still-valid 64-hex pins.
fn one_row_catalogue(owner: &str, repo: &str, tag: &str, asset: &str, nonce: u128) -> String {
    serde_json::json!({
        "entries": [{
            "owner": owner,
            "repo": repo,
            "tag": tag,
            "asset": asset,
            "url": format!("https://example.invalid/{asset}"),
            "sha256": format!("{nonce:064x}"),
        }]
    })
    .to_string()
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn catalogue_url_override_env_var_works() {
    let env = CatalogueEnvGuard::acquire();
    let server = CountingJsonServer::spawn(one_row_catalogue(
        "test-owner",
        "test-repo",
        "v0.0.1",
        "test-asset.zip",
        1,
    ));
    env.set(TOOLCHAIN_CATALOGUE_URL_ENV_VAR, &server.url);

    let rt = current_thread_runtime();
    rt.block_on(async {
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
        assert_eq!(entry.asset, "test-asset.zip");
        assert_eq!(
            entry.transport.direct_url(),
            Some("https://example.invalid/test-asset.zip")
        );

        let idx2 = get_or_fetch().await;
        assert!(
            Arc::ptr_eq(&idx, &idx2),
            "the same catalogue configuration must return the same cached index"
        );
    });

    assert_eq!(
        server.hits(),
        1,
        "the second call must be served from the cache, not refetched"
    );
}

#[test]
fn url_then_disabled_then_url_is_deterministic() {
    let env = CatalogueEnvGuard::acquire();
    let server = CountingJsonServer::spawn(one_row_catalogue(
        "cycle-owner",
        "cycle-repo",
        "v1.2.3",
        "cycle-asset.zip",
        2,
    ));
    env.set(TOOLCHAIN_CATALOGUE_URL_ENV_VAR, &server.url);

    let rt = current_thread_runtime();
    rt.block_on(async {
        let served = get_or_fetch().await;
        assert_eq!(served.entries.len(), 1);
        assert_eq!(served.entries[0].owner, "cycle-owner");
        assert_eq!(served.entries[0].asset, "cycle-asset.zip");
        assert_eq!(
            served.entries[0].transport.direct_url(),
            Some("https://example.invalid/cycle-asset.zip")
        );

        // "Disabled" is not a place you can fetch from, so it is its own
        // identity and must never be answered by a URL's entry. Before
        // soldr#2951 the disable check sat *after* the cache read, so a
        // populated cache silently satisfied it -- a knob that does nothing,
        // with no way for the caller to tell.
        env.set(MANIFEST_DISABLE_ENV_VAR, "1");
        let disabled = get_or_fetch().await;
        assert!(
            disabled.entries.is_empty(),
            "a disabled catalogue must resolve to an empty index, not the \
             URL configuration's cached entries: {:?}",
            disabled.entries
        );

        // ...and back to the same URL. Its entry must still be resident and be
        // re-selected; the request count below proves it came from the cache.
        env.unset(MANIFEST_DISABLE_ENV_VAR);
        let again = get_or_fetch().await;
        assert_eq!(again.entries.len(), 1);
        assert_eq!(again.entries[0].owner, "cycle-owner");
        assert_eq!(again.entries[0].asset, "cycle-asset.zip");
        assert!(
            Arc::ptr_eq(&served, &again),
            "returning to a configuration must reuse its entry, not rebuild it"
        );
    });

    assert_eq!(
        server.hits(),
        1,
        "the URL configuration must be fetched exactly once across the cycle"
    );
}

#[test]
fn a_second_url_gets_its_own_index() {
    let env = CatalogueEnvGuard::acquire();
    let first = CountingJsonServer::spawn(one_row_catalogue(
        "first-owner",
        "first-repo",
        "v0.0.1",
        "first-asset.zip",
        3,
    ));
    let second = CountingJsonServer::spawn(one_row_catalogue(
        "second-owner",
        "second-repo",
        "v0.0.2",
        "second-asset.zip",
        4,
    ));

    let rt = current_thread_runtime();
    rt.block_on(async {
        env.set(TOOLCHAIN_CATALOGUE_URL_ENV_VAR, &first.url);
        let one = get_or_fetch().await;
        assert_eq!(one.entries[0].owner, "first-owner");

        // The soldr#2951 production bug: the override set after the first
        // fetch was silently discarded and the first URL's index returned.
        env.set(TOOLCHAIN_CATALOGUE_URL_ENV_VAR, &second.url);
        let two = get_or_fetch().await;
        assert_eq!(
            two.entries[0].owner, "second-owner",
            "the second URL's catalogue must win; serving the first one back is the bug"
        );
        assert_eq!(two.entries[0].asset, "second-asset.zip");
        assert!(
            !Arc::ptr_eq(&one, &two),
            "two configurations must not share one index"
        );
    });

    assert_eq!(first.hits(), 1, "the first URL is fetched once");
    assert_eq!(
        second.hits(),
        1,
        "the second URL is fetched once, on its own"
    );
}

#[test]
fn concurrent_callers_share_one_fetch() {
    let env = CatalogueEnvGuard::acquire();
    let server = CountingJsonServer::spawn(one_row_catalogue(
        "solo-owner",
        "solo-repo",
        "v9.9.9",
        "solo-asset.zip",
        5,
    ));
    env.set(TOOLCHAIN_CATALOGUE_URL_ENV_VAR, &server.url);

    // A multi-thread runtime on purpose: under plain `cargo test` this is the
    // shape that used to duplicate the fetch *and* the leak, and nextest's
    // one-process-per-test isolation is exactly why nobody saw it.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    let results = rt.block_on(async {
        // Every task is spawned before any is awaited, so they genuinely
        // overlap rather than running one after another.
        let tasks: Vec<_> = (0..8).map(|_| tokio::spawn(get_or_fetch())).collect();
        let mut out = Vec::with_capacity(tasks.len());
        for task in tasks {
            out.push(task.await.expect("catalogue task panicked"));
        }
        out
    });

    for index in &results {
        assert_eq!(index.entries.len(), 1, "every caller sees the same content");
        assert_eq!(index.entries[0].owner, "solo-owner");
        assert_eq!(index.entries[0].asset, "solo-asset.zip");
        assert!(
            Arc::ptr_eq(index, &results[0]),
            "every caller must share one index, not a per-caller duplicate"
        );
    }
    assert_eq!(
        server.hits(),
        1,
        "single-flight: concurrent callers on one configuration must produce \
         exactly one origin request, got {}",
        server.hits()
    );
}

#[test]
fn distinct_configurations_stay_bounded() {
    let env = CatalogueEnvGuard::acquire();
    let servers: Vec<CountingJsonServer> = (0..MAX_CACHED_CONFIGS + 4)
        .map(|n| {
            CountingJsonServer::spawn(one_row_catalogue(
                &format!("owner-{n}"),
                "bounded-repo",
                "v0.0.1",
                &format!("bounded-asset-{n}.zip"),
                100 + n as u128,
            ))
        })
        .collect();

    let rt = current_thread_runtime();
    rt.block_on(async {
        for (n, server) in servers.iter().enumerate() {
            env.set(TOOLCHAIN_CATALOGUE_URL_ENV_VAR, &server.url);
            let index = get_or_fetch().await;
            assert_eq!(
                index.entries[0].owner,
                format!("owner-{n}"),
                "each configuration must resolve to its own document"
            );
        }
        // `<=`, not `==`: the cache is process-wide and sibling tests in this
        // binary contribute entries of their own. The bound is the claim.
        let resident = cached_catalogue_config_count().await;
        assert!(
            resident <= MAX_CACHED_CONFIGS,
            "{} configurations were fetched but at most {MAX_CACHED_CONFIGS} may stay \
             resident; found {resident}",
            servers.len()
        );
    });
}
