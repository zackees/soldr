//! soldr#2361 Phase 2: the front door's "spawn the broker" allowlisted
//! exception. **Unconditional** (soldr#2388): every eligible top-level `soldr`
//! invocation spawns/confirms the broker, and the compile hot path routes
//! through it. There is no env-var opt-out — the broker-fronted daemon is the
//! only supported topology.
//!
//! Per the #2364 design, the front door is the sole broker-spawner, and the
//! broker is the sole daemon-spawner via `serve_launching_backends`. The
//! broker→daemon→SESSION compile path is proven end-to-end on the real-process
//! integration harness (`session_multiprocess_smoke`).
//!
//! The one invariant that must never regress: a `RUSTC_WRAPPER` re-entry
//! (`soldr /path/to/rustc ...`, cargo calling back into soldr once per
//! compile unit) must NEVER reach this code. That path is `run_main`'s
//! `wrapper::is_wrapper_invocation` branch, which returns before this
//! module's entry point is ever called -- see the call site in
//! `soldr_main.rs`. If a broker spawn attempt fired on every compile unit
//! instead of once at the top-level invocation, it would recreate the
//! spawn-storm this whole redesign exists to kill (soldr#2360: "154x root
//! ownership is busy"). [`front_door_broker_spawn_eligible`] is a pure,
//! directly-unit-testable predicate for exactly this reason -- the call site
//! placement is a second, structural line of defense, not the only one.
//!
//! Spawns via `running_process::spawn_daemon_with_stdio_and_env_policy`
//! (the same detach machinery `soldr-daemon`'s own client-spawns-daemon path
//! uses, see `soldr_daemon::daemon::lifecycle::spawn::spawn_detached`) rather
//! than a bare `std::process::Command::spawn()`. A bare spawn on Windows
//! stays attached to the caller's job object / console, so a shell (or a
//! sandboxed tool harness) that waits for the whole descendant tree to exit
//! hangs on the long-lived broker even though the direct child (this `soldr`
//! invocation) has already returned -- caught by hand while smoke-testing
//! this against the real binary, not by any written test.

use running_process::{DaemonStdio, DaemonStdioSource, EnvironmentPolicy};
use std::time::{Duration, Instant};

/// How long the front door actively waits for a freshly-spawned broker's
/// control listener. Bounded so a wedged or slow-starting broker can never
/// turn an ordinary `soldr` invocation into a hang.
const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const RESURRECTION_WAIT_TIMEOUT: Duration = Duration::from_secs(12);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Whether the broker path is enabled — **always true** (soldr#2388). The
/// broker-fronted daemon is the only supported topology: there is no env-var
/// opt-out and no legacy "direct client → daemon without a broker" mode to
/// select. Kept as a named predicate so the call sites read intentionally
/// rather than hard-coding `true`.
pub(crate) fn broker_enabled() -> bool {
    true
}

/// Preserve Soldr's complete identity namespace plus the authoritative
/// endpoint resolver inputs across the detached front-door broker spawn.
/// `UserBaseline` intentionally removes process-local variables; without this
/// overlay the child can resolve a different home/runtime fallback than the
/// elected starter.
pub(crate) fn broker_spawn_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    filter_broker_spawn_env(std::env::vars_os())
}

fn filter_broker_spawn_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vars.into_iter()
        .filter(|(name, _)| {
            let name = name.to_string_lossy().to_ascii_uppercase();
            name.starts_with("SOLDR_")
                || matches!(
                    name.as_str(),
                    "HOME" | "USERPROFILE" | "LOCALAPPDATA" | "XDG_RUNTIME_DIR" | "TMPDIR"
                )
        })
        .collect()
}

/// Pure predicate: should this top-level invocation attempt to spawn the
/// broker as its allowlisted exception? `raw_args` is the full argv
/// (`raw_args[0]` is the program name), matching `run_main`'s shape.
///
/// Kept separate from the actual spawn so the wrapper-exclusion and
/// self-recursion-exclusion rules are unit-testable without spawning a
/// process.
pub(crate) fn front_door_broker_spawn_eligible(raw_args: &[String]) -> bool {
    if !broker_enabled() {
        return false;
    }
    let Some(first_positional) = raw_args.get(1) else {
        return false;
    };
    // A rustc-wrapper re-entry must never reach here -- see module doc.
    // Belt-and-suspenders: `run_main` already returns before calling this
    // module on that path, but the predicate stays correct standalone.
    if crate::wrapper::is_wrapper_invocation(first_positional) {
        return false;
    }
    // Don't spawn a broker to service a direct `soldr broker ...`
    // invocation -- that command IS the broker; spawning another one to
    // watch it start would just race its own singleton bind pointlessly.
    if first_positional == "broker" {
        return false;
    }
    true
}

fn ci_endpoint_diagnostics_eligible(raw_args: &[String]) -> bool {
    !raw_args.iter().any(|arg| {
        matches!(arg.as_str(), "--json" | "--github-env" | "--shell-export")
            || arg.starts_with("--github-env=")
    })
}

/// Best-effort: confirm an existing broker or spawn one detached, then wait
/// for an active connection to its control socket. Log text is deliberately
/// not synchronization: it can be stale in the append-only spawn log and is
/// printed before the control socket is usable. A broker Hello is deliberately
/// not used either: on a registry miss, Hello is launch-capable and a mere
/// readiness check must never resurrect the daemon.
///
/// Never fails a non-compiling caller merely because eager broker startup did
/// not finish. A later cacheable compile uses the mandatory broker route and
/// reports an attributed hard failure if that route is still unavailable.
pub(crate) fn maybe_spawn_broker_front_door(raw_args: &[String]) {
    if !front_door_broker_spawn_eligible(raw_args) {
        return;
    }
    if ci_endpoint_diagnostics_eligible(raw_args) {
        emit_ci_endpoint_diagnostics();
    }
    if let Err(error) = ensure_stable_broker_ready() {
        eprintln!("soldr: broker resurrection did not complete: {error}");
    }
}

fn ensure_stable_broker_ready() -> Result<(), String> {
    let endpoint = crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .map_err(|error| error.to_string())?;
    endpoint
        .create_owner_only_directories()
        .map_err(|error| error.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("could not create readiness runtime: {error}"))?;
    if stable_broker_is_ready(&runtime, &endpoint.bind_endpoint) {
        return Ok(());
    }

    let resurrection_deadline = Instant::now() + RESURRECTION_WAIT_TIMEOUT;
    let lease = loop {
        match crate::broker_lease::BrokerLease::acquire(&endpoint.lease_database_path) {
            Ok(lease) => break lease,
            Err(crate::broker_lease::BrokerLeaseError::Fenced) => {
                if stable_broker_is_ready(&runtime, &endpoint.bind_endpoint) {
                    return Ok(());
                }
                if Instant::now() >= resurrection_deadline {
                    return Err(format!(
                        "broker resurrection lease expired without readiness at {}",
                        endpoint.bind_endpoint
                    ));
                }
                // A dead or PID-reused owner is reclaimable immediately. A
                // live/SIGSTOP'd owner remains fenced until expiry.
                std::thread::sleep(crate::broker_lease::BrokerLease::contention_delay());
            }
            Err(error) => return Err(error.to_string()),
        }
    };

    #[cfg(debug_assertions)]
    if !test_pause_after_lease_acquired(&lease)? {
        lease.release();
        return Ok(());
    }

    let result = (|| {
        // A winner may have become ready while this process was entering the
        // immediate transaction. Never stage or spawn after that re-probe.
        if stable_broker_is_ready(&runtime, &endpoint.bind_endpoint) {
            return Ok(());
        }
        if let Some(instance) = broker_instance_at(&runtime, &endpoint.bind_endpoint) {
            stop_incompatible_broker(&runtime, &endpoint.bind_endpoint, &instance)?;
        }
        lease.check_fence().map_err(|error| error.to_string())?;
        stage_broker_image(&endpoint.executable_path, &lease).map_err(|error| {
            format!(
                "could not stage stable broker image {}: {error}",
                endpoint.executable_path.display()
            )
        })?;
        lease.renew().map_err(|error| error.to_string())?;
        lease.check_fence().map_err(|error| error.to_string())?;

        let log_file = open_append(
            &endpoint
                .executable_path
                .parent()
                .expect("broker executable has a parent")
                .join("broker-spawn.log"),
        )
        .ok_or_else(|| "could not open stable broker spawn log".to_string())?;
        let mut command = std::process::Command::new(&endpoint.executable_path);
        command.args(["broker", "serve"]);
        command.envs(broker_spawn_env());
        let stdio = daemon_stdio(&log_file);
        let child = running_process::spawn_daemon_with_stdio_and_env_policy(
            &mut command,
            stdio,
            EnvironmentPolicy::UserBaseline,
        )
        .map_err(|error| format!("could not spawn stable broker: {error}"))?;
        wait_for_stable_broker(&runtime, &endpoint.bind_endpoint, Some((&lease, child)))
    })();
    lease.release();
    result
}

#[cfg(debug_assertions)]
fn test_pause_after_lease_acquired(
    lease: &crate::broker_lease::BrokerLease,
) -> Result<bool, String> {
    let Some(milliseconds) = std::env::var("SOLDR_TEST_BROKER_LEASE_PAUSE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
    else {
        return Ok(true);
    };
    if let Some(path) = std::env::var_os("SOLDR_TEST_BROKER_LEASE_READY_FILE") {
        let _ = std::fs::write(path, b"acquired\n");
    }
    // Sleep in short slices so a SIGSTOP'd test owner notices wall-clock
    // lease expiry promptly after SIGCONT. One long nanosleep may be restarted
    // with its original remainder by the platform, delaying the fence check
    // for the full injected pause after the process resumes.
    let deadline = Instant::now() + Duration::from_millis(milliseconds);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    match lease.check_fence() {
        Ok(()) => Ok(true),
        Err(crate::broker_lease::BrokerLeaseError::Fenced) => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn wait_for_stable_broker(
    runtime: &tokio::runtime::Runtime,
    endpoint: &str,
    mut owned: Option<(
        &crate::broker_lease::BrokerLease,
        running_process::DaemonChild,
    )>,
) -> Result<(), String> {
    let deadline = Instant::now() + SPAWN_WAIT_TIMEOUT;
    let mut next_renew = Instant::now() + Duration::from_secs(1);
    loop {
        if stable_broker_is_ready(runtime, endpoint) {
            return Ok(());
        }
        if let Some((lease, child)) = owned.as_mut() {
            if child
                .try_wait()
                .map_err(|error| format!("could not inspect broker child: {error}"))?
                .is_some()
            {
                return Err("stable broker exited before readiness".into());
            }
            if Instant::now() >= next_renew {
                lease.renew().map_err(|error| error.to_string())?;
                next_renew = Instant::now() + Duration::from_secs(1);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "stable broker did not answer readiness at {endpoint} within {}ms",
                SPAWN_WAIT_TIMEOUT.as_millis()
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn stable_broker_is_ready(runtime: &tokio::runtime::Runtime, endpoint: &str) -> bool {
    broker_instance_at(runtime, endpoint)
        .is_some_and(|instance| instance == crate::broker_server::BROKER_INSTANCE_ID)
}

fn broker_instance_at(runtime: &tokio::runtime::Runtime, endpoint: &str) -> Option<String> {
    runtime.block_on(async {
        use interprocess::local_socket::tokio::prelude::*;
        use prost::Message as _;
        use running_process::broker::protocol::{
            validate_frame_envelope, AdminReply, AdminRequest, AdminVerb, Frame, FrameKind,
            ADMIN_PAYLOAD_PROTOCOL,
        };
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let probe = async {
            let name = crate::session_transport::local_session_name(endpoint)?;
            let mut stream = interprocess::local_socket::tokio::Stream::connect(name).await?;
            let request = AdminRequest {
                verb: AdminVerb::Status as i32,
                json: true,
                ..Default::default()
            };
            let request =
                Frame::request(ADMIN_PAYLOAD_PROTOCOL, request.encode_to_vec()).with_request_id(1);
            let bytes = running_process::broker::protocol::encode_framed(&request)
                .map_err(std::io::Error::other)?;
            stream.write_all(&bytes).await?;
            stream.flush().await?;
            let mut header = [0_u8; 5];
            stream.read_exact(&mut header).await?;
            if header[0] != running_process::broker::protocol::ENVELOPE_VERSION {
                return Err(std::io::Error::other("wrong broker framing version"));
            }
            let len = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
            if len > running_process::broker::protocol::MAX_FRAME_BYTES {
                return Err(std::io::Error::other("oversized readiness reply"));
            }
            let mut body = vec![0_u8; len];
            stream.read_exact(&mut body).await?;
            let response = Frame::decode(body.as_slice()).map_err(std::io::Error::other)?;
            validate_frame_envelope(&response, FrameKind::Response, ADMIN_PAYLOAD_PROTOCOL)
                .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
            if response.request_id != request.request_id {
                return Err(std::io::Error::other("readiness request id mismatch"));
            }
            let reply =
                AdminReply::decode(response.payload.as_slice()).map_err(std::io::Error::other)?;
            if reply.exit_code != 0 {
                return Err(std::io::Error::other(format!(
                    "broker readiness returned exit code {}",
                    reply.exit_code
                )));
            }
            let snapshot: serde_json::Value =
                serde_json::from_str(&reply.body).map_err(std::io::Error::other)?;
            snapshot
                .get("broker_instance")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| std::io::Error::other("readiness reply omitted broker_instance"))
        };
        match tokio::time::timeout(
            crate::broker_server::BrokerDeadlines::from_env().first_response,
            probe,
        )
        .await
        {
            Ok(Ok(instance)) => Some(instance),
            Ok(Err(_)) | Err(_) => None,
        }
    })
}

fn stop_incompatible_broker(
    runtime: &tokio::runtime::Runtime,
    endpoint: &str,
    instance: &str,
) -> Result<(), String> {
    use running_process::broker::client::send_admin_request;
    use running_process::broker::protocol::{AdminRequest, AdminVerb};

    let reply = send_admin_request(
        endpoint,
        AdminRequest {
            verb: AdminVerb::Shutdown as i32,
            drain_deadline_ms: SPAWN_WAIT_TIMEOUT.as_millis() as u64,
            ..Default::default()
        },
    )
    .map_err(|error| format!("could not stop incompatible broker instance {instance}: {error}"))?;
    if reply.exit_code != 0 {
        return Err(format!(
            "incompatible broker instance {instance} rejected shutdown with exit code {}",
            reply.exit_code
        ));
    }
    let deadline = Instant::now() + SPAWN_WAIT_TIMEOUT;
    while broker_instance_at(runtime, endpoint).is_some() {
        if Instant::now() >= deadline {
            return Err(format!(
                "incompatible broker instance {instance} did not release {endpoint}"
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

fn stage_broker_image(
    target: &std::path::Path,
    lease: &crate::broker_lease::BrokerLease,
) -> Result<(), String> {
    use std::io::{Read as _, Write as _};

    let source = std::env::current_exe().map_err(|error| error.to_string())?;
    if source == target {
        lease.check_fence().map_err(|error| error.to_string())?;
        return Ok(());
    }
    let mut next_renew = Instant::now() + Duration::from_secs(1);
    let mut checkpoint = || -> Result<(), String> {
        if Instant::now() >= next_renew {
            lease.renew().map_err(|error| error.to_string())?;
            next_renew = Instant::now() + Duration::from_secs(1);
        }
        Ok(())
    };
    if target.exists() && same_file_contents(&source, target, &mut checkpoint)? {
        lease.check_fence().map_err(|error| error.to_string())?;
        return Ok(());
    }
    let parent = target
        .parent()
        .ok_or_else(|| "stable broker path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let temporary = parent.join(format!(
        ".soldr-broker.stage-{}-{:016x}{suffix}",
        std::process::id(),
        stage_nonce()
    ));
    struct RemoveStagingFile(std::path::PathBuf);
    impl Drop for RemoveStagingFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove_staging_file = RemoveStagingFile(temporary.clone());
    let mut input = std::fs::File::open(&source).map_err(|error| error.to_string())?;
    let mut output = std::fs::File::create(&temporary).map_err(|error| error.to_string())?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|error| error.to_string())?;
        checkpoint()?;
    }
    output.flush().map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    output.sync_all().map_err(|error| error.to_string())?;
    drop(output);
    drop(input);
    lease.check_fence().map_err(|error| error.to_string())?;
    replace_staged_image(&temporary, target).map_err(|error| error.to_string())?;
    Ok(())
}

fn same_file_contents(
    left: &std::path::Path,
    right: &std::path::Path,
    checkpoint: &mut impl FnMut() -> Result<(), String>,
) -> Result<bool, String> {
    use std::io::Read as _;

    let left_meta = std::fs::metadata(left).map_err(|error| error.to_string())?;
    let right_meta = std::fs::metadata(right).map_err(|error| error.to_string())?;
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let hash = |path: &std::path::Path,
                checkpoint: &mut dyn FnMut() -> Result<(), String>|
     -> Result<zccache::hash::ContentHash, String> {
        let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        let mut hasher = zccache::hash::StreamHasher::new();
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            checkpoint()?;
        }
        Ok(hasher.finalize())
    };
    Ok(hash(left, checkpoint)? == hash(right, checkpoint)?)
}

fn stage_nonce() -> u64 {
    let mut bytes = [0_u8; 8];
    let _ = getrandom::fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

#[cfg(unix)]
fn replace_staged_image(source: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[derive(Debug, PartialEq, Eq)]
struct BrokerEndpointDiagnostics {
    executable: std::path::PathBuf,
    logical: String,
    bind: String,
    log: std::path::PathBuf,
}

fn broker_endpoint_diagnostics() -> Result<BrokerEndpointDiagnostics, String> {
    let endpoint = crate::broker_identity::ResolvedBrokerEndpoint::resolve()
        .map_err(|error| error.to_string())?;
    let log = endpoint
        .executable_path
        .parent()
        .ok_or_else(|| "broker executable has no parent".to_string())?
        .join("broker-spawn.log");
    Ok(BrokerEndpointDiagnostics {
        executable: endpoint.executable_path,
        logical: endpoint.logical_socket_path,
        bind: endpoint.bind_endpoint,
        log,
    })
}

fn render_ci_endpoint_diagnostics(
    ci_label: &str,
    diagnostics: &BrokerEndpointDiagnostics,
) -> String {
    format!(
        "soldr broker endpoint: ci={ci_label} executable={} logical={} bind={} log={}",
        diagnostics.executable.display(),
        diagnostics.logical,
        diagnostics.bind,
        diagnostics.log.display()
    )
}

fn emit_ci_endpoint_diagnostics() {
    let Some(ci_label) = crate::optimize_detect::detect_ci() else {
        return;
    };
    match broker_endpoint_diagnostics() {
        Ok(diagnostics) => eprintln!("{}", render_ci_endpoint_diagnostics(ci_label, &diagnostics)),
        Err(error) => eprintln!("soldr broker endpoint: ci={ci_label} unresolved: {error}"),
    }
}

#[cfg(windows)]
fn replace_staged_image(source: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn open_append(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

pub(crate) fn daemon_stdio(log: &std::fs::File) -> DaemonStdio<'_> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        DaemonStdio {
            stdout: DaemonStdioSource::Fd(log.as_fd()),
            stderr: DaemonStdioSource::Fd(log.as_fd()),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        DaemonStdio {
            stdout: DaemonStdioSource::Handle(log.as_handle()),
            stderr: DaemonStdioSource::Handle(log.as_handle()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // soldr#2024-adjacent hazard: env::set_var/remove_var races across
    // threads within one test binary. These tests share one lock so they
    // never interleave with each other -- matches the pattern other
    // env-var-gated tests in this crate use.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    crate::timed_test!(
        broker_spawn_env_preserves_soldr_and_endpoint_resolver_inputs,
        {
            use std::ffi::OsString;

            let forwarded = filter_broker_spawn_env(vec![
                (
                    OsString::from("SOLDR_CACHE_DIR"),
                    OsString::from("/tmp/cache"),
                ),
                (OsString::from("HOME"), OsString::from("/mounted/home")),
                (
                    OsString::from("XDG_RUNTIME_DIR"),
                    OsString::from("/run/user/123"),
                ),
                (OsString::from("PATH"), OsString::from("/usr/bin")),
            ]);
            assert_eq!(
                forwarded,
                vec![
                    (
                        OsString::from("SOLDR_CACHE_DIR"),
                        OsString::from("/tmp/cache")
                    ),
                    (OsString::from("HOME"), OsString::from("/mounted/home")),
                    (
                        OsString::from("XDG_RUNTIME_DIR"),
                        OsString::from("/run/user/123")
                    ),
                ],
            );
        }
    );

    crate::timed_test!(wrapper_invocation_is_never_eligible, {
        let _guard = ENV_LOCK.lock().unwrap();
        let raw_args = vec!["soldr".to_string(), "/usr/bin/rustc".to_string()];
        assert!(crate::wrapper::is_wrapper_invocation(&raw_args[1]));
        assert!(!front_door_broker_spawn_eligible(&raw_args));
    });

    crate::timed_test!(broker_subcommand_itself_does_not_recursively_spawn, {
        let _guard = ENV_LOCK.lock().unwrap();
        let raw_args = vec![
            "soldr".to_string(),
            "broker".to_string(),
            "serve".to_string(),
        ];
        assert!(!front_door_broker_spawn_eligible(&raw_args));
    });

    // soldr#2388: the broker is unconditional — an ordinary invocation is
    // always eligible (there is no opt-out).
    crate::timed_test!(ordinary_invocation_is_eligible, {
        let _guard = ENV_LOCK.lock().unwrap();
        let raw_args = vec!["soldr".to_string(), "status".to_string()];
        assert!(front_door_broker_spawn_eligible(&raw_args));
    });

    crate::timed_test!(no_positional_arg_is_ineligible, {
        let _guard = ENV_LOCK.lock().unwrap();
        let raw_args = vec!["soldr".to_string()];
        assert!(!front_door_broker_spawn_eligible(&raw_args));
    });

    crate::timed_test!(ci_diagnostics_preserve_machine_readable_output, {
        assert!(!ci_endpoint_diagnostics_eligible(&[
            "soldr".into(),
            "env".into(),
            "--json".into(),
        ]));
        assert!(!ci_endpoint_diagnostics_eligible(&[
            "soldr".into(),
            "prepare".into(),
            "--github-env=output.env".into(),
        ]));
        assert!(!ci_endpoint_diagnostics_eligible(&[
            "soldr".into(),
            "env".into(),
            "--shell-export".into(),
        ]));
        assert!(ci_endpoint_diagnostics_eligible(&[
            "soldr".into(),
            "build".into(),
        ]));
    });

    crate::timed_test!(ci_diagnostics_show_the_one_stable_path_derived_endpoint, {
        let diagnostics = BrokerEndpointDiagnostics {
            executable: std::path::PathBuf::from("/home/me/.soldr/broker/soldr-broker"),
            logical: "/home/me/.soldr/broker/soldr-broker.sock".into(),
            bind: "/home/me/.soldr/broker/soldr-broker.sock".into(),
            log: std::path::PathBuf::from("/home/me/.soldr/broker/broker-spawn.log"),
        };
        let rendered = render_ci_endpoint_diagnostics("github_actions", &diagnostics);
        assert!(rendered.contains("ci=github_actions"));
        assert!(rendered.contains("logical=/home/me/.soldr/broker/soldr-broker.sock"));
        assert!(rendered.contains("bind=/home/me/.soldr/broker/soldr-broker.sock"));
    });
}
