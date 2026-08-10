//! Soldr-owned broker launcher.
//!
//! `running_process::CommandBackendLauncher` intentionally starts children
//! from a user-baseline environment. Soldr's root and identity live in the
//! process-local `SOLDR_*` namespace, so the broker must apply that namespace
//! explicitly when it creates `soldr-daemon`.

use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::protocol::Endpoint;
use running_process::broker::server::{
    BackendEndpointAllocator, BackendLaunchError, BackendLaunchRequest, BackendLauncher,
    BACKEND_ENV_ENDPOINT_NAMESPACE, BACKEND_ENV_ENDPOINT_PATH, BACKEND_ENV_INSTANCE,
    BACKEND_ENV_SERVICE_NAME, BACKEND_ENV_SERVICE_VERSION, BACKEND_ENV_TRACEPARENT,
    BACKEND_ENV_TRACESTATE,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const BACKEND_ENV_SESSION_TOKEN: &str = "RUNNING_PROCESS_BROKER_V1_SESSION_TOKEN";

pub(crate) struct SoldrBackendLauncher {
    user_sid_hash: String,
    allocators: Mutex<HashMap<String, BackendEndpointAllocator>>,
    placed_images: Mutex<HashMap<String, PlacedImage>>,
}

struct PlacedImage {
    path: PathBuf,
    source_fingerprint: FileFingerprint,
    placed_fingerprint: FileFingerprint,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified_nanos: u128,
}

impl SoldrBackendLauncher {
    pub(crate) fn new() -> Self {
        Self {
            user_sid_hash: crate::broker_identity::resolve_user_sid(),
            allocators: Mutex::new(HashMap::new()),
            placed_images: Mutex::new(HashMap::new()),
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
        let started = std::time::Instant::now();
        let debug = std::env::var_os("SOLDR_BROKER_DEBUG").is_some();
        if debug {
            eprintln!(
                "soldr broker: launch begin route={}",
                request.key.service_name
            );
        }
        let paths = routed_paths(request)?;
        if crate::daemon::tombstone::is_active(&paths) {
            return Err(tombstone_error());
        }

        let endpoint = self.allocate_endpoint(request)?;
        let source_binary = canonical_backend_binary(request)?;
        // The broker is the sole owner of daemon placement.  Resolve the
        // request's allow-listed source image into this route's stable runtime
        // tree before spawning so the child PID registered below is the
        // long-lived daemon, never a short-lived self-relocation trampoline.
        // `ensure_daemon_relocated` serializes concurrent copies under the
        // route root; running-process serializes launch for the backend key.
        let binary_path = self.place_backend_image(request, &source_binary)?;
        if debug {
            eprintln!(
                "soldr broker: image ready route={} elapsed={:?}",
                request.key.service_name,
                started.elapsed()
            );
        }
        let mut command = std::process::Command::new(&binary_path);
        let daemon_env = crate::daemon::service_definition::daemon_env_from_labels(
            &request.service_definition.labels,
        )
        .map_err(|err| BackendLaunchError::Launcher(err.to_string()))?;
        command.envs(daemon_env);
        command.env(crate::core::SOLDR_CACHE_DIR_ENV_VAR, &paths.root);
        configure_backend_command(&mut command, request, &endpoint);
        let log = crate::broker_spawn::open_append(&paths.root.join("daemon-spawn.log"))
            .ok_or_else(|| {
                BackendLaunchError::Launcher(format!(
                    "could not open daemon log under {}",
                    paths.root.display()
                ))
            })?;
        let mut child = running_process::spawn_daemon_with_stdio_and_env_policy(
            &mut command,
            crate::broker_spawn::daemon_stdio(&log),
            running_process::EnvironmentPolicy::UserBaseline,
        )
        .map_err(BackendLaunchError::Spawn)?;
        if debug {
            eprintln!(
                "soldr broker: daemon spawned route={} elapsed={:?}",
                request.key.service_name,
                started.elapsed()
            );
        }

        // Close the stop-vs-launch window: if stop planted its tombstone after
        // the first check, do not allow this child to become ready.
        if crate::daemon::tombstone::is_active(&paths) {
            let _ = child.kill();
            return Err(tombstone_error());
        }

        match crate::daemon::backend_handle_adoption::wait_for_broker_backend_handle_while(
            &paths,
            &request.key.service_name,
            &request.key.service_version,
            &endpoint,
            Duration::from_secs(25),
            || child.try_wait(),
        ) {
            Ok(handle) if !crate::daemon::tombstone::is_active(&paths) => {
                if debug {
                    eprintln!(
                        "soldr broker: route ready route={} elapsed={:?}",
                        request.key.service_name,
                        started.elapsed()
                    );
                }
                Ok(handle)
            }
            Ok(_) => {
                let _ = child.kill();
                Err(tombstone_error())
            }
            Err(err) => {
                let _ = child.kill();
                Err(BackendLaunchError::Launcher(err.to_string()))
            }
        }
    }
}

impl SoldrBackendLauncher {
    fn place_backend_image(
        &self,
        request: &BackendLaunchRequest<'_>,
        source_binary: &std::path::Path,
    ) -> Result<PathBuf, BackendLaunchError> {
        let image_hash = request
            .service_definition
            .labels
            .get(crate::daemon::backend_handle_adoption::SOLDR_DAEMON_IMAGE_SHA256_LABEL)
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| {
                BackendLaunchError::Launcher(
                    "soldr daemon service definition is missing a valid image SHA-256".to_string(),
                )
            })?;
        let cache_key = format!("{}\0{image_hash}", source_binary.display());
        let mut placed = self
            .placed_images
            .lock()
            .map_err(|_| BackendLaunchError::AllocatorPoisoned)?;
        let source_fingerprint = file_fingerprint(source_binary).map_err(|err| {
            BackendLaunchError::Launcher(format!(
                "could not inspect registered soldr-daemon image {}: {err}",
                source_binary.display()
            ))
        })?;
        if let Some(image) = placed.get(&cache_key) {
            let placed_unchanged = file_fingerprint(&image.path)
                .is_ok_and(|fingerprint| fingerprint == image.placed_fingerprint);
            if image.source_fingerprint == source_fingerprint && placed_unchanged {
                return Ok(image.path.clone());
            }
            placed.remove(&cache_key);
        }
        let source_hash = sha256_hex(source_binary).map_err(|err| {
            BackendLaunchError::Launcher(format!(
                "could not hash registered soldr-daemon image {}: {err}",
                source_binary.display()
            ))
        })?;
        if !source_hash.eq_ignore_ascii_case(image_hash) {
            return Err(BackendLaunchError::Launcher(format!(
                "registered soldr-daemon image changed before broker launch: expected {image_hash}, got {source_hash} for {}",
                source_binary.display()
            )));
        }
        if file_fingerprint(source_binary).ok() != Some(source_fingerprint) {
            return Err(BackendLaunchError::Launcher(format!(
                "registered soldr-daemon image changed while it was being verified: {}",
                source_binary.display()
            )));
        }
        let image_paths = crate::daemon::service_definition::broker_owned_paths();
        let path = crate::self_relocate::ensure_daemon_relocated(&image_paths, source_binary)
            .map_err(|err| {
                BackendLaunchError::Launcher(format!(
                    "could not place soldr-daemon image {} for route {}: {err}",
                    source_binary.display(),
                    request.key.service_name
                ))
            })?;
        let placed_fingerprint = file_fingerprint(&path).map_err(|err| {
            BackendLaunchError::Launcher(format!(
                "could not inspect broker-owned soldr-daemon image {}: {err}",
                path.display()
            ))
        })?;
        let placed_hash = sha256_hex(&path).map_err(|err| {
            BackendLaunchError::Launcher(format!(
                "could not verify broker-owned soldr-daemon image {}: {err}",
                path.display()
            ))
        })?;
        if !placed_hash.eq_ignore_ascii_case(image_hash) {
            return Err(BackendLaunchError::Launcher(format!(
                "broker-owned soldr-daemon image failed verification: expected {image_hash}, got {placed_hash} for {}",
                path.display()
            )));
        }
        if file_fingerprint(&path).ok() != Some(placed_fingerprint) {
            return Err(BackendLaunchError::Launcher(format!(
                "broker-owned soldr-daemon image changed while it was being verified: {}",
                path.display()
            )));
        }
        placed.insert(
            cache_key,
            PlacedImage {
                path: path.clone(),
                source_fingerprint,
                placed_fingerprint,
            },
        );
        Ok(path)
    }
}

fn sha256_hex(path: &std::path::Path) -> std::io::Result<String> {
    Ok(hex::encode(Sha256::digest(std::fs::read(path)?)))
}

fn file_fingerprint(path: &std::path::Path) -> std::io::Result<FileFingerprint> {
    use std::time::UNIX_EPOCH;

    let metadata = std::fs::metadata(path)?;
    let modified_nanos = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    Ok(FileFingerprint {
        len: metadata.len(),
        modified_nanos,
    })
}

fn routed_paths(
    request: &BackendLaunchRequest<'_>,
) -> Result<crate::core::SoldrPaths, BackendLaunchError> {
    let root = request
        .service_definition
        .labels
        .get(crate::daemon::service_definition::SOLDR_ROOT_SERVICE_LABEL)
        .filter(|root| !root.is_empty())
        .ok_or_else(|| {
            BackendLaunchError::Launcher(
                "soldr daemon service definition is missing its soldr-root route".to_string(),
            )
        })?;
    Ok(crate::core::SoldrPaths::with_root(PathBuf::from(root)))
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
    let binary = std::fs::canonicalize(&path)
        .map_err(|source| BackendLaunchError::CanonicalizeBinary { path, source })?;
    let path = PathBuf::from(&definition.per_version_binary_dir);
    let root = std::fs::canonicalize(&path)
        .map_err(|source| BackendLaunchError::CanonicalizeBinaryRoot { path, source })?;
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
    command.env(
        crate::daemon::session_endpoint::SOLDR_SESSION_ENDPOINT_PATH_ENV,
        &endpoint.path,
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use running_process::broker::protocol::ServiceDefinition;
    use running_process::broker::protocol_v2::ServiceDefinitionBuilder;
    use running_process::broker::server::service_definition_v2_to_v1;
    use running_process::broker::server::{BackendKey, BrokerInstanceKey, TraceContext};

    fn request_parts(
        root: &std::path::Path,
        binary: &std::path::Path,
        image_hash: &str,
    ) -> (ServiceDefinition, BackendKey, TraceContext) {
        let mut definition = service_definition_v2_to_v1(
            ServiceDefinitionBuilder::shared_broker(
                "soldr-daemon-test",
                binary.display().to_string(),
            )
            .per_version_binary_dir(
                binary
                    .parent()
                    .expect("binary parent")
                    .display()
                    .to_string(),
            )
            .min_version("1.0.0")
            .version_allow_list(["1.0.0"])
            .build(),
        );
        definition.labels.insert(
            crate::daemon::service_definition::SOLDR_ROOT_SERVICE_LABEL.into(),
            root.display().to_string(),
        );
        definition.labels.insert(
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_IMAGE_SHA256_LABEL.into(),
            image_hash.into(),
        );
        let key = BackendKey::new(BrokerInstanceKey::Shared, "soldr-daemon-test", "1.0.0", "");
        (definition, key, TraceContext::default())
    }

    crate::timed_test!(active_tombstone_refuses_before_image_or_spawn_work, {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let paths = crate::core::SoldrPaths::with_root(root.clone());
        crate::daemon::tombstone::plant(&paths, Duration::from_secs(30));
        let missing = temp.path().join("missing-daemon");
        let (definition, key, trace_context) = request_parts(&root, &missing, &"0".repeat(64));
        let request = BackendLaunchRequest {
            key: &key,
            service_definition: &definition,
            trace_context: &trace_context,
            session_token: None,
        };

        let err = match SoldrBackendLauncher::new().launch(&request) {
            Ok(_) => panic!("tombstone must suppress launch"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("tombstone active"), "{err}");
    });

    crate::timed_test!(changed_registered_image_is_rejected_before_spawn, {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("soldr-daemon");
        std::fs::write(&binary, b"changed image").expect("write image");
        let root = temp.path().join("root");
        let (definition, key, trace_context) = request_parts(&root, &binary, &"0".repeat(64));
        let request = BackendLaunchRequest {
            key: &key,
            service_definition: &definition,
            trace_context: &trace_context,
            session_token: None,
        };

        let err = match SoldrBackendLauncher::new().launch(&request) {
            Ok(_) => panic!("stale image registration must fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("changed before broker launch"),
            "{err}"
        );
    });
}
