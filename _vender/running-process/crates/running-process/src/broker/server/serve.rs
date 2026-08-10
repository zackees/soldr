//! Broker serve-mode wiring for registered and launch-backed backends.
//!
//! Phase 4 grows the long-lived daemon incrementally. This module connects the
//! existing service-definition loader, broker instance routing, backend
//! registry, backend launch coordination, and framed local-socket accept loop.
//! Tests can still request bounded runs while the CLI defaults to accepting
//! until process exit.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use crate::broker::backend_handle::{BackendHandle, BackendHandleError, DaemonProcess};
use crate::broker::backend_lifecycle::identity::IdentityError;
use crate::broker::lifecycle::sid::SidError;
use crate::broker::protocol::{Endpoint, ServiceDefinition};

use super::admin::AdminSnapshot;
use super::backend_launcher::{BackendLauncher, CommandBackendLauncher};
use super::backend_registry::BackendRegistry;
use super::connection::{BrokerConnectionError, PeerCredentialPolicy};
use super::control_socket::{
    serve_control_socket_connections_with_limit_policy_post_hello_and_fd_guard,
    ControlSocketConnectionLimit, ControlSocketError,
};
use super::fd_pressure::FdPressureGuard;
use super::handoff_serve::{complete_negotiated_handoff, ServeHandoffContext};
use super::hello_handler::{HelloHandler, HelloHandlerError};
use super::hello_router::HelloRouter;
use super::instance::{BrokerInstanceError, BrokerInstanceKey};
use super::service_def_loader::{
    service_definition_dir, ServiceDefinitionError, ServiceDefinitionLoader,
};
use super::spawn_coordinator::SpawnCoordinator;
use super::version_allow_list::{check_version_allowed, VersionPolicyBlock};

/// Configuration for a bounded broker serve-mode run.
#[derive(Clone, Debug)]
pub struct BrokerServeConfig {
    /// Local socket path or Windows pipe name to bind.
    pub socket_path: String,
    /// Service definition to load.
    pub service_name: String,
    /// Backend version to register for Hello negotiation.
    pub service_version: String,
    /// Direct backend endpoint returned to negotiated clients.
    pub backend_endpoint: String,
    /// Directory containing `<service>.servicedef` protobuf files.
    pub service_definition_dir: PathBuf,
    /// Optional number of control-socket connections to accept before returning.
    pub max_connections: Option<NonZeroUsize>,
    /// Optional backend handoff endpoint enabling the Phase 6 handle-passing
    /// optimization (#387). `None` (the default) disables handoff entirely:
    /// negotiated clients always reconnect through `backend_endpoint`. This
    /// matches the opt-in Phase 6 gate in `docs/v1-rollout-policy.md`.
    pub handoff_endpoint: Option<String>,
}

/// Configuration for serve mode that launches backends on Hello miss.
#[derive(Clone, Debug)]
pub struct BrokerLaunchServeConfig {
    /// Local socket path or Windows pipe name to bind.
    pub socket_path: String,
    /// Directory containing `<service>.servicedef` protobuf files.
    pub service_definition_dir: PathBuf,
    /// Optional number of control-socket connections to accept before returning.
    pub max_connections: Option<NonZeroUsize>,
}

impl BrokerServeConfig {
    /// Build a serve config using the platform service-definition directory.
    pub fn new(
        socket_path: impl Into<String>,
        service_name: impl Into<String>,
        service_version: impl Into<String>,
        backend_endpoint: impl Into<String>,
        max_connections: usize,
    ) -> Result<Self, BrokerServeError> {
        Ok(Self {
            socket_path: socket_path.into(),
            service_name: service_name.into(),
            service_version: service_version.into(),
            backend_endpoint: backend_endpoint.into(),
            service_definition_dir: service_definition_dir(),
            max_connections: Some(
                NonZeroUsize::new(max_connections)
                    .ok_or(BrokerServeError::InvalidMaxConnections)?,
            ),
            handoff_endpoint: None,
        })
    }

    /// Build an unbounded serve config using the platform service-definition
    /// directory.
    pub fn unbounded(
        socket_path: impl Into<String>,
        service_name: impl Into<String>,
        service_version: impl Into<String>,
        backend_endpoint: impl Into<String>,
    ) -> Self {
        Self {
            socket_path: socket_path.into(),
            service_name: service_name.into(),
            service_version: service_version.into(),
            backend_endpoint: backend_endpoint.into(),
            service_definition_dir: service_definition_dir(),
            max_connections: None,
            handoff_endpoint: None,
        }
    }

    /// Override the service-definition directory.
    pub fn with_service_definition_dir(mut self, root: impl Into<PathBuf>) -> Self {
        self.service_definition_dir = root.into();
        self
    }

    /// Opt in to the Phase 6 handle-passing handoff by configuring the
    /// backend handoff endpoint the broker dials after negotiation (#387).
    pub fn with_handoff_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.handoff_endpoint = Some(endpoint.into());
        self
    }

    /// Return the configured accept-loop connection limit.
    pub fn connection_limit(&self) -> ControlSocketConnectionLimit {
        self.max_connections.map_or(
            ControlSocketConnectionLimit::Unbounded,
            ControlSocketConnectionLimit::Bounded,
        )
    }
}

impl BrokerLaunchServeConfig {
    /// Build a launch-backed serve config using the platform
    /// service-definition directory.
    pub fn new(
        socket_path: impl Into<String>,
        max_connections: usize,
    ) -> Result<Self, BrokerServeError> {
        Ok(Self {
            socket_path: socket_path.into(),
            service_definition_dir: service_definition_dir(),
            max_connections: Some(
                NonZeroUsize::new(max_connections)
                    .ok_or(BrokerServeError::InvalidMaxConnections)?,
            ),
        })
    }

    /// Build an unbounded launch-backed serve config using the platform
    /// service-definition directory.
    pub fn unbounded(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            service_definition_dir: service_definition_dir(),
            max_connections: None,
        }
    }

    /// Override the service-definition directory.
    pub fn with_service_definition_dir(mut self, root: impl Into<PathBuf>) -> Self {
        self.service_definition_dir = root.into();
        self
    }

    /// Return the configured accept-loop connection limit.
    pub fn connection_limit(&self) -> ControlSocketConnectionLimit {
        self.max_connections.map_or(
            ControlSocketConnectionLimit::Unbounded,
            ControlSocketConnectionLimit::Bounded,
        )
    }
}

/// Serve a bounded number of broker Hello connections.
pub fn serve_registered_backend(config: BrokerServeConfig) -> Result<(), BrokerServeError> {
    let RegisteredServeBackend {
        loader,
        registry,
        instance,
        ..
    } = build_registered_backend(&config)?;
    let registry = Mutex::new(registry);
    let router = HelloRouter::with_lifecycle_monitor(&loader, &registry);
    let peer_policy =
        PeerCredentialPolicy::current_user().ok_or(BrokerServeError::PeerPolicyUnavailable)?;
    let started_at = Instant::now();
    let fd_guard = FdPressureGuard::default();
    let snapshot_provider = || {
        let registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let demoted = fd_guard.is_demoted();
        AdminSnapshot::from_registry(
            instance.id(),
            started_at.elapsed(),
            !demoted,
            0,
            &registry,
            &[],
        )
        .with_fd_pressure_demoted(demoted)
    };
    serve_control_socket_connections_with_limit_policy_post_hello_and_fd_guard(
        &config.socket_path,
        &router,
        snapshot_provider,
        config.connection_limit(),
        &peer_policy,
        |stream, reply| {
            // Off by default: no handoff endpoint means no handoff attempt.
            let Some(handoff_endpoint) = config.handoff_endpoint.as_deref() else {
                return;
            };
            let ctx = ServeHandoffContext {
                handoff_endpoint,
                service_name: &config.service_name,
                service_version: &config.service_version,
                instance: &instance,
                registry: &registry,
            };
            complete_negotiated_handoff(&ctx, stream, reply);
        },
        &fd_guard,
    )?;
    Ok(())
}

/// Serve a bounded number of broker Hello connections, launching backends on
/// verified registry misses.
pub fn serve_launching_backends(config: BrokerLaunchServeConfig) -> Result<(), BrokerServeError> {
    let launcher = CommandBackendLauncher::for_current_user()?;
    serve_launching_backends_with_launcher(config, &launcher)
}

/// Testable launch-backed serve mode with an injected launcher.
pub fn serve_launching_backends_with_launcher(
    config: BrokerLaunchServeConfig,
    launcher: &dyn BackendLauncher,
) -> Result<(), BrokerServeError> {
    let loader = ServiceDefinitionLoader::new(&config.service_definition_dir);
    let registry = Mutex::new(BackendRegistry::new());
    let spawn_coordinator = Mutex::new(SpawnCoordinator::new());
    let router = HelloRouter::with_lifecycle_monitor(&loader, &registry)
        .with_spawn_coordinator(&spawn_coordinator)
        .with_backend_launcher(launcher);
    let peer_policy =
        PeerCredentialPolicy::current_user().ok_or(BrokerServeError::PeerPolicyUnavailable)?;
    let started_at = Instant::now();
    let fd_guard = FdPressureGuard::default();
    let snapshot_provider = || {
        let registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let demoted = fd_guard.is_demoted();
        AdminSnapshot::from_registry("launch", started_at.elapsed(), !demoted, 0, &registry, &[])
            .with_fd_pressure_demoted(demoted)
    };
    serve_control_socket_connections_with_limit_policy_post_hello_and_fd_guard(
        &config.socket_path,
        &router,
        snapshot_provider,
        config.connection_limit(),
        &peer_policy,
        |_stream, _reply| {},
        &fd_guard,
    )?;
    Ok(())
}

/// Build a Hello handler from one service definition and backend endpoint.
pub fn build_hello_handler(config: &BrokerServeConfig) -> Result<HelloHandler, BrokerServeError> {
    let registered = build_registered_backend(config)?;
    let backend = registered
        .registry
        .registered_backend_for(
            &registered.instance,
            &registered.service_definition,
            &config.service_version,
        )
        .ok_or(BrokerServeError::RegisteredBackendMissing)?;

    Ok(HelloHandler::new().with_backend(backend)?)
}

struct RegisteredServeBackend {
    loader: ServiceDefinitionLoader,
    registry: BackendRegistry,
    instance: BrokerInstanceKey,
    service_definition: ServiceDefinition,
}

fn build_registered_backend(
    config: &BrokerServeConfig,
) -> Result<RegisteredServeBackend, BrokerServeError> {
    if config.backend_endpoint.is_empty() {
        return Err(BrokerServeError::EmptyBackendEndpoint);
    }

    let loader = ServiceDefinitionLoader::new(&config.service_definition_dir);
    let service_definition = loader.lookup_or_reload(&config.service_name)?;
    check_version_allowed(&config.service_version, &service_definition)
        .map_err(BrokerServeError::VersionPolicy)?;

    let instance = BrokerInstanceKey::from_service_definition(&service_definition)?;
    let endpoint = Endpoint {
        namespace_id: instance.id(),
        path: config.backend_endpoint.clone(),
    };
    let daemon = DaemonProcess::current_process(endpoint.clone(), Some(30))?;
    let handle = BackendHandle::probe_with_service(
        config.service_name.clone(),
        config.service_version.clone(),
        &endpoint,
        &daemon,
    )?;

    let mut registry = BackendRegistry::new();
    registry.insert(instance.clone(), handle);

    Ok(RegisteredServeBackend {
        loader,
        registry,
        instance,
        service_definition,
    })
}

/// Errors raised while wiring or serving the bounded broker.
#[derive(Debug, thiserror::Error)]
pub enum BrokerServeError {
    /// The connection bound must be non-zero.
    #[error("max_connections must be greater than zero")]
    InvalidMaxConnections,
    /// The configured backend endpoint is empty.
    #[error("backend endpoint must not be empty")]
    EmptyBackendEndpoint,
    /// Service-definition load or validation failed.
    #[error(transparent)]
    ServiceDefinition(#[from] ServiceDefinitionError),
    /// Service isolation could not be mapped to a broker instance.
    #[error(transparent)]
    BrokerInstance(#[from] BrokerInstanceError),
    /// Current process identity could not be recorded for the configured backend.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// Current user SID hash could not be computed for backend endpoint allocation.
    #[error(transparent)]
    Sid(#[from] SidError),
    /// Configured backend version is blocked by the service definition.
    #[error("configured service version is blocked by service-definition policy: {0:?}")]
    VersionPolicy(VersionPolicyBlock),
    /// Backend identity verification failed.
    #[error(transparent)]
    BackendHandle(#[from] BackendHandleError),
    /// Registry lookup failed after inserting the configured backend.
    #[error("registered backend was missing after registry insert")]
    RegisteredBackendMissing,
    /// Hello handler construction failed.
    #[error(transparent)]
    HelloHandler(#[from] HelloHandlerError),
    /// The platform current-user peer policy could not be constructed.
    #[error("current-user peer credential policy is unavailable")]
    PeerPolicyUnavailable,
    /// Local-socket serving failed.
    #[error(transparent)]
    Connection(#[from] BrokerConnectionError),
    /// Shared control-socket serving failed.
    #[error(transparent)]
    ControlSocket(#[from] ControlSocketError),
}
