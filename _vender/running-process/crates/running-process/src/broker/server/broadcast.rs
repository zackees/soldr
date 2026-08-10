//! Backend lifecycle broadcast model.
//!
//! This module only models broker-to-backend lifecycle control fanout. It does
//! not open sockets, send frames, or define backend RPC. Callers can use the
//! result shape here when later Phase 5 work wires real maintenance requests
//! into live backend connections.

use std::path::PathBuf;
use std::time::Duration;

use super::backend_registry::BackendKey;

/// Default time allowed for one backend to acknowledge a lifecycle broadcast.
pub const DEFAULT_BROADCAST_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Broker lifecycle broadcast operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BroadcastOperation {
    /// Ask backends to release file handles below a path prefix.
    ReleaseHandles {
        /// Path prefix whose handles should be released.
        path_prefix: PathBuf,
    },
    /// Ask backends to stop accepting new work and drain.
    Quiesce {
        /// Reason the backend is being asked to drain.
        reason: QuiesceReason,
    },
}

impl BroadcastOperation {
    /// Build a release-handles operation for `path_prefix`.
    pub fn release_handles(path_prefix: impl Into<PathBuf>) -> Self {
        Self::ReleaseHandles {
            path_prefix: path_prefix.into(),
        }
    }

    /// Build a quiesce operation.
    pub fn quiesce(reason: QuiesceReason) -> Self {
        Self::Quiesce { reason }
    }
}

/// Reason attached to a broker quiesce broadcast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuiesceReason {
    /// Backend crossed its configured idle threshold.
    IdleTimeout,
    /// Broker is shutting down gracefully.
    BrokerShutdown,
    /// Operator or maintenance policy requested a drain.
    Maintenance,
}

/// Broadcast timeout policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BroadcastPolicy {
    /// Maximum time to wait for each backend acknowledgement.
    pub ack_timeout: Duration,
}

impl BroadcastPolicy {
    /// Build a policy, clamping zero timeout to a non-zero floor.
    pub fn new(ack_timeout: Duration) -> Self {
        Self {
            ack_timeout: if ack_timeout.is_zero() {
                Duration::from_millis(1)
            } else {
                ack_timeout
            },
        }
    }
}

impl Default for BroadcastPolicy {
    fn default() -> Self {
        Self {
            ack_timeout: DEFAULT_BROADCAST_ACK_TIMEOUT,
        }
    }
}

/// Testable model of one backend's lifecycle-broadcast endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BroadcastBackend {
    /// Backend key receiving broadcasts.
    pub key: BackendKey,
    live: bool,
    response: BroadcastBackendResponse,
    received: Vec<BroadcastOperation>,
}

impl BroadcastBackend {
    /// Build a live backend endpoint that acknowledges broadcasts.
    pub fn live(key: BackendKey) -> Self {
        Self {
            key,
            live: true,
            response: BroadcastBackendResponse::Ack,
            received: Vec::new(),
        }
    }

    /// Build a dead backend endpoint that should be skipped.
    pub fn dead(key: BackendKey) -> Self {
        Self {
            key,
            live: false,
            response: BroadcastBackendResponse::Ack,
            received: Vec::new(),
        }
    }

    /// Set the modeled response returned when this backend receives a request.
    pub fn with_response(mut self, response: BroadcastBackendResponse) -> Self {
        self.response = response;
        self
    }

    /// Mark the endpoint live or dead.
    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    /// Return true when this backend should receive broadcasts.
    pub fn is_live(&self) -> bool {
        self.live
    }

    /// Operations that reached this backend in the model.
    pub fn received_operations(&self) -> &[BroadcastOperation] {
        &self.received
    }
}

/// Modeled backend response to a lifecycle broadcast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BroadcastBackendResponse {
    /// Backend acknowledged the operation.
    Ack,
    /// Backend did not acknowledge before the broadcast timeout.
    Timeout,
    /// Backend rejected or failed the operation.
    Failure(BroadcastFailureReason),
}

/// Failure reason returned by a modeled backend endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BroadcastFailureReason {
    /// Backend does not support the requested lifecycle operation.
    UnsupportedOperation,
    /// Backend rejected the operation.
    Rejected,
    /// Backend failed while processing the operation.
    BackendError,
}

/// In-repo lifecycle broadcast model for broker-managed backends.
#[derive(Debug)]
pub struct LifecycleBroadcastModel {
    policy: BroadcastPolicy,
    backends: Vec<BroadcastBackend>,
}

impl LifecycleBroadcastModel {
    /// Create an empty model with the default timeout policy.
    pub fn new() -> Self {
        Self::with_policy(BroadcastPolicy::default())
    }

    /// Create an empty model with an explicit timeout policy.
    pub fn with_policy(policy: BroadcastPolicy) -> Self {
        Self {
            policy,
            backends: Vec::new(),
        }
    }

    /// Register or replace one backend endpoint.
    pub fn register_backend(&mut self, backend: BroadcastBackend) -> Option<BroadcastBackend> {
        if let Some(existing) = self
            .backends
            .iter_mut()
            .find(|existing| existing.key == backend.key)
        {
            return Some(std::mem::replace(existing, backend));
        }

        self.backends.push(backend);
        None
    }

    /// Return a registered backend by key.
    pub fn backend(&self, key: &BackendKey) -> Option<&BroadcastBackend> {
        self.backends.iter().find(|backend| &backend.key == key)
    }

    /// Return all registered backend endpoints in insertion order.
    pub fn backends(&self) -> &[BroadcastBackend] {
        &self.backends
    }

    /// Broadcast a release-handles operation to all live backends.
    pub fn release_handles_under_path(
        &mut self,
        path_prefix: impl Into<PathBuf>,
    ) -> BroadcastResult {
        self.broadcast(BroadcastOperation::release_handles(path_prefix))
    }

    /// Broadcast a quiesce operation to all live backends.
    pub fn quiesce(&mut self, reason: QuiesceReason) -> BroadcastResult {
        self.broadcast(BroadcastOperation::quiesce(reason))
    }

    /// Broadcast one lifecycle operation to all live backends.
    pub fn broadcast(&mut self, operation: BroadcastOperation) -> BroadcastResult {
        let mut result = BroadcastResult::new(operation.clone());

        for backend in &mut self.backends {
            if !backend.live {
                result.skipped_dead.push(backend.key.clone());
                continue;
            }

            backend.received.push(operation.clone());
            match backend.response {
                BroadcastBackendResponse::Ack => {
                    result.acks.push(BroadcastAck {
                        key: backend.key.clone(),
                    });
                }
                BroadcastBackendResponse::Timeout => {
                    result.timeouts.push(BroadcastTimeout {
                        key: backend.key.clone(),
                        timeout: self.policy.ack_timeout,
                    });
                }
                BroadcastBackendResponse::Failure(reason) => {
                    result.failures.push(BroadcastFailure {
                        key: backend.key.clone(),
                        reason,
                    });
                }
            }
        }

        result
    }
}

impl Default for LifecycleBroadcastModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Broadcast result across all registered model endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BroadcastResult {
    /// Operation that was broadcast.
    pub operation: BroadcastOperation,
    /// Backends that acknowledged the operation.
    pub acks: Vec<BroadcastAck>,
    /// Live backends that timed out.
    pub timeouts: Vec<BroadcastTimeout>,
    /// Live backends that failed the operation.
    pub failures: Vec<BroadcastFailure>,
    /// Dead backends skipped before fanout.
    pub skipped_dead: Vec<BackendKey>,
}

impl BroadcastResult {
    fn new(operation: BroadcastOperation) -> Self {
        Self {
            operation,
            acks: Vec::new(),
            timeouts: Vec::new(),
            failures: Vec::new(),
            skipped_dead: Vec::new(),
        }
    }

    /// Number of live backends that received the broadcast.
    pub fn sent_count(&self) -> usize {
        self.acks.len() + self.timeouts.len() + self.failures.len()
    }

    /// Return true when all live backends acknowledged the operation.
    pub fn all_live_backends_acked(&self) -> bool {
        self.timeouts.is_empty() && self.failures.is_empty()
    }
}

/// Successful backend acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BroadcastAck {
    /// Backend that acknowledged the operation.
    pub key: BackendKey,
}

/// Backend timeout during broadcast acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BroadcastTimeout {
    /// Backend that timed out.
    pub key: BackendKey,
    /// Timeout from the active broadcast policy.
    pub timeout: Duration,
}

/// Backend failure during broadcast handling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BroadcastFailure {
    /// Backend that failed the operation.
    pub key: BackendKey,
    /// Failure reason.
    pub reason: BroadcastFailureReason,
}
