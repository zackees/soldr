//! Serve-side wiring of the handle-passing handoff into the production
//! broker accept loop (#387).
//!
//! When a Hello negotiation issues a one-time handoff token (the client
//! advertised [`CAP_HANDLE_PASSING`] and `Negotiated.handle_passed_token`
//! is non-empty) and the broker was configured with a backend handoff
//! endpoint, this module runs the platform handoff after the `Negotiated`
//! reply has been written:
//!
//! 1. dial the configured backend handoff endpoint,
//! 2. transfer the still-open client connection — `DuplicateHandle` into
//!    the verified backend process on Windows, `sendmsg(SCM_RIGHTS)` over
//!    the handoff connection on Unix — paired with the one-time token,
//! 3. send the [`HandoffOffer`](crate::broker::protocol::HandoffOffer)
//!    frame and wait for the backend [`HandoffAck`] through
//!    [`WireHandoffDelivery`], bounded by the
//!    [`HandoffAckRegistry`] ACK deadline, and
//! 4. on acceptance, relay the handoff-ready EVENT frame
//!    ([`handoff_ready_frame`]) to the waiting client on the connection
//!    that carried Hello.
//!
//! Any failure at any step abandons the handoff through the existing
//! registry APIs and writes **nothing** to the client: the client's
//! bounded relay wait expires and it silently reconnects through the
//! negotiated `backend_pipe`. Handoff failures are logged but are never
//! client errors — the reconnect path stays authoritative (the frozen
//! correctness contract).
//!
//! # Token lifecycle
//!
//! The production [`HelloRouter`](super::hello_router::HelloRouter) builds
//! one ephemeral [`HelloHandler`](super::hello_handler::HelloHandler) per
//! request, so the token store that issued `handle_passed_token` is gone
//! by the time the reply reaches the accept loop. This module re-seeds a
//! connection-local [`HandoffTokenStore`]/[`HandoffAckRegistry`] pair with
//! the exact issued token bytes; the orchestrators then enforce the same
//! exactly-once consumption, revocation-on-failure, and ACK-deadline
//! semantics they were tested with.
//!
//! # Handle-leak contract
//!
//! On Windows, a failure after `DuplicateHandle` succeeded leaks the
//! duplicated handle in the backend process until that process exits
//! ([`WindowsHandoffFallback::leaked_backend_handle`](super::handoff::WindowsHandoffFallback));
//! on Unix the broker keeps ownership of its descriptor, but a duplicate
//! that already reached the backend cannot be reclaimed
//! ([`UnixHandoffFallback::fd_reached_backend`](super::handoff::UnixHandoffFallback)).
//! Both are logged honestly here instead of pretending cleanup happened.

use std::sync::Mutex;
use std::time::Instant;

use prost::Message;

use crate::broker::capabilities::CAP_HANDLE_PASSING;
use crate::broker::client::connect_local_socket;
use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, write_frame, HandoffAck, HelloReply, Negotiated,
};

use super::backend_registry::BackendRegistry;
use super::handoff::{
    handoff_ready_frame, HandoffAckRegistry, HandoffToken, HandoffTokenStore,
    PendingHandoffBackend, WireHandoffDelivery, HANDOFF_TOKEN_BYTES,
};
use super::instance::BrokerInstanceKey;

/// Broker-side inputs shared by every handoff attempted from one serve loop.
pub struct ServeHandoffContext<'a> {
    /// Backend handoff endpoint the broker dials to deliver the connection.
    pub handoff_endpoint: &'a str,
    /// Service name registered for Hello negotiation.
    pub service_name: &'a str,
    /// Backend version registered for Hello negotiation.
    pub service_version: &'a str,
    /// Broker instance key used for registry lookups.
    pub instance: &'a BrokerInstanceKey,
    /// Live backend registry holding the verified backend handle.
    pub registry: &'a Mutex<BackendRegistry>,
}

/// Run the platform handoff for one freshly negotiated Hello connection.
///
/// No-op unless `reply` negotiated handle passing (capability bit plus a
/// well-formed 16-byte `handle_passed_token`). On a completed handoff the
/// handoff-ready EVENT frame is written to `client_stream`; on any failure
/// nothing is written and the client falls back to the `backend_pipe`
/// reconnect on its own. This function never returns an error: serve-side
/// handoff failures are silent optimization failures by contract.
pub fn complete_negotiated_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &mut interprocess::local_socket::Stream,
    reply: &HelloReply,
) {
    let Some(negotiated) = negotiated_with_handoff(reply) else {
        return;
    };
    let Ok(token_bytes) =
        <[u8; HANDOFF_TOKEN_BYTES]>::try_from(negotiated.handle_passed_token.as_slice())
    else {
        return;
    };

    // Re-seed the one-time token issued by the per-request Hello handler
    // (see the module docs) so the orchestrators own its lifecycle.
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = match tokens.issue_with_random128(now, || Ok(token_bytes)) {
        Ok(issued) => issued,
        Err(error) => {
            log_handoff_fallback(&format!("failed to re-seed issued token: {error}"));
            return;
        }
    };
    acks.register(
        issued,
        PendingHandoffBackend::for_service(ctx.service_name),
        now,
    );

    let backend_stream = match connect_local_socket(ctx.handoff_endpoint) {
        Ok(stream) => stream,
        Err(error) => {
            acks.abandon(&mut tokens, &issued);
            log_handoff_fallback(&format!(
                "failed to dial backend handoff endpoint {}: {error}",
                ctx.handoff_endpoint
            ));
            return;
        }
    };
    let io_deadline = Instant::now()
        .checked_add(acks.ack_deadline())
        .unwrap_or_else(Instant::now);
    let mut delivery = WireHandoffDelivery::new_local_socket(
        backend_stream,
        ctx.service_name,
        negotiated.connection_id,
        io_deadline,
    );

    if !run_platform_handoff(
        ctx,
        &*client_stream,
        issued,
        &mut tokens,
        &mut acks,
        &mut delivery,
    ) {
        return;
    }

    // Relay the handoff-ready EVENT to the waiting client. The token was
    // consumed exactly once above; a failed relay write means the client
    // is gone and there is nothing further to clean up on the broker side.
    let ack = HandoffAck {
        token: token_bytes.to_vec(),
        accepted: true,
        error_detail: String::new(),
        correlation_id: negotiated.connection_id,
    };
    let frame = handoff_ready_frame(&ack);
    if let Err(error) = write_frame(client_stream, &frame.encode_to_vec()) {
        log_handoff_fallback(&format!(
            "completed handoff but failed to relay handoff-ready event to client: {error}"
        ));
    }
}

/// Return the negotiated reply when it carries a handoff to complete.
fn negotiated_with_handoff(reply: &HelloReply) -> Option<&Negotiated> {
    let HelloReplyResult::Negotiated(negotiated) = reply.result.as_ref()? else {
        return None;
    };
    if negotiated.server_capabilities & CAP_HANDLE_PASSING == 0
        || negotiated.handle_passed_token.is_empty()
    {
        return None;
    }
    Some(negotiated)
}

#[cfg(windows)]
fn run_platform_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &interprocess::local_socket::Stream,
    issued: HandoffToken,
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    delivery: &mut WireHandoffDelivery<interprocess::local_socket::Stream>,
) -> bool {
    use std::os::windows::io::{AsHandle, AsRawHandle};

    use super::handoff::{
        execute_verified_windows_handoff, WindowsHandleValue, WindowsHandoffOutcome,
    };

    let pipe_handle = match client_stream {
        interprocess::local_socket::Stream::NamedPipe(stream) => {
            WindowsHandleValue::new(stream.as_handle().as_raw_handle() as usize)
        }
    };

    // The accept loop is sequential, so holding the registry lock for the
    // duration of one handoff cannot deadlock against another connection.
    let registry = ctx
        .registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(backend) = registry.get(ctx.instance, ctx.service_name, ctx.service_version) else {
        acks.abandon(tokens, &issued);
        log_handoff_fallback("registered backend disappeared before handoff delivery");
        return false;
    };

    match execute_verified_windows_handoff(backend, pipe_handle, issued, tokens, acks, delivery) {
        WindowsHandoffOutcome::Completed(_) => true,
        WindowsHandoffOutcome::FallbackToReconnect(fallback) => {
            let leak = match fallback.leaked_backend_handle {
                Some(handle) => format!(
                    "; duplicated handle {:#x} leaks in backend pid {} until it exits",
                    handle.get(),
                    backend.daemon_process.pid
                ),
                None => String::new(),
            };
            log_handoff_fallback(&format!(
                "abandoned at {:?} stage: {}{leak}",
                fallback.stage, fallback.detail
            ));
            false
        }
    }
}

#[cfg(unix)]
fn run_platform_handoff(
    ctx: &ServeHandoffContext<'_>,
    client_stream: &interprocess::local_socket::Stream,
    issued: HandoffToken,
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    delivery: &mut WireHandoffDelivery<interprocess::local_socket::Stream>,
) -> bool {
    use std::cell::RefCell;
    use std::os::fd::{AsFd, AsRawFd};
    use std::time::Instant;

    use super::handoff::{
        execute_unix_handoff_with_transport, try_send_scm_rights_over, HandoffDelivery,
        HandoffDeliveryError, ScmRightsAttempt, ScmRightsError, ScmRightsResult,
        UnixFileDescriptor, UnixHandoffAckWait, UnixHandoffOutcome, UnixHandoffRequest,
        UnixHandoffSocket, WindowsHandleValue,
    };

    let client_fd = match client_stream {
        interprocess::local_socket::Stream::UdSocket(stream) => stream.as_fd().as_raw_fd(),
    };
    let backend_fd = match delivery.stream() {
        interprocess::local_socket::Stream::UdSocket(stream) => stream.as_fd().as_raw_fd(),
    };
    let request = UnixHandoffRequest::new(
        UnixFileDescriptor::new(client_fd),
        UnixHandoffSocket::new(ctx.handoff_endpoint),
        issued,
    );

    // The transport closure and the ACK wait both need the one wire
    // delivery channel; they run strictly one after the other, so a
    // RefCell resolves the shared mutable borrow safely.
    let delivery = RefCell::new(delivery);
    let transport = |attempt: &ScmRightsAttempt| -> ScmRightsResult {
        let mut delivery = delivery.borrow_mut();
        let sent = try_send_scm_rights_over(backend_fd, attempt)?;
        delivery
            .deliver(WindowsHandleValue::new(0), &attempt.handoff_token)
            .map_err(|error| {
                log_handoff_fallback(&format!("failed to write HandoffOffer frame: {error}"));
                ScmRightsError::SendFailed {
                    fd: attempt.fd.raw(),
                    socket: attempt.backend_socket.path.clone(),
                    raw_os_error: None,
                }
            })?;
        Ok(sent)
    };

    struct DeliveryAckWait<'a, 'b> {
        delivery: &'a RefCell<&'b mut WireHandoffDelivery<interprocess::local_socket::Stream>>,
    }
    impl UnixHandoffAckWait for DeliveryAckWait<'_, '_> {
        fn await_backend_ack(
            &mut self,
            token: &HandoffToken,
            deadline: Instant,
        ) -> Result<Instant, HandoffDeliveryError> {
            self.delivery
                .borrow_mut()
                .await_backend_ack(token, deadline)
        }
    }
    let mut ack_wait = DeliveryAckWait {
        delivery: &delivery,
    };

    match execute_unix_handoff_with_transport(tokens, acks, &request, transport, &mut ack_wait) {
        UnixHandoffOutcome::Completed(_) => true,
        UnixHandoffOutcome::FallbackToReconnect(fallback) => {
            let reached = if fallback.fd_reached_backend {
                "; a duplicated descriptor already reached the backend and lives until it closes it"
            } else {
                ""
            };
            log_handoff_fallback(&format!(
                "abandoned at {:?} stage: {}{reached}",
                fallback.stage, fallback.detail
            ));
            false
        }
    }
}

/// Log one silent serve-side handoff fallback.
///
/// The broker has no tracing subscriber on the client-feature build; the
/// existing convention (lifecycle SID probing, the broker binary) is
/// stderr. Failures here are silent toward the client by contract.
fn log_handoff_fallback(detail: &str) {
    eprintln!("running-process-broker: handoff fallback: {detail}");
}
