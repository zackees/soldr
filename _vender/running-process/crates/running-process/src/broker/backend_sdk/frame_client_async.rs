//! Async flavor of [`FrameClient`] for tokio daemons (#414).
//!
//! Mirrors the blocking [`FrameClient`] surface — same request-id
//! correlation, same payload-protocol echo check, same
//! `NotAResponse`/`RequestIdMismatch`/`PayloadProtocolMismatch`
//! error mapping — but exposes `.await`-able entry points so async
//! daemons (zccache, soldr, clud) don't have to `spawn_blocking` at
//! every call site.
//!
//! ## Implementation note (frozen wire, opt-in async)
//!
//! v1 ships the canonical frame wire as a synchronous
//! [`FrameClient`] backed by `interprocess::local_socket::Stream`.
//! That code path is the wire-of-record — it owns the request-id
//! counter, the response correlation, and the precise error mapping
//! that every consumer's golden-bytes test pins against.
//! Re-implementing the same wire against
//! `tokio::io::AsyncRead`/`AsyncWrite` would duplicate the surface
//! for no observable behavior gain and risk drift.
//!
//! Instead, [`AsyncFrameClient`] holds an owned blocking
//! [`FrameClient`] and runs each `request` (and the initial connect)
//! on `tokio::task::spawn_blocking`. The caller gets the async
//! surface so an `await` from a tokio task replaces a manual
//! `spawn_blocking` wrap at the call site, the wire is unchanged,
//! and the runtime worker thread is freed during each round-trip.
//!
//! Available when the `client-async` cargo feature is enabled.
//!
//! [`FrameClient`]: super::FrameClient

use std::time::Duration;

use crate::broker::backend_sdk::frame_client::{FrameClient, FrameClientError};
use crate::broker::protocol::{Endpoint, Frame};

/// Async request/response client for a backend daemon's frame lane.
///
/// Async counterpart of [`super::FrameClient`]; same wire contract,
/// same request-id correlation (monotonically increasing, skipping
/// zero), same payload-protocol echo and `RESPONSE` kind validation.
///
/// ```no_run
/// # #[cfg(feature = "client-async")]
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use running_process::broker::backend_sdk::AsyncFrameClient;
/// use running_process::broker::protocol::Endpoint;
///
/// let endpoint = Endpoint::unix_socket("my-daemon", "/tmp/my-daemon.sock")?;
/// let mut client = AsyncFrameClient::connect(&endpoint).await?;
/// let response = client.request(0x7A63, b"ping".to_vec()).await?;
/// assert_eq!(response.payload, b"pong");
/// # Ok(()) }
/// ```
pub struct AsyncFrameClient {
    /// Owned blocking client moved across `spawn_blocking` calls.
    ///
    /// `Option` so [`Self::request`] can `take` the inner client into
    /// the worker thread and put it back on completion without
    /// fighting the borrow checker over `&mut self`. If a request
    /// panics inside `spawn_blocking`, the slot stays `None` and
    /// every subsequent request fails fast with
    /// [`FrameClientError::Framing`] of [`std::io::ErrorKind::Other`].
    inner: Option<FrameClient>,
}

impl AsyncFrameClient {
    /// Connect to a backend endpoint, running the blocking connect on
    /// a `spawn_blocking` worker.
    pub async fn connect(endpoint: &Endpoint) -> Result<Self, FrameClientError> {
        let endpoint = endpoint.clone();
        let inner = match tokio::task::spawn_blocking(move || FrameClient::connect(&endpoint)).await
        {
            Ok(result) => result?,
            Err(join_err) => return Err(join_error_to_client(join_err)),
        };
        Ok(Self { inner: Some(inner) })
    }

    /// Connect with a hard deadline on the connect itself.
    ///
    /// On expiry returns
    /// [`FrameClientError::Connect`] with
    /// [`std::io::ErrorKind::TimedOut`]; the spawned worker thread is
    /// detached but harmless (the blocking connect either completes
    /// and is dropped or the OS-level connect timeout fires).
    pub async fn connect_with_timeout(
        endpoint: &Endpoint,
        timeout: Duration,
    ) -> Result<Self, FrameClientError> {
        match tokio::time::timeout(timeout, Self::connect(endpoint)).await {
            Ok(result) => result,
            Err(_) => Err(FrameClientError::Connect(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "async frame client connect timed out",
            ))),
        }
    }

    /// Wrap an already-connected blocking [`FrameClient`] (e.g. one
    /// opened through a verified
    /// [`BackendHandle`](crate::broker::backend_handle::BackendHandle)
    /// in synchronous setup code and now handed to an async runtime).
    pub fn from_blocking(client: FrameClient) -> Self {
        Self {
            inner: Some(client),
        }
    }

    /// Send one request frame and await its response.
    ///
    /// Same correlation contract as [`super::FrameClient::request`]:
    /// the request id is assigned monotonically, the response must
    /// echo it and the request's `payload_protocol`, and the
    /// response frame kind must be `RESPONSE`. The blocking
    /// round-trip runs on `tokio::task::spawn_blocking`.
    pub async fn request(
        &mut self,
        payload_protocol: u32,
        payload: Vec<u8>,
    ) -> Result<Frame, FrameClientError> {
        let mut client = self.inner.take().ok_or_else(|| {
            FrameClientError::Framing(crate::broker::protocol::FramingError::Io(
                std::io::Error::other("async frame client was poisoned by a prior request panic"),
            ))
        })?;
        let join = tokio::task::spawn_blocking(move || {
            let result = client.request(payload_protocol, payload);
            (client, result)
        })
        .await;
        match join {
            Ok((client, result)) => {
                self.inner = Some(client);
                result
            }
            Err(join_err) => Err(join_error_to_client(join_err)),
        }
    }

    /// Like [`Self::request`] but bounds the entire round-trip by
    /// `timeout`. On expiry the inner blocking client is dropped
    /// (because the worker future is cancelled) and the next
    /// request returns the poisoned-client error; callers that want
    /// to reuse the connection across a soft timeout should retry
    /// without the timeout or reopen the client.
    pub async fn request_with_timeout(
        &mut self,
        payload_protocol: u32,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<Frame, FrameClientError> {
        match tokio::time::timeout(timeout, self.request(payload_protocol, payload)).await {
            Ok(result) => result,
            Err(_) => Err(FrameClientError::Framing(
                crate::broker::protocol::FramingError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "async frame client request timed out",
                )),
            )),
        }
    }

    /// The request id the next [`Self::request`] call will use, or
    /// `None` if the client has been poisoned by a prior request
    /// panic.
    pub fn next_request_id(&self) -> Option<u64> {
        self.inner.as_ref().map(FrameClient::next_request_id)
    }

    /// Recover the owned blocking [`FrameClient`], or `None` if a prior
    /// request panicked inside `spawn_blocking` and poisoned the slot.
    ///
    /// Used by [`AsyncBrokerSession::into_backend_io`] to take the
    /// negotiated socket back out without crossing another async hop.
    ///
    /// [`AsyncBrokerSession::into_backend_io`]: crate::broker::adopt::AsyncBrokerSession::into_backend_io
    pub fn into_blocking(self) -> Option<FrameClient> {
        self.inner
    }
}

fn join_error_to_client(join_err: tokio::task::JoinError) -> FrameClientError {
    FrameClientError::Framing(crate::broker::protocol::FramingError::Io(
        std::io::Error::other(format!(
            "async frame client worker thread panicked or was cancelled: {join_err}"
        )),
    ))
}
