//! Broker server foundation for `running-process-broker-v1`.
//!
//! Phase 4 (#235) grows this module into the pipe accept loop, service
//! definition loader, backend registry, admin verbs, and perf guard.
//! The first slice keeps the core Hello validation and negotiation
//! logic testable without binding sockets or spawning backends.

pub mod admin;
pub mod backend_endpoint_allocator;
pub mod backend_launcher;
pub mod backend_registry;
pub mod broadcast;
pub mod connection;
pub mod control_socket;
#[doc(hidden)]
pub mod deadline_stream;
pub mod fd_pressure;
pub mod handoff;
pub mod handoff_serve;
pub mod hello_handler;
pub mod hello_router;
pub mod idle_coord;
pub mod instance;
pub mod metrics;
pub mod perf_guard;
pub mod recovery;
pub mod serve;
pub mod service_def_loader;
pub mod spawn_coordinator;
pub mod spawn_wait;
pub mod trace_context;
pub mod version_allow_list;

pub use admin::{
    handle_admin_connection, serve_one_admin_socket, AdminBackend, AdminConnectionError,
    AdminFrameError, AdminInodePressure, AdminSnapshot, AdminSpawnBudget, ADMIN_PAYLOAD_PROTOCOL,
    ADMIN_SCHEMA_VERSION,
};
pub use backend_endpoint_allocator::{
    BackendEndpointAllocator, BackendEndpointAllocatorError, DEFAULT_BACKEND_ENDPOINT_ATTEMPTS,
};
pub use backend_launcher::{
    BackendLaunchError, BackendLaunchRequest, BackendLauncher, CommandBackendLauncher,
    BACKEND_ENV_ENDPOINT_NAMESPACE, BACKEND_ENV_ENDPOINT_PATH, BACKEND_ENV_INSTANCE,
    BACKEND_ENV_SERVICE_NAME, BACKEND_ENV_SERVICE_VERSION, BACKEND_ENV_TRACEPARENT,
    BACKEND_ENV_TRACESTATE,
};
pub use backend_registry::{BackendKey, BackendRegistry};
pub use broadcast::{
    BroadcastAck, BroadcastBackend, BroadcastBackendResponse, BroadcastFailure,
    BroadcastFailureReason, BroadcastOperation, BroadcastPolicy, BroadcastResult, BroadcastTimeout,
    LifecycleBroadcastModel, QuiesceReason, DEFAULT_BROADCAST_ACK_TIMEOUT,
};
pub use connection::{
    handle_hello_connection, handle_hello_connection_with,
    handle_hello_connection_with_peer_policy, local_socket_name, peer_identity_from_stream,
    serve_local_socket_connections, serve_local_socket_connections_with,
    serve_local_socket_connections_with_peer_policy, serve_local_socket_connections_with_policy,
    serve_one_local_socket, serve_one_local_socket_with, serve_one_local_socket_with_peer_policy,
    BrokerConnectionError, HelloResponder, PeerCredentialPolicy,
};
pub use control_socket::{
    handle_control_connection_with_peer_policy,
    handle_control_connection_with_peer_policy_and_fd_guard,
    serve_control_socket_connections_with_limit_and_policy,
    serve_control_socket_connections_with_limit_policy_and_post_hello,
    serve_control_socket_connections_with_limit_policy_post_hello_and_fd_guard,
    serve_control_socket_connections_with_policy, ControlSocketConnectionLimit, ControlSocketError,
    ControlSocketReply,
};
pub use fd_pressure::{
    fd_exhaustion_error_for_tests, is_fd_exhaustion_error, FdPressureConfig, FdPressureDecision,
    FdPressureGuard, DEFAULT_FD_PRESSURE_RECOVERY_ACCEPTS, DEFAULT_FD_PRESSURE_RETRY_AFTER_MS,
};
pub use handoff::{
    AcknowledgedHandoff, ExpiredHandoff, HandoffAckError, HandoffAckRegistry,
    HandoffAttemptDecision, HandoffAttemptFailure, HandoffAttemptInputs, HandoffFallbackDecision,
    HandoffFallbackPolicy, HandoffFallbackReason, HandoffFallbackState, HandoffToken,
    HandoffTokenError, HandoffTokenStore, HandoffTokenStoreConfig, PendingHandoffBackend,
    PendingHandoffOverflow, PendingHandoffQueue, PendingHandoffQueueConfig,
    DEFAULT_HANDOFF_ACK_DEADLINE, DEFAULT_HANDOFF_FAILED_ATTEMPTS_PER_WINDOW,
    DEFAULT_HANDOFF_FAILED_ATTEMPT_WINDOW, DEFAULT_HANDOFF_TOKEN_COLLISION_ATTEMPTS,
    DEFAULT_HANDOFF_TOKEN_TTL, DEFAULT_MAX_PENDING_HANDOFFS, DEFAULT_MAX_PENDING_HANDOFF_TOKENS,
    DEFAULT_PENDING_HANDOFF_TTL, HANDOFF_TOKEN_BYTES,
};
pub use handoff_serve::{complete_negotiated_handoff, ServeHandoffContext};
pub use hello_handler::{
    HelloHandler, HelloHandlerError, HelloRequest, PeerIdentity, RegisteredBackend,
};
pub use hello_router::HelloRouter;
pub use idle_coord::{
    BackendIdleCoordinator, BackendIdleDue, BackendIdlePolicy, DEFAULT_BACKEND_IDLE_TIMEOUT,
};
pub use instance::{BrokerInstanceError, BrokerInstanceKey};
pub use perf_guard::{
    enforce_hello_latency_budget, summarize_hello_latencies, HelloLatencySummary, PerfGuardError,
    HELLO_P50_BUDGET, HELLO_P99_BUDGET, HELLO_PERF_GUARD_ENV, HELLO_PERF_SAMPLE_COUNT,
};
pub use recovery::{
    BackendRecoveryDecision, BackendRecoveryPolicy, BackendRecoveryRefusalReason,
    BackendRecoveryState, DEFAULT_RECOVERY_BUDGET_WINDOW, DEFAULT_RECOVERY_RETRY_BACKOFF,
};
pub use serve::{
    build_hello_handler, serve_launching_backends, serve_launching_backends_with_launcher,
    serve_registered_backend, BrokerLaunchServeConfig, BrokerServeConfig, BrokerServeError,
};
pub use service_def_loader::{
    ensure_service_definition_dir, service_definition_dir, service_definition_path,
    validate_service_definition_for_service, write_service_definition, ServiceDefinitionError,
    ServiceDefinitionLoader, SERVICE_DEF_DIR_ENV, SERVICE_DEF_EXTENSION,
};
pub use spawn_coordinator::{
    acquire_spawn_lock, SpawnBeginError, SpawnBudgetConfig, SpawnBudgetSnapshot, SpawnCoordinator,
    SpawnLockError, SpawnLockFileIdentity, SpawnLockGuard, SpawnOutcome, SpawnPermit,
    DEFAULT_SPAWN_ATTEMPTS_PER_WINDOW, DEFAULT_SPAWN_BUDGET_WINDOW,
};
pub use spawn_wait::{
    SpawnWaitDecision, SpawnWaitPolicy, SpawnWaitProbe, DEFAULT_SPAWN_WAIT_HARD_CEILING,
    SPAWN_WAIT_BACKOFF_SEQUENCE,
};
pub use trace_context::TraceContext;
pub use version_allow_list::{check_version_allowed, VersionPolicyBlock};
