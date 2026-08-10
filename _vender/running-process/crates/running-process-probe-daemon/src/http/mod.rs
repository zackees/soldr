//! The daemon's HTTP surface (S13 / #642).
//!
//! A direct `axum` server over the same [`ProbeOps`] core the control socket
//! uses — **not** a proxy in front of it. That is the whole design constraint:
//! if each ingress owned its own policy logic they would drift, and the weaker
//! one would quietly become the way in. Both call `ProbeOps`, so an env value
//! the socket refuses to disclose is one HTTP refuses too, without either side
//! knowing the other exists.
//!
//! # Why HTTP at all, when there is already a socket
//!
//! Two things the framed socket cannot do:
//!
//! - **Serve a browser.** Crash triage is a looking-at-things activity, and a
//!   flame graph is not a thing you read over a length-prefixed protobuf.
//! - **Move a large artifact.** The control socket caps a frame at 16 MiB
//!   because a request frame is buffered whole before it is parsed. A
//!   minidump is routinely larger. [`artifacts`] streams instead, so a 2 GiB
//!   dump costs the daemon one 8 KiB chunk at a time.
//!
//! # Everything is gated
//!
//! Loopback is not a user boundary — see [`auth`]. Every route, the landing
//! page included, goes through the bearer middleware.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use crate::probe_ops::ProbeOps;

pub mod artifacts;
pub mod auth;
pub mod flamegraph;
pub mod handlers;
pub mod ui;

#[cfg(test)]
mod tests;

/// Opt out of the loopback-only bind.
///
/// Off by default and deliberately awkward to set. Binding this surface to a
/// routable address publishes every registered process, every crash artifact,
/// and a stack-capture trigger to the network, protected by one bearer token.
/// That can be the right call inside a container with no other ingress; it is
/// never the right default.
pub const BIND_ALL_ENV: &str = "RUNNING_PROCESS_PROBE_BIND_ALL";

/// Largest request body accepted.
///
/// Requests here are JSON control messages. Artifacts move in the *response*
/// direction, so nothing legitimate is large, and the cap is set where a
/// malformed or hostile body is refused before it is allocated.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Why the HTTP surface refused to start.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// A non-loopback bind was requested without the opt-out.
    #[error(
        "refusing to bind the probe HTTP surface to {addr}: it is not loopback. \
         Set {BIND_ALL_ENV}=1 to publish it deliberately."
    )]
    NonLoopbackBind {
        /// The address that was refused.
        addr: SocketAddr,
    },
    /// The listener could not be created or served.
    #[error("probe HTTP surface failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Shared state behind every handler.
#[derive(Clone)]
pub struct HttpState {
    ops: Arc<ProbeOps>,
    token: Arc<String>,
    profiles: Arc<crate::profile::store::ProfileStore>,
}

impl std::fmt::Debug for HttpState {
    /// Never prints the token.
    ///
    /// A `#[derive(Debug)]` here would put the daemon's authentication secret
    /// into any log line, panic message, or error report that happened to
    /// include the state — which is exactly the sort of accident that turns a
    /// secret into a published one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpState")
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl HttpState {
    /// Build state over `ops`, authenticated by `token`.
    pub fn new(ops: Arc<ProbeOps>, token: String) -> Self {
        Self {
            ops,
            token: Arc::new(token),
            profiles: Arc::new(crate::profile::store::ProfileStore::new()),
        }
    }

    /// Finished profiles, retained briefly so the UI can render and download
    /// them. Ephemeral on purpose — see [`crate::profile::store`].
    pub fn profiles(&self) -> &Arc<crate::profile::store::ProfileStore> {
        &self.profiles
    }

    /// The shared request core. The same one the control socket dispatches to.
    pub fn ops(&self) -> &Arc<ProbeOps> {
        &self.ops
    }

    /// The expected bearer token.
    pub fn token(&self) -> &str {
        &self.token
    }
}

/// Build the router, with authentication already applied.
///
/// The middleware is attached here rather than at the call site so there is no
/// way to construct a router without it — an unauthenticated route added by a
/// later slice would have to delete this line to exist, instead of merely
/// forgetting to add one.
pub fn build_router(state: HttpState) -> Router {
    Router::new()
        .route("/", get(ui::index))
        .route("/assets/{file}", get(ui::asset))
        .route("/v1/ps", get(handlers::ps))
        .route("/v1/crashes", get(handlers::crashes))
        .route("/v1/crashes/stats", get(handlers::crash_stats))
        .route("/v1/snapshot", post(handlers::snapshot))
        .route("/v1/profile", post(handlers::profile))
        .route("/v1/profiles", get(handlers::profiles))
        .route("/v1/profiles/{id}/flamegraph", get(flamegraph::page))
        .route(
            "/v1/profiles/{id}/export/{format}",
            get(flamegraph::download),
        )
        .route("/v1/flame", get(flamegraph::tree))
        .route("/v1/artifacts/{id}", get(artifacts::download))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            MAX_BODY_BYTES,
        ))
        .with_state(state)
}

/// Whether `addr` is safe to bind without an explicit opt-out.
pub fn is_loopback(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Whether the operator opted into publishing this surface.
fn bind_all_opt_in() -> bool {
    std::env::var(BIND_ALL_ENV).is_ok_and(|value| value == "1")
}

/// Check a bind address against the loopback rule.
///
/// Separate from the bind itself, and checked *before* it, so a refused
/// address never briefly exists as a listening socket.
pub fn check_bind(addr: SocketAddr) -> Result<(), HttpError> {
    if is_loopback(&addr) || bind_all_opt_in() {
        Ok(())
    } else {
        Err(HttpError::NonLoopbackBind { addr })
    }
}

/// The default bind address: loopback, port chosen by the OS.
pub fn default_bind() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// Serve the HTTP surface until the process ends.
///
/// Takes an already-bound listener so the caller can learn the port — the
/// daemon has to publish it in the discovery file *before* it starts serving,
/// or a client that read the file would race the listener.
pub async fn serve(listener: tokio::net::TcpListener, state: HttpState) -> Result<(), HttpError> {
    let router = build_router(state);
    serve_router(listener, router).await
}

/// Serve an already-built router.
///
/// Split out so [`spawn`] can construct the router on the *calling* thread:
/// axum validates routes by panicking, and a panic on the serving thread
/// leaves the daemon up with no HTTP surface and only a stray stderr line to
/// say so.
async fn serve_router(listener: tokio::net::TcpListener, router: Router) -> Result<(), HttpError> {
    let addr = listener.local_addr()?;
    check_bind(addr)?;
    axum::serve(listener, router).await.map_err(HttpError::from)
}

/// Start the HTTP surface on its own runtime thread.
///
/// The daemon's accept loop is blocking `std::net`, and rewriting it to be
/// async would mean rewriting the peer-credential handling that is the
/// socket's entire authorization story. A dedicated runtime keeps the two
/// ingresses independent: an HTTP request storm cannot stall registration, and
/// a wedged registration cannot stall the UI.
pub fn spawn(
    addr: SocketAddr,
    state: HttpState,
) -> Result<(SocketAddr, std::thread::JoinHandle<()>), HttpError> {
    check_bind(addr)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    // Build the router here, not on the serving thread. axum validates routes
    // by panicking, and a panic over there would leave the daemon running with
    // no HTTP surface and nothing but a stray line on stderr to say so — which
    // is how a malformed route slipped past a full test run once already.
    let router = build_router(state);

    // Bind on this thread too, so the caller gets the resolved port
    // synchronously and can publish it before anything is served.
    let listener = runtime.block_on(tokio::net::TcpListener::bind(addr))?;
    let bound = listener.local_addr()?;
    serve_on(runtime, listener, bound, router)
}

/// Adopt an already-bound listener and serve on it.
///
/// Lets the daemon learn its HTTP port *before* it does any slow startup work,
/// without the reserve-then-rebind window that would open if the port were
/// discovered with one socket and served with another. The listener that
/// reported the port is the listener that serves it.
pub fn spawn_with_listener(
    listener: std::net::TcpListener,
    state: HttpState,
) -> Result<(SocketAddr, std::thread::JoinHandle<()>), HttpError> {
    let bound = listener.local_addr()?;
    check_bind(bound)?;
    // tokio requires a non-blocking socket; a blocking one silently stalls the
    // whole reactor on the first accept.
    listener.set_nonblocking(true)?;

    let router = build_router(state);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let listener = {
        let _guard = runtime.enter();
        tokio::net::TcpListener::from_std(listener)?
    };
    serve_on(runtime, listener, bound, router)
}

/// Hand a bound listener to a serving thread.
fn serve_on(
    runtime: tokio::runtime::Runtime,
    listener: tokio::net::TcpListener,
    bound: SocketAddr,
    router: Router,
) -> Result<(SocketAddr, std::thread::JoinHandle<()>), HttpError> {
    let handle = std::thread::Builder::new()
        .name("rpprobed-http".into())
        .spawn(move || {
            if let Err(error) = runtime.block_on(serve_router(listener, router)) {
                eprintln!("rpprobed: HTTP surface stopped: {error}");
            }
        })?;

    Ok((bound, handle))
}
