//! Client helpers for daemon-owned session observer subscriptions
//! (#221 Phase 2 / #429).
//!
//! Mirrors `client::telemetry` but for event-stream observer payloads
//! (process lifecycle Started/Exited, eventually file/network/process
//! events in Phase 3). The client wraps the three IPC round trips:
//!
//! - [`DaemonClient::register_session_observer`]
//! - [`DaemonClient::unregister_session_observer`]
//! - [`DaemonClient::get_session_observer_status`]
//!
//! Registrations live on the per-session struct on the daemon side, so
//! they survive the IPC connection going away. Events emitted while no
//! consumer is draining the bounded channel are accounted for via
//! [`SessionObserverStatus::missed_events`] under `DropOldest` policy.

use std::sync::mpsc;

use crate::client::{ClientError, DaemonClient};
use crate::observer::{EventCategory, ObserverEvent, ObserverSubscriber};
use crate::proto::daemon::{
    DaemonRequest, GetSessionObserverStatusRequest,
    ObserverBackpressure as ProtoObserverBackpressure,
    ObserverSessionKind as ProtoObserverSessionKind, RegisterSessionObserverRequest, RequestType,
    StatusCode, UnregisterSessionObserverRequest,
};

/// Session transport that owns the observed process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionObserverKind {
    /// Daemon-owned pseudo-terminal session.
    Pty,
    /// Daemon-owned pipe-backed session.
    Pipe,
}

/// Backpressure policy for the daemon-side bounded sink.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionObserverBackpressure {
    /// Drop the newest event and increment `missed_events` when the sink is full.
    #[default]
    DropOldest,
    /// Block the emitter (daemon-side) until the sink has room.
    Block,
}

/// Request used to register a daemon-managed observer subscription on a
/// session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionObserverRequest {
    /// Session identifier returned by the spawn API.
    pub session_id: String,
    /// Kind of session that owns `session_id`.
    pub session_kind: SessionObserverKind,
    /// Requested event categories. Empty defaults to `[Lifecycle]` on the
    /// server, matching the Phase 1 in-process default.
    pub categories: Vec<EventCategory>,
    /// Bounded sink capacity. `0` means use the daemon default (1024).
    pub ring_capacity_events: u32,
    /// Sink backpressure policy.
    pub backpressure: SessionObserverBackpressure,
}

impl SessionObserverRequest {
    /// Construct a request with default `[Lifecycle]` categories, daemon
    /// default capacity, and `DropOldest` backpressure.
    pub fn new(session_id: impl Into<String>, session_kind: SessionObserverKind) -> Self {
        Self {
            session_id: session_id.into(),
            session_kind,
            categories: vec![EventCategory::Lifecycle],
            ring_capacity_events: 0,
            backpressure: SessionObserverBackpressure::DropOldest,
        }
    }

    /// Override the requested category set.
    pub fn categories(mut self, categories: impl IntoIterator<Item = EventCategory>) -> Self {
        self.categories = categories.into_iter().collect();
        self
    }

    /// Override the bounded sink capacity. `0` keeps the daemon default.
    pub fn ring_capacity_events(mut self, capacity: u32) -> Self {
        self.ring_capacity_events = capacity;
        self
    }

    /// Override the backpressure policy.
    pub fn backpressure(mut self, backpressure: SessionObserverBackpressure) -> Self {
        self.backpressure = backpressure;
        self
    }
}

/// Outcome of [`DaemonClient::register_session_observer`].
///
/// Holds the server-assigned [`subscriber_id`](Self::subscriber_id) plus a
/// local [`ObserverSubscriber`] handle. The local subscriber is wired to a
/// connection-local `mpsc` channel: future work will pump
/// [`crate::observer::ObserverEvent`] frames into that channel from a
/// streaming IPC attach. In this PR no streaming attach exists yet, so the
/// channel stays empty — clients can still observe activity by polling
/// [`DaemonClient::get_session_observer_status`].
pub struct RemoteObserverSubscription {
    /// Server-assigned UUID identifying this subscription. Pass back to
    /// [`DaemonClient::unregister_session_observer`] /
    /// [`DaemonClient::get_session_observer_status`].
    pub subscriber_id: String,
    /// Local subscriber whose channel is currently inert.
    ///
    /// Held here so the surface matches the Phase 1 in-process API and so
    /// callers can pass it to existing code that expects an
    /// `ObserverSubscriber`. Once a streaming-attach frame is added the
    /// sender half (held internally) will start feeding this receiver.
    pub subscriber: ObserverSubscriber,
    // Local sender held so the channel does not disconnect immediately.
    // Future work will use this to dispatch IPC-delivered events.
    _local_sender: mpsc::Sender<ObserverEvent>,
}

/// Current daemon-side counters for a registered subscription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionObserverStatus {
    /// Cumulative events that arrived while the bounded sink was full.
    pub missed_events: u64,
    /// True once the downstream receiver has disconnected.
    pub disconnected: bool,
    /// Cumulative events successfully delivered into the bounded sink.
    pub delivered_events: u64,
}

impl DaemonClient {
    /// Register a session-scoped observer subscription on the daemon.
    pub fn register_session_observer(
        &mut self,
        request: &SessionObserverRequest,
    ) -> Result<RemoteObserverSubscription, ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::RegisterSessionObserver.into(),
            protocol_version: 1,
            register_session_observer: Some(RegisterSessionObserverRequest {
                session_id: request.session_id.clone(),
                session_kind: proto_session_kind(request.session_kind) as i32,
                categories: request
                    .categories
                    .iter()
                    .map(|c| event_category_to_u32(*c))
                    .collect(),
                ring_capacity_events: request.ring_capacity_events,
                backpressure: proto_backpressure(request.backpressure) as i32,
            }),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)?;
        let payload = response
            .register_session_observer
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "register_session_observer response missing payload".into(),
            })?;
        // Create a local inert channel for the surface
        // — future streaming attach will pump events into the sender.
        let (tx, rx) = mpsc::channel();
        Ok(RemoteObserverSubscription {
            subscriber_id: payload.subscriber_id,
            subscriber: ObserverSubscriber::from_receiver(rx),
            _local_sender: tx,
        })
    }

    /// Unregister a previously registered subscription.
    pub fn unregister_session_observer(
        &mut self,
        session_kind: SessionObserverKind,
        session_id: &str,
        subscriber_id: &str,
    ) -> Result<(), ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::UnregisterSessionObserver.into(),
            protocol_version: 1,
            unregister_session_observer: Some(UnregisterSessionObserverRequest {
                session_id: session_id.to_string(),
                session_kind: proto_session_kind(session_kind) as i32,
                subscriber_id: subscriber_id.to_string(),
            }),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)
    }

    /// Fetch current counters for a registered subscription.
    pub fn get_session_observer_status(
        &mut self,
        session_kind: SessionObserverKind,
        session_id: &str,
        subscriber_id: &str,
    ) -> Result<SessionObserverStatus, ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::GetSessionObserverStatus.into(),
            protocol_version: 1,
            get_session_observer_status: Some(GetSessionObserverStatusRequest {
                session_id: session_id.to_string(),
                session_kind: proto_session_kind(session_kind) as i32,
                subscriber_id: subscriber_id.to_string(),
            }),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)?;
        let payload = response
            .get_session_observer_status
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "get_session_observer_status response missing payload".into(),
            })?;
        Ok(SessionObserverStatus {
            missed_events: payload.missed_events,
            disconnected: payload.disconnected,
            delivered_events: payload.delivered_events,
        })
    }
}

fn ensure_ok(response: &crate::proto::daemon::DaemonResponse) -> Result<(), ClientError> {
    if response.code == StatusCode::Ok as i32 {
        return Ok(());
    }
    let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
    Err(ClientError::Server {
        code,
        message: response.message.clone(),
    })
}

fn proto_session_kind(kind: SessionObserverKind) -> ProtoObserverSessionKind {
    match kind {
        SessionObserverKind::Pty => ProtoObserverSessionKind::Pty,
        SessionObserverKind::Pipe => ProtoObserverSessionKind::Pipe,
    }
}

fn proto_backpressure(b: SessionObserverBackpressure) -> ProtoObserverBackpressure {
    match b {
        SessionObserverBackpressure::DropOldest => ProtoObserverBackpressure::DropOldest,
        SessionObserverBackpressure::Block => ProtoObserverBackpressure::Block,
    }
}

fn event_category_to_u32(category: EventCategory) -> u32 {
    match category {
        EventCategory::Lifecycle => 0,
        EventCategory::File => 1,
        EventCategory::Network => 2,
        EventCategory::Process => 3,
    }
}
