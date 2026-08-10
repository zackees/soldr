//! Soldr-owned broker launcher.
//!
//! `running_process::CommandBackendLauncher` intentionally starts children
//! from a user-baseline environment. Soldr's root and identity live in the
//! process-local `SOLDR_*` namespace, so the broker must apply that namespace
//! explicitly when it creates `soldr-daemon`.

use running_process::broker::backend_handle::{BackendHandle, DaemonProcess};
use running_process::broker::backend_lifecycle::identity::sha256_file;
use running_process::broker::protocol::Endpoint;
use running_process::broker::server::{
    BackendEndpointAllocator, BackendLaunchError, BackendLaunchRequest, BackendLauncher,
    BACKEND_ENV_ENDPOINT_NAMESPACE, BACKEND_ENV_ENDPOINT_PATH, BACKEND_ENV_INSTANCE,
    BACKEND_ENV_SERVICE_NAME, BACKEND_ENV_SERVICE_VERSION, BACKEND_ENV_TRACEPARENT,
    BACKEND_ENV_TRACESTATE,
};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const BACKEND_ENV_SESSION_TOKEN: &str = "RUNNING_PROCESS_BROKER_V1_SESSION_TOKEN";

pub(crate) struct SoldrBackendLauncher {
    user_sid_hash: String,
    allocators: Mutex<HashMap<String, BackendEndpointAllocator>>,
    soldr_env: Vec<(OsString, OsString)>,
    paths: crate::core::SoldrPaths,
}

impl SoldrBackendLauncher {
    pub(crate) fn new(
        soldr_env: Vec<(OsString, OsString)>,
        paths: crate::core::SoldrPaths,
    ) -> Self {
        Self {
            user_sid_hash: crate::broker_identity::resolve_user_sid(),
            allocators: Mutex::new(HashMap::new()),
            soldr_env,
            paths,
        }
    }

    fn allocate_endpoint(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<Endpoint, BackendLaunchError> {
        let namespace_id = request.key.instance.id();
        let mut allocators = self
            .allocators
            .lock()
            .map_err(|_| BackendLaunchError::AllocatorPoisoned)?;
        Ok(allocators
            .entry(namespace_id.clone())
            .or_insert_with(|| BackendEndpointAllocator::new(&self.user_sid_hash, namespace_id))
            .allocate()?)
    }
}

impl BackendLauncher for SoldrBackendLauncher {
    fn launch(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<BackendHandle, BackendLaunchError> {
        if crate::daemon::tombstone::is_active(&self.paths) {
            return Err(tombstone_error());
        }

        let endpoint = self.allocate_endpoint(request)?;
        let binary_path = canonical_backend_binary(request)?;
        let mut command = std::process::Command::new(&binary_path);
        command.envs(self.soldr_env.iter().cloned());
        configure_backend_command(&mut command, request, &endpoint);
        let mut child = running_process::spawn_daemon(&mut command)
            .map_err(BackendLaunchError::Spawn)?;

        // Close the stop-vs-launch window: if stop planted its tombstone after
        // the first check, do not allow this child to become ready.
        if crate::daemon::tombstone::is_active(&self.paths) {
            let _ = child.kill();
            return Err(tombstone_error());
        }

        let daemon = DaemonProcess {
            pid: child.id(),
            exe_path: binary_path.clone(),
            exe_sha256: sha256_file(&binary_path).map_err(|source| {
                BackendLaunchError::Identity(
                    running_process::broker::backend_lifecycle::identity::IdentityError::Io(source),
                )
            })?,
            boot_id: running_process::broker::host_identity::current_for_path(&binary_path).boot_id,
            ipc_endpoint: endpoint.clone(),
            started_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            idle_timeout_secs: Some(30),
        };

        match BackendHandle::probe_with_service(
            request.key.service_name.clone(),
            request.key.service_version.clone(),
            &endpoint,
            &daemon,
        ) {
            Ok(handle) if !crate::daemon::tombstone::is_active(&self.paths) => Ok(handle),
            Ok(_) => {
                let _ = child.kill();
                Err(tombstone_error())
            }
            Err(err) => {
                let _ = child.kill();
                Err(BackendLaunchError::BackendHandle(err))
            }
        }
    }
}

fn tombstone_error() -> BackendLaunchError {
    BackendLaunchError::Launcher(
        "soldr daemon tombstone active; implicit broker launch suppressed".to_string(),
    )
}

fn canonical_backend_binary(
    request: &BackendLaunchRequest<'_>,
) -> Result<PathBuf, BackendLaunchError> {
    let definition = request.service_definition;
    if definition.binary_path.is_empty() {
        return Err(BackendLaunchError::EmptyBinaryPath);
    }
    if definition.per_version_binary_dir.is_empty() {
        return Err(BackendLaunchError::EmptyPerVersionBinaryDir);
    }
    let path = PathBuf::from(&definition.binary_path);
    let binary = std::fs::canonicalize(&path).map_err(|source| {
        BackendLaunchError::CanonicalizeBinary { path, source }
    })?;
    let path = PathBuf::from(&definition.per_version_binary_dir);
    let root = std::fs::canonicalize(&path).map_err(|source| {
        BackendLaunchError::CanonicalizeBinaryRoot { path, source }
    })?;
    if !binary.starts_with(&root) {
        return Err(BackendLaunchError::BinaryOutsideAllowRoot { binary, root });
    }
    Ok(binary)
}

fn configure_backend_command(
    command: &mut std::process::Command,
    request: &BackendLaunchRequest<'_>,
    endpoint: &Endpoint,
) {
    command
        .env(BACKEND_ENV_SERVICE_NAME, &request.key.service_name)
        .env(BACKEND_ENV_SERVICE_VERSION, &request.key.service_version)
        .env(BACKEND_ENV_ENDPOINT_PATH, &endpoint.path)
        .env(BACKEND_ENV_ENDPOINT_NAMESPACE, &endpoint.namespace_id)
        .env(BACKEND_ENV_INSTANCE, request.key.instance.id());
    if !request.trace_context.traceparent.is_empty() {
        command.env(BACKEND_ENV_TRACEPARENT, &request.trace_context.traceparent);
    }
    if !request.trace_context.tracestate.is_empty() {
        command.env(BACKEND_ENV_TRACESTATE, &request.trace_context.tracestate);
    }
    if let Some(token) = request.session_token {
        command.env(BACKEND_ENV_SESSION_TOKEN, hex_encode(token));
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
