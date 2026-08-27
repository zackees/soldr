//! Soldr-owned broker launcher.
//!
//! `running_process::CommandBackendLauncher` intentionally starts children
//! from a user-baseline environment. Soldr's root and identity live in the
//! process-local `SOLDR_*` namespace, so the broker must apply that namespace
//! explicitly when it creates `soldr-daemon`.

use running_process::broker::backend_handle::BackendHandle;
use running_process::broker::protocol::Endpoint;
use running_process::broker::server::{
    BackendKey, BackendLaunchError, BackendLaunchRequest, BackendLauncher, BrokerInstanceKey,
    CombinedServiceDefinitionLoader, TraceContext, BACKEND_ENV_ENDPOINT_NAMESPACE,
    BACKEND_ENV_ENDPOINT_PATH, BACKEND_ENV_INSTANCE, BACKEND_ENV_SERVICE_NAME,
    BACKEND_ENV_SERVICE_VERSION, BACKEND_ENV_TRACEPARENT, BACKEND_ENV_TRACESTATE,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

// A cold embedded-zccache startup can exceed the old 25-second budget on
// emulated macOS x86 runners. Keep this bounded, while leaving enough room for
// the daemon to publish its authenticated route claim.
const DAEMON_ROUTE_READINESS_TIMEOUT: Duration = Duration::from_secs(45);

pub(crate) struct SoldrBackendLauncher {
    placed_images: Mutex<HashMap<String, PlacedImage>>,
    route_deadlines: Mutex<HashMap<String, std::time::Instant>>,
    progress: tokio::sync::broadcast::Sender<LauncherProgress>,
}

#[derive(Clone, Debug)]
pub(crate) struct LauncherProgress {
    pub(crate) service_name: String,
    pub(crate) stage: &'static str,
    pub(crate) latest_result: String,
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
        let (progress, _) = tokio::sync::broadcast::channel(256);
        Self {
            placed_images: Mutex::new(HashMap::new()),
            route_deadlines: Mutex::new(HashMap::new()),
            progress,
        }
    }

    pub(crate) fn note_route_deadline(&self, service_name: &str, deadline: std::time::Instant) {
        let mut deadlines = self
            .route_deadlines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        deadlines
            .entry(service_name.to_string())
            .and_modify(|current| *current = (*current).max(deadline))
            .or_insert(deadline);
    }

    fn ensure_route_deadline(
        &self,
        request: &BackendLaunchRequest<'_>,
    ) -> Result<(), BackendLaunchError> {
        let deadline = self
            .route_deadlines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&request.key.service_name)
            .copied();
        if deadline.is_some_and(|value| std::time::Instant::now() >= value) {
            return Err(BackendLaunchError::Launcher(format!(
                "route {} expired before daemon launch completed",
                request.key.service_name
            )));
        }
        Ok(())
    }

    pub(crate) fn subscribe_progress(&self) -> tokio::sync::broadcast::Receiver<LauncherProgress> {
        self.progress.subscribe()
    }

    /// Rehydrate a requested control route without launching a missing daemon.
    /// This is the admin-path counterpart to the Hello launcher's claim-first
    /// behavior: a broker restart can answer status/stop for a surviving daemon,
    /// while a genuinely absent route stays absent.
    pub(crate) fn adopt_existing_control_route(
        &self,
        loader: &CombinedServiceDefinitionLoader,
        service_name: &str,
    ) -> Result<Option<(BrokerInstanceKey, BackendHandle)>, String> {
        let definition = loader
            .lookup_or_reload(service_name)
            .map_err(|error| error.to_string())?;
        let instance = BrokerInstanceKey::from_service_definition(&definition)
            .map_err(|error| error.to_string())?;
        let key = BackendKey::new(
            instance.clone(),
            service_name,
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION,
            "control-re-adoption",
        );
        let trace = TraceContext::default();
        let request = BackendLaunchRequest {
            key: &key,
            service_definition: &definition,
            trace_context: &trace,
            session_token: None,
        };
        let paths = routed_paths(&request).map_err(|error| error.to_string())?;
        // The persisted claim already carries the executable path and digest
        // that BackendHandle verifies. Re-adoption must fit inside a normal
        // control request's timeout, so do not re-hash and re-place a large
        // source image after every broker restart. Constrain the claimed path
        // to this route's broker-owned runtime before probing it.
        let claim = match crate::daemon::backend_handle_adoption::read_broker_route_claim(&paths)
            .map_err(|error| error.to_string())?
        {
            Some(claim) => claim,
            None => return Ok(None),
        };
        let claimed_binary = std::fs::canonicalize(&claim.exe_path).ok();
        let route_runtime = crate::self_relocate::daemon_runtime_root(&route_image_paths(&request));
        let route_runtime = std::fs::canonicalize(&route_runtime).unwrap_or(route_runtime);
        if claimed_binary
            .as_deref()
            .is_none_or(|path| !path.starts_with(&route_runtime))
        {
            // The claim is root-scoped, not route-scoped. A different daemon
            // image may legitimately own this root while its replacement is
            // being registered. Preserve that foreign claim so the lifecycle
            // preflight can identify and retire the incumbent.
            return Ok(None);
        }
        let claimed_binary = claimed_binary.expect("checked above");
        Ok(self
            .adopt_route_claim(&request, &paths, &claimed_binary, false)
            .map(|handle| (instance, handle)))
    }

    fn report_progress(
        &self,
        request: &BackendLaunchRequest<'_>,
        stage: &'static str,
        latest_result: impl Into<String>,
    ) {
        let _ = self.progress.send(LauncherProgress {
            service_name: request.key.service_name.clone(),
            stage,
            latest_result: latest_result.into(),
        });
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
        self.report_progress(
            request,
            "route-request",
            "broker accepted the route request",
        );
        self.ensure_route_deadline(request)?;
        let paths = routed_paths(request)?;
        let source_binary = canonical_backend_binary(request)?;
        self.report_progress(
            request,
            "image-source-resolved",
            format!("resolved daemon image {}", source_binary.display()),
        );
        // The broker is the sole owner of daemon placement.  Resolve the
        // request's allow-listed source image into this route's stable runtime
        // tree before spawning so the child PID registered below is the
        // long-lived daemon, never a short-lived self-relocation trampoline.
        // `ensure_daemon_relocated` serializes concurrent copies under the
        // route root; running-process serializes launch for the backend key.
        let binary_path = self.place_backend_image(request, &source_binary)?;
        self.ensure_route_deadline(request)?;
        self.report_progress(
            request,
            "image-verified",
            format!("verified daemon image {}", binary_path.display()),
        );
        if let Some(handle) = self.adopt_route_claim(request, &paths, &binary_path, true) {
            self.report_progress(
                request,
                "claim-adopted",
                format!("verified live daemon pid={}", handle.daemon_process.pid),
            );
            if debug {
                eprintln!(
                    "soldr broker: route re-adopted route={} pid={} elapsed={:?}",
                    request.key.service_name,
                    handle.daemon_process.pid,
                    started.elapsed()
                );
            }
            return Ok(handle);
        }
        let endpoint =
            crate::broker_identity::daemon_session_endpoint_from_executable(&binary_path)
                .map_err(|error| BackendLaunchError::Launcher(error.to_string()))?;
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
        let daemon_log_path = paths.root.join("daemon-spawn.log");
        let log = crate::broker_spawn::open_append(&daemon_log_path).ok_or_else(|| {
            BackendLaunchError::Launcher(format!(
                "could not open daemon log under {}",
                paths.root.display()
            ))
        })?;
        // spawn_blocking cannot cancel a running OS thread. Re-check the
        // original connection's hard deadline at the last possible point so
        // a timed-out route worker cannot materialize a daemon afterward.
        self.ensure_route_deadline(request)?;
        let mut child = running_process::spawn_daemon_with_stdio_and_env_policy(
            &mut command,
            crate::broker_spawn::daemon_stdio(&log),
            running_process::EnvironmentPolicy::UserBaseline,
        )
        .map_err(BackendLaunchError::Spawn)?;
        self.report_progress(
            request,
            "child-spawned",
            format!("spawned daemon child pid={}", child.id()),
        );
        if debug {
            eprintln!(
                "soldr broker: daemon spawned route={} elapsed={:?}",
                request.key.service_name,
                started.elapsed()
            );
        }

        self.report_progress(
            request,
            "readiness-probe",
            "daemon child is alive; waiting for exact endpoint and nonce verification",
        );

        match crate::daemon::backend_handle_adoption::wait_for_broker_backend_handle_while(
            &paths,
            &request.key.service_name,
            &request.key.service_version,
            &endpoint,
            DAEMON_ROUTE_READINESS_TIMEOUT,
            || child.try_wait(),
            |latest_result| {
                self.report_progress(request, "readiness-probe", latest_result.to_string());
            },
        ) {
            Ok(handle) => {
                // spawn_blocking cannot cancel a running OS thread, so a
                // timed-out route worker re-checks the connection deadline
                // here too: a daemon that became ready after the caller gave
                // up must not be published as this route's backend.
                if let Err(error) = self.ensure_route_deadline(request) {
                    let _ = child.kill();
                    return Err(error);
                }
                if debug {
                    eprintln!(
                        "soldr broker: route ready route={} elapsed={:?}",
                        request.key.service_name,
                        started.elapsed()
                    );
                }
                Ok(handle)
            }
            Err(err) => {
                let _ = child.kill();
                drop(log);
                let err = daemon_launch_failure(&err, &daemon_log_path);
                if debug {
                    eprintln!(
                        "soldr broker: route failed route={} elapsed={:?}: {err}",
                        request.key.service_name,
                        started.elapsed()
                    );
                }
                Err(BackendLaunchError::Launcher(err))
            }
        }
    }
}

fn daemon_launch_failure(error: &std::io::Error, log_path: &std::path::Path) -> String {
    let mut message = error.to_string();
    let Ok(log) = std::fs::read_to_string(log_path) else {
        return message;
    };
    let excerpt = log
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ");
    if !excerpt.is_empty() {
        message.push_str("; daemon startup log: ");
        message.push_str(&excerpt);
    }
    message
}

impl SoldrBackendLauncher {
    fn adopt_route_claim(
        &self,
        request: &BackendLaunchRequest<'_>,
        paths: &crate::core::SoldrPaths,
        expected_binary: &std::path::Path,
        prune_invalid: bool,
    ) -> Option<BackendHandle> {
        let claim = match crate::daemon::backend_handle_adoption::read_broker_route_claim(paths) {
            Ok(Some(claim)) => claim,
            Ok(None) => return None,
            Err(error) => {
                eprintln!(
                    "soldr broker: pruning unreadable daemon route claim {}: {error}",
                    crate::daemon::backend_handle_adoption::broker_route_claim_path(paths)
                        .display()
                );
                if prune_invalid {
                    crate::daemon::backend_handle_adoption::prune_broker_route_claim(paths);
                }
                return None;
            }
        };
        let expected_binary = std::fs::canonicalize(expected_binary).ok()?;
        let claimed_binary = std::fs::canonicalize(&claim.exe_path).ok();
        if claimed_binary.as_deref() != Some(expected_binary.as_path()) {
            if std::env::var_os("SOLDR_BROKER_DEBUG").is_some() {
                eprintln!(
                    "soldr broker: route claim identity mismatch claimed_binary={:?} expected_binary={} claim_boot={} current_boot={}",
                    claimed_binary,
                    expected_binary.display(),
                    claim.boot_id,
                    running_process::broker::host_identity::current().boot_id,
                );
            }
            // A root-scoped claim for another image route is not corrupt.
            // Deleting it here prevents preflight from naming the process
            // that owns the root lock during an image transition.
            return None;
        }
        if claim.boot_id != running_process::broker::host_identity::current().boot_id {
            if prune_invalid {
                crate::daemon::backend_handle_adoption::prune_broker_route_claim(paths);
            }
            return None;
        }
        match BackendHandle::probe_with_service(
            request.key.service_name.clone(),
            request.key.service_version.clone(),
            &claim.ipc_endpoint,
            &claim,
        ) {
            Ok(handle) => Some(handle),
            Err(error) => {
                if std::env::var_os("SOLDR_BROKER_DEBUG").is_some() {
                    eprintln!(
                        "soldr broker: daemon route claim failed exact probe and was pruned: {error}"
                    );
                }
                if prune_invalid {
                    crate::daemon::backend_handle_adoption::prune_broker_route_claim(paths);
                }
                None
            }
        }
    }

    fn place_backend_image(
        &self,
        request: &BackendLaunchRequest<'_>,
        source_binary: &std::path::Path,
    ) -> Result<PathBuf, BackendLaunchError> {
        let image_hash = request
            .service_definition
            .labels
            .get(crate::daemon::backend_handle_adoption::SOLDR_DAEMON_IMAGE_HASH_LABEL)
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| {
                BackendLaunchError::Launcher(
                    "soldr daemon service definition is missing a valid BLAKE3 image hash"
                        .to_string(),
                )
            })?;
        let cache_key = format!(
            "{}\0{}\0{image_hash}",
            request.key.service_name,
            source_binary.display()
        );
        let lock_started = std::time::Instant::now();
        let mut next_lock_progress = Duration::from_secs(1);
        let mut placed = loop {
            match self.placed_images.try_lock() {
                Ok(placed) => break placed,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(BackendLaunchError::AllocatorPoisoned)
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    let elapsed = lock_started.elapsed();
                    if elapsed >= next_lock_progress {
                        self.report_progress(
                            request,
                            "image-placement-wait",
                            format!(
                                "waiting for concurrent daemon image verification ({}ms)",
                                elapsed.as_millis()
                            ),
                        );
                        next_lock_progress += Duration::from_secs(1);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        };
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
        // soldr#2442 / 0.9.0: blake3 (via zccache's shared hasher) with a
        // (path,size,mtime) cache instead of a whole-file SHA-256 read, so a
        // warm image is not re-read on every launch. Matches the registration
        // side's algorithm so the label comparison below still holds.
        self.report_progress(
            request,
            "image-hash",
            format!(
                "verifying daemon image hash for {}",
                source_binary.display()
            ),
        );
        let mut last_hash_progress = std::time::Instant::now();
        let source_hash = crate::daemon::image_hash::cached_blake3_hex_with_progress(
            // soldr#2521 B1: machine-scoped memo so isolated roots share hits.
            &crate::daemon::image_hash::machine_scoped_cache_dir(
                &crate::daemon::service_definition::broker_owned_paths()
                    .cache
                    .join("image-hash"),
            ),
            source_binary,
            |completed, total| {
                if last_hash_progress.elapsed() >= Duration::from_millis(500) || completed >= total
                {
                    self.report_progress(
                        request,
                        "image-hash",
                        format!("daemon image hash: {completed}/{total} bytes"),
                    );
                    last_hash_progress = std::time::Instant::now();
                }
            },
        )
        .map_err(|err| {
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
        self.report_progress(
            request,
            "image-hash-verified",
            format!("verified daemon image hash for {}", source_binary.display()),
        );
        // Every route gets its own real executable path. The SESSION,
        // control, and handoff endpoint names are derived from that path, so
        // distinct soldr roots cannot collide even when they use the same
        // installed source image.
        let image_paths = route_image_paths(request);
        self.report_progress(
            request,
            "image-placement",
            "placing the verified daemon image in the broker-owned runtime",
        );
        let mut last_placement_progress = std::time::Instant::now();
        let path = crate::self_relocate::ensure_daemon_relocated_for_route_with_progress(
            &image_paths,
            source_binary,
            |operation, completed, total| {
                if last_placement_progress.elapsed() >= Duration::from_millis(500)
                    || completed >= total
                {
                    self.report_progress(
                        request,
                        "image-placement",
                        format!("daemon image {operation}: {completed}/{total} bytes"),
                    );
                    last_placement_progress = std::time::Instant::now();
                }
            },
        )
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
        // soldr#2442 / 0.9.0: the source was already hash-verified against the
        // label above, and a same-filesystem copy is byte-faithful, so a full
        // re-hash of the placed copy (a third whole-file read of a large binary
        // on the cold path) is redundant. Guard the copy with its (size, mtime)
        // fingerprint, which catches a truncated or replaced file.
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

fn route_image_paths(request: &BackendLaunchRequest<'_>) -> crate::core::SoldrPaths {
    let broker_paths = crate::daemon::service_definition::broker_owned_paths();
    crate::core::SoldrPaths::with_root(
        broker_paths
            .root
            .join("routes")
            .join(&request.key.service_name),
    )
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
    command.env(
        crate::daemon::session_endpoint::SOLDR_CONTROL_ENDPOINT_PATH_ENV,
        crate::daemon::session_endpoint::private_control_endpoint_from_session(&endpoint.path),
    );
    if !request.trace_context.traceparent.is_empty() {
        command.env(BACKEND_ENV_TRACEPARENT, &request.trace_context.traceparent);
    }
    if !request.trace_context.tracestate.is_empty() {
        command.env(BACKEND_ENV_TRACESTATE, &request.trace_context.tracestate);
    }
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
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_IMAGE_HASH_LABEL.into(),
            image_hash.into(),
        );
        let key = BackendKey::new(BrokerInstanceKey::Shared, "soldr-daemon-test", "1.0.0", "");
        (definition, key, TraceContext::default())
    }

    #[test]
    fn changed_registered_image_is_rejected_before_spawn() {
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
    }

    #[test]
    fn daemon_launch_failure_surfaces_the_startup_log() {
        let temp = tempfile::tempdir().expect("tempdir");
        let log = temp.path().join("daemon-spawn.log");
        std::fs::write(
            &log,
            "old launch\n\nsoldr-daemon failed: Io(Kind(Unsupported))\n",
        )
        .expect("write daemon log");
        let error = std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "broker-launched soldr-daemon exited before readiness (1)",
        );

        let message = daemon_launch_failure(&error, &log);

        assert!(message.contains("exited before readiness (1)"), "{message}");
        assert!(
            message.contains("soldr-daemon failed: Io(Kind(Unsupported))"),
            "{message}"
        );
    }

    #[test]
    fn foreign_live_route_claim_survives_replacement_preflight() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let paths = crate::core::SoldrPaths::with_root(root.clone());
        let endpoint = running_process::broker::protocol::Endpoint::unix_socket(
            "incumbent-test",
            temp.path().join("incumbent.sock").display().to_string(),
        )
        .or_else(|_| {
            running_process::broker::protocol::Endpoint::windows_pipe(
                "incumbent-test",
                "soldr-incumbent-test",
            )
        })
        .expect("incumbent endpoint");
        let incumbent =
            running_process::broker::backend_handle::DaemonProcess::current_process(endpoint, None)
                .expect("incumbent claim");
        crate::daemon::backend_handle_adoption::publish_broker_route_claim(&paths, &incumbent)
            .expect("publish incumbent claim");

        let replacement = temp.path().join("replacement").join("soldr-daemon");
        std::fs::create_dir_all(replacement.parent().expect("replacement parent"))
            .expect("replacement dir");
        std::fs::write(&replacement, b"replacement image").expect("replacement image");
        let (definition, key, trace_context) = request_parts(&root, &replacement, &"0".repeat(64));
        let request = BackendLaunchRequest {
            key: &key,
            service_definition: &definition,
            trace_context: &trace_context,
            session_token: None,
        };

        assert!(
            SoldrBackendLauncher::new()
                .adopt_route_claim(&request, &paths, &replacement, true)
                .is_none(),
            "a foreign image claim must not be adopted"
        );
        assert!(
            crate::daemon::backend_handle_adoption::broker_route_claim_path(&paths).exists(),
            "replacement preflight still needs the incumbent PID/image claim"
        );
    }
}
