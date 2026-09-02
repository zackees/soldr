/// Environment switch that enables tokio-console diagnostics for the daemon.
pub const TOKIO_CONSOLE_ENV_VAR: &str = "SOLDR_DAEMON_TOKIO_CONSOLE";
/// Optional soldr-owned bridge to console-subscriber's recording path.
///
/// Detached daemons intentionally start from a scrubbed environment and
/// forward only `SOLDR_*`, so callers cannot rely on the upstream
/// `TOKIO_CONSOLE_RECORD_PATH` variable crossing the spawn boundary.
pub const TOKIO_CONSOLE_RECORD_PATH_ENV_VAR: &str = "SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH";
/// Optional soldr-owned bridge for console-subscriber's publish interval.
///
/// The detached daemon only inherits `SOLDR_*` variables, so the upstream
/// `TOKIO_CONSOLE_PUBLISH_INTERVAL` variable cannot configure a normally
/// spawned daemon. Values are milliseconds; `0` and malformed values leave
/// console-subscriber's default unchanged.
pub const TOKIO_CONSOLE_PUBLISH_INTERVAL_MS_ENV_VAR: &str =
    "SOLDR_DAEMON_TOKIO_CONSOLE_PUBLISH_INTERVAL_MS";

fn tokio_console_requested() -> bool {
    std::env::var(TOKIO_CONSOLE_ENV_VAR)
        .map(|v| crate::core::flag_value(&v))
        .unwrap_or(false)
}

fn parse_tokio_console_publish_interval(raw: Option<&str>) -> Option<Duration> {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
}

fn tokio_console_publish_interval() -> Option<Duration> {
    parse_tokio_console_publish_interval(
        std::env::var(TOKIO_CONSOLE_PUBLISH_INTERVAL_MS_ENV_VAR)
            .ok()
            .as_deref(),
    )
}

/// Install the tokio-console subscriber layer when
/// [`TOKIO_CONSOLE_ENV_VAR`] is truthy, so `tokio-console` can attach to
/// the daemon's runtime and expose per-task poll/idle time and worker
/// behaviour — the tooling needed to pin down the daemon-worker
/// `sched_yield` spin from soldr#1334 at the async-task level.
///
/// `console_subscriber::spawn()` panics unless the crate was compiled
/// with `--cfg tokio_unstable` (the tokio task instrumentation is
/// otherwise absent). We `catch_unwind` that so a normal release build
/// simply logs a hint instead of crashing the daemon. Mirrors the
/// established `zccache-daemon` pattern. No-op (and no subscriber
/// installed — daemon stays silent as before) when the env var is unset.
#[cfg(feature = "tokio-console")]
fn maybe_init_tokio_console() {
    if !tokio_console_requested() {
        return;
    }
    let spawn = || {
        let mut builder = console_subscriber::Builder::default().with_default_env();
        if let Some(path) = std::env::var_os(TOKIO_CONSOLE_RECORD_PATH_ENV_VAR) {
            builder = builder.recording_path(path);
        }
        if let Some(interval) = tokio_console_publish_interval() {
            builder = builder.publish_interval(interval);
        }
        builder.spawn()
    };
    match std::panic::catch_unwind(spawn) {
        Ok(console_layer) => {
            use tracing_subscriber::prelude::*;
            let _ = tracing_subscriber::registry()
                .with(console_layer)
                .try_init();
            tracing::info!(
                "tokio-console enabled for soldr-daemon; connect with `tokio-console` \
                 (default 127.0.0.1:6669)"
            );
        }
        Err(_) => {
            eprintln!(
                "soldr-daemon: {TOKIO_CONSOLE_ENV_VAR} set but console-subscriber is \
                 inert — rebuild with RUSTFLAGS=\"--cfg tokio_unstable\" to use tokio-console"
            );
        }
    }
}

#[cfg(not(feature = "tokio-console"))]
fn maybe_init_tokio_console() {
    if tokio_console_requested() {
        eprintln!(
            "soldr-daemon: {TOKIO_CONSOLE_ENV_VAR} set but soldr-cli was built without \
             the `tokio-console` feature; rebuild with `--features tokio-console` and \
             RUSTFLAGS=\"--cfg tokio_unstable\" to use tokio-console"
        );
    }
}

pub fn run(opts: ServerOptions) -> Result<(), ServerError> {
    // Opt-in async-runtime instrumentation. Must run before the runtime
    // is built so console-subscriber's aggregator is in place.
    maybe_init_tokio_console();

    // L6 (soldr#980): cap the daemon Tokio runtime to a small fixed
    // worker count so it doesn't oversubscribe the CPU during cargo
    // builds. The daemon handles IPC + bookkeeping; rustc is the CPU
    // bottleneck. Off-CPU profiling showed `tokio-rt-worker` context
    // switches were ~3x higher cold vs bare and `sched_yield` events
    // tripled — capping at 2-4 workers cuts the unused-worker noise.
    // L6 reverted post-#980 perf measurement: capping the worker pool
    // to 2-4 starved the daemon's Compile dispatch (each compile awaits
    // its rustc subprocess on a worker; the cap forced serial dispatch
    // when cargo wanted N-way parallelism). Use the full host
    // parallelism — the tokio scheduler keeps workers cheap when idle.
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let workers = available.max(2);
    tracing::info!("soldr-daemon Tokio runtime: {workers} workers (host parallelism: {available})");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;
    runtime.block_on(run_async(opts))
}

/// Async daemon entry point. Use this when calling from inside an
/// existing tokio runtime (e.g. `soldr daemon start --foreground`
/// dispatched from `main`'s `#[tokio::main]` runtime). The synchronous
/// `run` builds its own multi-thread runtime and is the right entry
/// point for the `soldr-daemon` bin target whose `main` has no
/// ambient runtime. Calling `run` from within an ambient runtime
/// panics with "Cannot start a runtime from within a runtime" — that
/// was the failure on soldr#985's perf-matrix CI run.
pub async fn run_async(opts: ServerOptions) -> Result<(), ServerError> {
    let paths = SoldrPaths::new()?;
    std::fs::create_dir_all(soldr_daemon_dir(&paths))?;
    init_embedded_service_file_tracing(&paths);

    // Bounded grace for the stop→relaunch race: the previous daemon's lock
    // is released only when its process exits, which lags the stop
    // acknowledgment. Retry while nobody serves the root; back off
    // immediately when a live daemon does (issue #1814 semantics).
    let _root_ownership = {
        use crate::daemon::lifecycle::RootAcquireOutcome;
        let outcome = crate::daemon::lifecycle::RootOwnershipGuard::acquire_with_grace(
            &paths,
            ROOT_OWNERSHIP_ACQUIRE_BUDGET,
            ROOT_OWNERSHIP_ACQUIRE_POLL,
            || existing_daemon_pid(&paths),
        )?;
        match outcome {
            RootAcquireOutcome::Acquired(guard) => guard,
            RootAcquireOutcome::AlreadyServing(pid) => {
                return Err(ServerError::AlreadyRunning(pid));
            }
            RootAcquireOutcome::TimedOut => {
                return Err(ServerError::Io(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    crate::daemon::lifecycle::describe_root_ownership_conflict(&paths),
                )));
            }
        }
    };

    if let Some(existing) = existing_daemon_pid(&paths) {
        return Err(ServerError::AlreadyRunning(existing));
    }
    // Claim the control endpoint immediately after the version-blind
    // occupancy check. Delaying bind until a detached accept task used to
    // leave a large initialization window in which an older daemon could bind
    // and then have its live socket unlinked by this process. Unix hosts
    // claim a filesystem socket (with the identity fencing the retirement
    // fence); Windows hosts serve a named-pipe pool and take the None branch.
    let control_listener =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            None
        } else {
            let sock = crate::daemon::session_endpoint::resolved_control_endpoint_path(&paths)?;
            Some(crate::platform::ipc::listener::claim_control_endpoint_at(
                &sock,
            )?)
        };

    let db_path = data_db_path(&paths);
    let start_instant = Instant::now();
    let idle_timeout_secs = u32::try_from(opts.idle_timeout.as_secs()).ok();
    let daemon_identity = current_daemon_process(&paths, idle_timeout_secs)
        .map_err(|err| ServerError::Io(std::io::Error::other(err.to_string())))?;

    // Bind before heavyweight startup; SESSION payloads await compile-service publication.
    let session_listener = crate::daemon::session_endpoint::resolve_session_listener(&paths)?
        .ok_or_else(|| {
            ServerError::Io(std::io::Error::other(
                "broker-facing SESSION endpoint was not configured",
            ))
        })?;
    let handoff_listener = crate::daemon::session_endpoint::resolve_handoff_listener(&paths)?;

    // soldr#2436 phase 2: bound the journal, attribute any un-drained
    // predecessor, then record this start with version + exe identity.
    crate::daemon::lifecycle::rotate_lifecycle_journal(&paths);
    crate::daemon::lifecycle::detect_unclean_shutdown(&paths);
    crate::daemon::lifecycle::append_lifecycle_event_with(
        &paths,
        "spawn",
        crate::daemon::lifecycle::LifecycleDetails::recording_daemon_identity(),
    );
    // A panic previously left no exit record at all (soldr#2436 fact 4).
    // Chain the default hook so backtraces still print.
    {
        let panic_paths = paths.clone();
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            crate::daemon::lifecycle::append_lifecycle_event_with(
                &panic_paths,
                "died-panic",
                crate::daemon::lifecycle::LifecycleDetails {
                    reason: Some(crate::daemon::lifecycle::LifecycleReason::Panic),
                    ..crate::daemon::lifecycle::LifecycleDetails::recording_daemon_identity()
                },
            );
            default_hook(info);
        }));
    }
    let mut session_identity = daemon_identity.clone();
    session_identity.ipc_endpoint.path =
        crate::daemon::session_endpoint::resolved_session_endpoint_path(&paths)?;
    if let Ok(namespace_id) = std::env::var("RUNNING_PROCESS_BROKER_V1_BACKEND_NAMESPACE") {
        if !namespace_id.is_empty() {
            session_identity.ipc_endpoint.namespace_id = namespace_id;
        }
    }
    crate::daemon::backend_handle_adoption::publish_broker_route_claim(&paths, &session_identity)?;
    let session_mux = Arc::new(crate::daemon::session_endpoint::soldr_session_endpoint_mux(
        session_identity,
    ));
    let (compile_readiness, compile_publisher) =
        crate::daemon::session_endpoint::CompileServiceReadiness::pending();
    let (session_handle, handoff_handle) =
        crate::daemon::session_endpoint::spawn_session_endpoint_servers(
            session_listener,
            handoff_listener,
            compile_readiness,
            paths.clone(),
            session_mux,
        );

    // Embedded zccache initializes asynchronously. The first operation that
    // actually needs it awaits this task through `CompileServiceReadiness`;
    // broker probes remain independent and responsive throughout.
    let compile_paths = paths.clone();
    let compile_identity = daemon_identity.clone();
    let compile_handle = tokio::spawn(async move {
        let result = start_compile_service(&compile_paths, &compile_identity).await;
        match &result {
            Ok(service) => compile_publisher.publish(Ok(Arc::clone(service))),
            Err(error) => compile_publisher.publish(Err(format!(
                "embedded compile service initialization failed: {error:?}"
            )
            .into())),
        }
        result
    });

    // Database initialization runs concurrently on the blocking pool. It is
    // not a prerequisite for broker readiness or SESSION compile execution.
    let startup_db_path = db_path.clone();
    let db_result = tokio::task::spawn_blocking(move || {
        let touched = TargetRegistry::open(&startup_db_path).map(|_| ());
        let _ = db::ensure_initialized(&startup_db_path);
        let _ = cook_index::ensure_initialized(&startup_db_path);
        touched
    })
    .await;
    let db_result = match db_result {
        Ok(result) => result,
        Err(error) => {
            compile_handle.abort();
            session_handle.abort();
            handoff_handle.abort();
            return Err(ServerError::Io(std::io::Error::other(error.to_string())));
        }
    };
    if let Err(error) = db_result {
        compile_handle.abort();
        session_handle.abort();
        handoff_handle.abort();
        return Err(ServerError::Registry(error));
    }

    let compile_service = match compile_handle.await {
        Ok(Ok(service)) => service,
        Ok(Err(error)) => {
            session_handle.abort();
            handoff_handle.abort();
            return Err(error);
        }
        Err(error) => {
            session_handle.abort();
            handoff_handle.abort();
            return Err(ServerError::Io(std::io::Error::other(format!(
                "embedded compile service initialization task failed: {error}"
            ))));
        }
    };

    // L4 (issue soldr#980): start the background event-flusher BEFORE we
    // accept any IPC traffic so the very first compile event lands on
    // a live channel. The drain task lives in the daemon's tokio runtime
    // and exits cleanly via `shutdown()` below.
    let event_batcher = EventBatcher::start(db_path.clone());

    let state = Arc::new(State {
        db_path,
        paths: paths.clone(),
        daemon_identity,
        start_instant,
        request_count: AtomicU64::new(0),
        last_activity_ms: AtomicU64::new(0),
        exit_via_idle: AtomicBool::new(false),
        cook_hits_this_session: AtomicU64::new(0),
        shutdown: Arc::new(crate::daemon::maintenance::ShutdownSignal::default()),
        event_batcher,
        compile_admission: CompileAdmission::new(
            ipc_queue_capacity(windows_listener_pool_size()),
            // soldr#2023: read the limit the service applied rather than
            // resolving a second time — see `crate::compile_limit`.
            compile_service.applied_jobs().jobs,
        ),
        compile_service,
    });

    let accept_state = state.clone();
    let accept_handle = match control_listener {
        Some((listener, _identity)) => tokio::spawn(async move {
            let _ = run_accept_loop_unix(listener, accept_state).await;
        }),
        None => {
            let paths_for_accept = paths.clone();
            tokio::spawn(async move {
                let _ = run_accept_loop_windows(paths_for_accept, accept_state).await;
            })
        }
    };

    let idle_handle = (opts.idle_timeout != Duration::MAX).then(|| {
        let idle_state = state.clone();
        let idle_timeout = opts.idle_timeout;
        tokio::spawn(async move {
            run_idle_watchdog(idle_state, idle_timeout).await;
        })
    });

    let owner_handle = opts.owner_pid.map(|owner_pid| {
        let owner_state = state.clone();
        tokio::spawn(async move {
            run_owner_watchdog(owner_state, owner_pid).await;
        })
    });

    let maintenance_context = crate::daemon::maintenance::MaintenanceContext {
        paths: paths.clone(),
        db_path: state.db_path.clone(),
        compile_service: Arc::clone(&state.compile_service),
        shutdown: Arc::clone(&state.shutdown),
    };
    let maintenance_handle = tokio::spawn(async move {
        crate::daemon::maintenance::run_loop(maintenance_context).await;
    });

    // soldr#3038 / soldr#3057: opt-in, fail-fast RSS ceiling watchdog.
    // Spawned only when `SOLDR_DAEMON_RSS_CEILING_BYTES` parses to a
    // positive byte count, so an ordinary daemon start (the overwhelming
    // common case) pays no extra timer, file write, or mimalloc stats call
    // at all -- see `rss_ceiling`'s module docs for the full design
    // rationale, including why a breach now dumps memory and exits rather
    // than recording and continuing. The sampled profiler must be started
    // before the watchdog can produce a useful heap.pprof on breach, and
    // only when a ceiling is actually configured -- same gate, same call.
    let rss_ceiling_bytes = crate::daemon::rss_ceiling::ceiling_bytes_from_env();
    crate::daemon::rss_ceiling::start_sampled_profiler_if_configured(rss_ceiling_bytes);
    let rss_ceiling_handle = rss_ceiling_bytes.map(|ceiling_bytes| {
        let rss_paths = paths.clone();
        let rss_shutdown = Arc::clone(&state.shutdown);
        tokio::spawn(async move {
            crate::daemon::rss_ceiling::run_watchdog(
                rss_paths,
                rss_shutdown,
                ceiling_bytes,
                crate::daemon::rss_ceiling::ProcessRole::Daemon,
            )
            .await;
        })
    });

    let signal_state = state.clone();
    tokio::spawn(async move {
        if (tokio::signal::ctrl_c().await).is_ok() {
            signal_state.shutdown.request();
        }
    });
    // Issue #1286 (F1): SIGTERM previously killed the daemon without
    // the graceful-drain path below, silently discarding the embedded
    // zccache's in-memory artifact index + depgraph — a full cold
    // cache on the next daemon start. `pkill soldr-daemon`, container
    // stop, and service managers all send TERM, not INT. The signal
    // wait lives in the platform process facade (Windows parks the
    // hook — there is no POSIX SIGTERM to wait for).
    let term_state = state.clone();
    tokio::spawn(async move {
        if crate::platform::process::signal::wait_for_terminate_signal().await {
            term_state.shutdown.request();
        }
    });

    state.shutdown.wait().await;
    arm_shutdown_watchdog(paths.clone());
    accept_handle.abort();
    session_handle.abort();
    handoff_handle.abort();
    if let Some(handle) = idle_handle {
        handle.abort();
    }
    if let Some(handle) = owner_handle {
        handle.abort();
    }
    let shutdown_phase = |name| {
        crate::daemon::lifecycle::append_lifecycle_event_with(
            &paths,
            name,
            crate::daemon::lifecycle::LifecycleDetails::default().for_target_route(
                std::process::id(),
                state.daemon_identity.started_at_unix_ms,
                state.daemon_identity.ipc_endpoint.path.clone(),
            ),
        );
    };
    shutdown_phase("shutdown-phase-maintenance");
    // A destructive pass that already acquired the root maintenance lease is
    // allowed to finish. In particular, await its spawn_blocking deletion
    // worker before releasing root ownership.
    let _ = maintenance_handle.await;
    // Same shutdown-aware loop shape as maintenance_handle (it selects on
    // `shutdown.wait()` internally and returns promptly), so it is awaited
    // rather than aborted -- an abort mid-write could leave a torn
    // `rss-ceiling-v1.json` behind.
    if let Some(handle) = rss_ceiling_handle {
        let _ = handle.await;
    }

    // L4 (issue soldr#980): drain whatever the background event flusher
    // has staged in memory before the daemon process exits. Must run
    // BEFORE the redb file lock is released so the final write txn
    // succeeds; `shutdown()` awaits the drain task's ack.
    shutdown_phase("shutdown-phase-event-batcher");
    let _ = state.event_batcher.shutdown().await;

    // Issue #977 / #980 L1: drain the embedded zccache service before
    // any other shutdown work so pending writes flush through.
    shutdown_phase("shutdown-phase-compile-service");
    shutdown_compile_service(&state).await;

    // Deliberately retain the protobuf route claim and Unix socket nodes.
    // A check-then-unlink fence is not atomic: an older Soldr release that
    // does not honor `root-owner.lock` can publish a successor between the
    // check and unlink, and the retiring daemon would delete the successor's
    // state. Startup already probes liveness, atomically replaces stale claims, and
    // reclaims a stale socket before binding, so retaining these artifacts is
    // safe and closes that cross-version race.
    let event = if state.exit_via_idle.load(Ordering::Relaxed) {
        "died-idle"
    } else {
        "died-shutdown"
    };
    append_lifecycle_event(&paths, event);
    Ok(())
}

/// Env override for [`SHUTDOWN_WATCHDOG_GRACE`], in seconds. `0` disables.
pub const SHUTDOWN_WATCHDOG_ENV_VAR: &str = "SOLDR_SHUTDOWN_WATCHDOG_SECS";

/// How long teardown may run before the process exits regardless.
///
/// Deliberately under `GRACEFUL_SHUTDOWN_WAIT_TIMEOUT` (300s) so the process
/// is gone before the client gives up and reports failure.
pub const SHUTDOWN_WATCHDOG_GRACE: Duration = Duration::from_secs(240);

pub(crate) fn shutdown_watchdog_grace() -> Option<Duration> {
    parse_watchdog_grace(std::env::var(SHUTDOWN_WATCHDOG_ENV_VAR).ok().as_deref())
}

/// `None` means "no backstop". Only an explicit `0` may produce it — a
/// malformed override falls back to the default rather than silently
/// disabling the one thing guaranteeing the process exits.
pub(crate) fn parse_watchdog_grace(raw: Option<&str>) -> Option<Duration> {
    let Some(raw) = raw else {
        return Some(SHUTDOWN_WATCHDOG_GRACE);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => Some(SHUTDOWN_WATCHDOG_GRACE),
    }
}

/// Guarantee the process exits once teardown has started.
///
/// Every step after this point is a best-effort flush, and several are
/// unbounded: the maintenance join waits out an in-flight pass whose
/// `spawn_blocking` deletion worker cannot be cancelled, the embedded
/// zccache drain takes an untimed publication write barrier and joins its
/// index writer, and dropping the multi-thread runtime waits for every
/// outstanding blocking task. Any one of them wedging left a daemon alive
/// forever holding the route claim and endpoints — and the CLI
/// deliberately never force-kills a daemon that acknowledged shutdown, so
/// nothing else bounded it.
///
/// A detached OS thread, NOT a tokio task: it has to fire even when the
/// runtime is stalled or its blocking pool is fully occupied, which is
/// exactly the situation it exists for.
///
/// soldr#3059: this used to call `std::process::exit(0)` on expiry, telling
/// a supervisor, a test, or a CI lane that watched the exit status that a
/// daemon which blew its entire [`SHUTDOWN_WATCHDOG_GRACE`] exited cleanly.
/// It now exits [`SHUTDOWN_WATCHDOG_EXIT_CODE`] instead — a clean drain
/// (the normal `Ok(())` return from the caller) still reaches the process's
/// ordinary `main` exit path and still exits 0; only this expiry arm
/// changes. Nothing in this repo waits on the daemon's raw OS exit status
/// today, so this is a pure legibility improvement (a log line and a
/// distinctive code for whoever *does* start checking) rather than a
/// behavior change for any existing caller. Checked directly:
/// `soldr_main_helpers.rs`'s `DaemonSubcommand::Stop` calls
/// `lifecycle::wait_for_shutdown_responder`, which polls `pid_is_alive` plus
/// an IPC status probe and never inspects an exit code;
/// `tests/common/isolated_daemon.rs`'s `Drop` impl only checks
/// `try_wait().is_some()` (exited vs. not); and
/// `cli_daemon_single_instance.rs`'s loser-daemon assertion matches on
/// stderr text ("already running"), explicitly noting the exit code is not
/// the contract there.
fn arm_shutdown_watchdog(paths: SoldrPaths) {
    let Some(grace) = shutdown_watchdog_grace() else {
        return;
    };
    std::thread::Builder::new()
        .name("soldr-shutdown-watchdog".into())
        .spawn(move || {
            std::thread::sleep(grace);
            tracing::error!(
                grace_secs = grace.as_secs(),
                exit_code = SHUTDOWN_WATCHDOG_EXIT_CODE,
                "graceful shutdown did not complete within the watchdog grace; \
                 exiting now. Cache state may not be fully flushed."
            );
            // Record the expiry in the lifecycle JSONL before exiting: the
            // ordinary `died-idle`/`died-shutdown` event at the end of the
            // graceful path may never run if teardown is genuinely wedged,
            // so without this line a watchdog-forced exit and a clean one
            // are indistinguishable in the log.
            crate::daemon::lifecycle::append_lifecycle_event(
                &paths,
                "shutdown-watchdog-expired",
            );
            std::process::exit(SHUTDOWN_WATCHDOG_EXIT_CODE);
        })
        .ok();
}

/// Distinctive non-zero exit code for a watchdog-forced exit (soldr#3059).
///
/// 124 is the conventional "command timed out" exit code (GNU coreutils
/// `timeout(1)`), so a log or CI lane that already recognizes that
/// convention reads this correctly without needing a soldr-specific
/// lookup. Deliberately not `1`: `rss_ceiling::die_on_breach` already uses
/// plain `1` for a memory-ceiling breach, and the two failure modes
/// (over-budget memory vs. a stuck graceful drain) should not collapse to
/// the same code.
const SHUTDOWN_WATCHDOG_EXIT_CODE: i32 = 124;

fn existing_daemon_pid(paths: &SoldrPaths) -> Option<u32> {
    select_existing_daemon_pid(
        crate::daemon::lifecycle::claimed_daemon_occupies_route(paths),
        is_live(paths),
    )
}

fn select_existing_daemon_pid(
    version_blind: Option<u32>,
    protocol_aware: Option<u32>,
) -> Option<u32> {
    version_blind.or(protocol_aware)
}

#[cfg(test)]
mod endpoint_occupancy_tests {
    use super::select_existing_daemon_pid;

    #[test]
    fn protocol_mismatched_daemon_still_blocks_direct_startup() {
        assert_eq!(select_existing_daemon_pid(Some(41), None), Some(41));
        assert_eq!(select_existing_daemon_pid(None, Some(42)), Some(42));
        assert_eq!(select_existing_daemon_pid(None, None), None);
    }
}

// The Unix endpoint exclusivity and retirement-fence tests live in
// `tests/daemon_unix_endpoint.rs` (`#![cfg(unix)]`) — they exercise
// the platform listener leaf now that server.rs is host-neutral.

/// The embedded zccache service runs in-process, so its `tracing::warn!`
/// events otherwise have no durable destination in the normal daemon mode.
/// Keep a daily rolling file under soldr's daemon state for post-build
/// investigation; startup must remain best-effort when the cache is on a
/// read-only or otherwise unusual filesystem.
fn init_embedded_service_file_tracing(paths: &SoldrPaths) {
    let log_dir = embedded_service_log_dir(paths);
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "soldr-daemon: cannot create embedded zccache warning log directory {}: {error}",
            log_dir.display()
        );
        return;
    }
    let appender = tracing_appender::rolling::daily(log_dir, "embedded-zccache.warn.log");
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(appender)
        .try_init();
}

fn embedded_service_log_dir(paths: &SoldrPaths) -> PathBuf {
    soldr_daemon_dir(paths).join("logs")
}

/// Accept loop for the Unix control endpoint. `listener` is the claimed
/// filesystem socket from the platform listener leaf, which already
/// resolved each peer's identity and current-user admission during
/// accept.
async fn run_accept_loop_unix(
    listener: crate::platform::ipc::listener::BoxedControlListener,
    state: Arc<State>,
) -> std::io::Result<()> {
    loop {
        let accepted = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => continue,
        };
        if !accepted.peer.is_current_user {
            tracing::warn!(target: "soldr::daemon", "rejected foreign daemon-control peer");
            continue;
        }
        let peer = crate::daemon::ipc_peer::PeerIdentity::from_accepted_peer(&accepted.peer);
        let state = state.clone();
        tokio::spawn(async move {
            let _ = handle_connection(accepted.stream, state, peer).await;
        });
    }
}

/// Accept loop for the Windows control endpoint: a self-replenishing
/// named-pipe instance pool (see [`accept_windows_pipe_instance`]).
async fn run_accept_loop_windows(paths: SoldrPaths, state: Arc<State>) -> std::io::Result<()> {
    // soldr#1808: identity failure is fatal here — this loop *is* the
    // endpoint. Propagating beats a fallback name no client would dial.
    let pipe_name = crate::daemon::session_endpoint::resolved_control_endpoint_path(&paths)?
        .to_string_lossy()
        .into_owned();
    let pool_size = windows_listener_pool_size();
    tracing::info!(
        pool_size,
        queue_capacity = state.compile_admission.capacity,
        expected_compile_slots = state.compile_admission.expected_compile_slots,
        "soldr-daemon Windows named-pipe listener pool ready"
    );
    for index in 0..pool_size {
        spawn_windows_pipe_instance(pipe_name.clone(), state.clone(), index == 0);
    }
    // Park until shutdown rather than forever. The pool instances are
    // detached and self-replenishing, so aborting this task cannot stop
    // them — each instance observes the same signal and drops its own pipe
    // handle. Returning here is what lets the caller's `.await` complete.
    state.shutdown.wait().await;
    Ok(())
}

fn spawn_windows_pipe_instance(pipe_name: String, state: Arc<State>, first_pipe_instance: bool) {
    // Keep this launcher synchronous. Calling `tokio::spawn` directly from
    // `accept_windows_pipe_instance` would make the async function's opaque
    // future recursively depend on itself, which Windows rejects because its
    // `Send` bound cannot be inferred.
    tokio::spawn(async move {
        if let Err(error) =
            accept_windows_pipe_instance(pipe_name, state, first_pipe_instance).await
        {
            tracing::debug!(%error, "Windows named-pipe listener exited");
        }
    });
}

async fn accept_windows_pipe_instance(
    pipe_name: String,
    state: Arc<State>,
    first_pipe_instance: bool,
) -> std::io::Result<()> {
    // Never open a fresh instance once teardown has begun; that would
    // re-arm the endpoint the shutdown path is trying to retire.
    if state.shutdown.is_requested() {
        return Ok(());
    }
    let mut server = crate::platform::ipc::peer::create_owner_only_windows_pipe(
        &pipe_name,
        first_pipe_instance,
    )?;

    // Drop the instance as soon as shutdown starts. Otherwise a wrapper can
    // connect after the compile service has latched shut. Unix drops its
    // listener with the accept task; this gives Windows the same fallback.
    let connected = tokio::select! {
        result = crate::platform::ipc::peer::pipe_server_connect(&mut server) => result.is_ok(),
        _ = state.shutdown.wait() => return Ok(()),
    };

    if state.shutdown.is_requested() {
        return Ok(());
    }
    if connected {
        // Replenish before parsing the connected request, keeping the pool
        // admission capacity independent from compile execution throughput.
        spawn_windows_pipe_instance(pipe_name, state.clone(), false);
        let peer = PeerIdentity::from_windows_pipe_server(&mut server);
        let _ = handle_connection(server, state, peer).await;
    } else {
        spawn_windows_pipe_instance(pipe_name, state, false);
    }
    Ok(())
}

/// Read budget for draining a doomed connection (#1853). Short on purpose:
/// we only need to clear what the peer already queued, not wait for more.
const DRAIN_READ_TIMEOUT: Duration = Duration::from_millis(250);

/// How long a starting daemon waits for a busy root-owner lock when no live
/// daemon serves the root — the window where the previous owner has
/// acknowledged `daemon stop` but its process (and thus the lock handle) has
/// not finished exiting. Well under the broker's 60s daemon-readiness budget
/// so a genuine conflict still surfaces inside one spawn attempt.
const ROOT_OWNERSHIP_ACQUIRE_BUDGET: Duration = Duration::from_secs(10);
const ROOT_OWNERSHIP_ACQUIRE_POLL: Duration = Duration::from_millis(200);

/// Tell a peer whose protocol we cannot speak *why* we are hanging up, then
/// close cleanly (#1853).
///
/// The reject record is version-independent by construction (see
/// [`crate::daemon::ipc::REJECT_RECORD_VERSION`]), so even a client several
/// versions old gets a diagnosable `InvalidData` with a message instead of an
/// opaque `ECONNRESET` after burning its whole retry budget.
async fn reject_version_mismatch<S>(stream: &mut S, peer_version: u32)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    use tokio::time::timeout;
    tracing::warn!(
        peer_version,
        daemon_version = PROTOCOL_VERSION,
        "soldr-daemon: rejecting IPC peer speaking a different protocol version",
    );
    let record = crate::daemon::ipc::encode_reject_record(&format!(
        "protocol version mismatch: peer={peer_version}, daemon={PROTOCOL_VERSION}"
    ));
    // Bounded by the drain budget, not the handshake budget: we are already
    // hanging up, so a peer that will not read must not hold a worker.
    let _ = timeout(DRAIN_READ_TIMEOUT, stream.write_all(&record)).await;
    let _ = timeout(DRAIN_READ_TIMEOUT, stream.flush()).await;
    drain_then_close(stream).await;
}

/// Half-close, then discard whatever the peer already queued, before dropping
/// the connection (#1853).
///
/// On an AF_UNIX SOCK_STREAM socket, closing while bytes remain in our receive
/// queue makes the kernel raise `ECONNRESET` on the peer (Linux
/// `unix_release_sock`), which a client cannot distinguish from a daemon
/// crash — so it retries for its entire budget and then hard-fails with no
/// fallback. Draining first turns that into a clean EOF, which every existing
/// client already classifies as "daemon unavailable" and degrades on. That is
/// what makes this fix reach clients that were shipped long before it.
/// Unix only, deliberately. `ECONNRESET`-on-unread-data is an AF_UNIX /
/// SOCK_STREAM behavior; Windows named pipes have no RST concept, so there is
/// nothing to prevent there — and paying a shutdown/drain round trip on every
/// rejected connection measurably slows the Windows hot path.
async fn drain_then_close<S>(stream: &mut S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        // No-op on Windows: named pipes cannot raise `ECONNRESET`, so
        // dropping the handle already yields the clean end-of-stream the
        // Unix path has to work for.
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;
    // Half-close first so a peer still streaming a large body sees EOF and
    // stops writing, instead of us draining against a live producer.
    //
    // Every step here is bounded by DRAIN_READ_TIMEOUT, not the handshake
    // budget: this is a courtesy on a connection we have already decided to
    // drop, so it must never become a latency source. An earlier revision
    // used the 5s handshake timeout here and pushed
    // `dependency_failure_cancels_sibling_lint_children` past its 5s budget.
    let _ = timeout(DRAIN_READ_TIMEOUT, stream.shutdown()).await;
    let cap =
        u64::from(crate::daemon::protocol::MAX_BODY_BYTES) + CONTROL_FRAME_HEADER_BYTES as u64;
    let mut sink = [0_u8; 8192];
    let mut drained = 0_u64;
    while drained < cap {
        match timeout(DRAIN_READ_TIMEOUT, stream.read(&mut sink)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => drained += n as u64,
        }
    }
}
