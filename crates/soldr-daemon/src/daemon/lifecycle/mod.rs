//! PID-file based lifecycle for soldr-daemon: detect a live daemon,
//! spawn one detached when none is found, append structured JSONL
//! lifecycle events.
//!
//! The PID file stores two lines: the decimal PID and the absolute path
//! to the daemon executable. Readers verify both — the file is only
//! authoritative if the PID is alive AND its exe stem is
//! `soldr-daemon`. Defends against recycled PIDs the way zccache does.

mod relocate;
mod spawn;
mod spawn_env;
pub use relocate::{
    current_process_is_declared_daemon, reexec_from_runtime_root,
    reexec_from_runtime_root_for_daemon_entry,
};
pub(crate) use spawn::*;
pub(crate) use spawn_env::*;

use crate::cache_lib::{daemon_lifecycle_log_path, daemon_pid_path, soldr_daemon_dir};
use crate::core::SoldrPaths;
use fs2::FileExt;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Private parent-to-wrapper handoff for the canonical daemon executable.
///
/// Cargo invokes Soldr through compiler-named hardlinks such as `rustc`.
/// Spawning a long-lived daemon from that `current_exe()` gives the process a
/// `rustc` executable identity, which the PID-recycling safety gate must reject.
/// The Cargo front door therefore materializes a `soldr-daemon` alias and
/// passes its absolute path through this variable.
pub const SOLDR_DAEMON_EXE_ENV_VAR: &str = "SOLDR_INTERNAL_DAEMON_EXE";

#[derive(Debug)]
pub enum LifecycleError {
    Io(std::io::Error),
    NoExe,
    Spawn(std::io::Error),
}

const ROOT_OWNER_LOCK_NAME: &str = "root-owner.lock";

/// Version-independent ownership for one product root. The daemon holds this
/// for its whole lifetime; explicit orphan-root maintenance uses the same lock
/// so startup and manual deletion cannot race even across protocol versions.
pub struct RootOwnershipGuard {
    file: File,
}

impl RootOwnershipGuard {
    pub fn try_acquire(paths: &SoldrPaths) -> std::io::Result<Option<Self>> {
        let dir = soldr_daemon_dir(paths);
        fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join(ROOT_OWNER_LOCK_NAME))?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if crate::cache_lib::cargo_lock::lock_is_held(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl Drop for RootOwnershipGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl From<std::io::Error> for LifecycleError {
    fn from(e: std::io::Error) -> Self {
        LifecycleError::Io(e)
    }
}

/// Consecutive observations of a missing image before the daemon gives up.
///
/// Hysteresis is the whole safety argument. A daemon that exits the first time
/// `current_exe()` fails to resolve -- a momentarily unavailable network path,
/// a slow removable volume, an antivirus lock -- is strictly worse than the
/// orphan it prevents: it would take down healthy daemons for transient
/// reasons. Three consecutive maintenance ticks is minutes of sustained
/// absence, which no transient survives and a deleted image never recovers
/// from.
pub const DAEMON_IMAGE_MISSING_STRIKES: u32 = 3;

/// Does this process's own executable still exist on disk?
///
/// `None` when the path cannot be determined at all, which is not evidence of
/// deletion and must not be counted as a strike.
pub fn daemon_image_present() -> Option<bool> {
    std::env::current_exe().ok().map(|exe| exe.exists())
}

/// Tracks sustained absence of the daemon's own image (soldr#1987).
///
/// A daemon spawned from a temp directory that is later deleted -- a `uv` or
/// `pip` build does exactly this -- keeps running and keeps the root-ownership
/// lock, so every later spawn fails `AddrInUse` and every compile on the
/// machine degrades to direct rustc. On the reporting host that lasted 28
/// hours. The orphan cannot be reached by `soldr daemon stop`, which probes
/// the pipe while the orphan holds the filesystem lock, so nothing external
/// can clear it.
///
/// A daemon whose own executable is gone can never again be a legitimate
/// owner: it cannot be upgraded, restarted in place, or verified by the
/// PID-recycling identity gate. Standing down is the only correct response.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MissingImageDetector {
    consecutive: u32,
}

impl MissingImageDetector {
    /// Feed one observation. Returns true exactly once, on the strike that
    /// confirms sustained absence.
    ///
    /// `None` (path unknown) resets rather than accumulates: not knowing where
    /// our image is says nothing about whether it exists.
    pub fn observe(&mut self, present: Option<bool>) -> bool {
        match present {
            Some(false) => {
                self.consecutive = self.consecutive.saturating_add(1);
                self.consecutive == DAEMON_IMAGE_MISSING_STRIKES
            }
            // A single successful sighting clears the count: the condition
            // this detects is permanent, so any recovery proves it was not it.
            Some(true) | None => {
                self.consecutive = 0;
                false
            }
        }
    }

    pub fn strikes(&self) -> u32 {
        self.consecutive
    }
}

/// Explain who owns the soldr root when acquisition fails.
///
/// soldr#1987: a daemon spawned from a temp directory that is later deleted --
/// a `uv`/`pip` build does exactly this -- keeps holding the root lock. Every
/// later spawn fails `AddrInUse`, so every compile silently degrades to direct
/// rustc. On the reporting host that ran for 28 hours and 246/246 compiles,
/// visible only as `compiler cache unavailable` with nothing naming a cause.
///
/// The orphan is also unreachable by `soldr daemon stop`, which probes the
/// pipe while the orphan holds only the filesystem lock. So the message has to
/// carry enough to act on: the PID, and whether its image still exists --
/// because a holder whose executable is gone cannot be a legitimate owner.
///
/// Deliberately hedged. The PID file records the daemon that last wrote it,
/// which is *probably* the lock holder but is not proven to be, so the wording
/// says "recorded" rather than asserting identity. Naming a plausible suspect
/// beats today's silence; claiming certainty we do not have would be worse.
pub fn describe_root_ownership_conflict(paths: &SoldrPaths) -> String {
    let root = paths.root.display();
    let Some((pid, exe)) = read_pid_file(paths) else {
        return format!(
            "soldr root ownership is busy: {root} (no daemon PID file to name the owner)"
        );
    };
    let alive = pid_is_alive(pid);
    let image_exists = exe.exists();
    match (alive, image_exists) {
        (true, false) => format!(
            "soldr root ownership is busy: {root}
             soldr: the recorded daemon is PID {pid}, whose image no longer exists ({}).
             soldr: an orphan like this holds the lock indefinitely and cannot be reached by              `soldr daemon stop`, which probes the pipe rather than the lock (soldr#1987).
             soldr: terminate PID {pid} to recover.",
            exe.display()
        ),
        (true, true) => format!(
            "soldr root ownership is busy: {root} (held by PID {pid}, image {})",
            exe.display()
        ),
        (false, _) => format!(
            "soldr root ownership is busy: {root} (recorded PID {pid} is not alive;              the lock is held by an unrecorded process)"
        ),
    }
}

/// Read the PID file. Returns `(pid, exe_path)` if the file is well
/// formed; absent / malformed reads return None. **Does not** verify
/// liveness — that's `is_live`.
pub fn read_pid_file(paths: &SoldrPaths) -> Option<(u32, PathBuf)> {
    let raw = fs::read_to_string(daemon_pid_path(paths)).ok()?;
    let mut lines = raw.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let exe = lines.next()?.trim();
    if exe.is_empty() {
        return None;
    }
    Some((pid, PathBuf::from(exe)))
}

/// Verify the daemon recorded in the PID file is still alive AND its
/// running exe stem looks like a soldr-daemon. Returns the PID on
/// success, None on any mismatch / missing file.
pub fn is_live(paths: &SoldrPaths) -> Option<u32> {
    is_live_with_running_process_disabled(
        paths,
        crate::daemon::backend_handle_adoption::running_process_disabled(),
    )
}

pub(crate) fn is_live_with_running_process_disabled(
    paths: &SoldrPaths,
    running_process_disabled: bool,
) -> Option<u32> {
    if running_process_disabled {
        return direct_pid_file_live(paths);
    }

    // Full v1 broker adoption (zackees/running-process#434): try broker
    // discovery first. The broker negotiates a verified backend endpoint via a
    // Hello handshake. When the broker is unreachable, refuses, or is disabled,
    // `broker_discovery::soldr_daemon_pid_via_broker` returns None and we fall
    // through to the existing direct `BackendHandle` probe — keeping the direct
    // soldr-daemon path active during the rollout window.
    crate::daemon::broker_discovery::soldr_daemon_pid_via_broker(paths)
        .or_else(|| direct_backend_handle_probe(paths))
}

/// The pre-#434 direct discovery path: probe the local PID file's recorded
/// daemon with the `running-process` `BackendHandle` nonce challenge. Kept as
/// the fall-through when broker discovery does not resolve a backend.
pub(crate) fn direct_backend_handle_probe(paths: &SoldrPaths) -> Option<u32> {
    crate::daemon::backend_handle_adoption::probe_soldr_daemon(paths).map(|handle| handle.pid())
}

pub(crate) fn direct_pid_file_live(paths: &SoldrPaths) -> Option<u32> {
    let (pid, _exe_path) = read_pid_file(paths)?;
    pid_is_soldr_daemon(pid).then_some(pid)
}

fn direct_pid_file_live_for_stem(paths: &SoldrPaths, expected_stem: &str) -> Option<u32> {
    let (pid, _exe_path) = read_pid_file(paths)?;
    if pid_is_alive(pid) && pid_exe_stem_matches(pid, expected_stem) {
        Some(pid)
    } else {
        None
    }
}

/// Env escape hatch (soldr#1495): set `SOLDR_DAEMON_DISPLACE=off` (or
/// `0`/`false`/`no`) to disable displacement of a stale-version daemon
/// and revert to the pre-#1495 "first daemon wins" behavior.
pub(crate) const SOLDR_DAEMON_DISPLACE_ENV: &str = "SOLDR_DAEMON_DISPLACE";

/// Normal upper bound for an acknowledged graceful shutdown.
///
/// Embedded cache persistence may legitimately take minutes on a large or
/// slow cache. Once a daemon acknowledges shutdown callers wait for that exact
/// generation and never convert this deadline into permission to signal it.
pub const GRACEFUL_SHUTDOWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How often [`wait_for_shutdown_responder`] reports that it is still
/// waiting (soldr#1838). Matches the compile-reply and cargo front-door
/// cadence so every long wait in soldr ticks at the same rate.
const SHUTDOWN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

pub(crate) fn displacement_enabled() -> bool {
    match std::env::var(SOLDR_DAEMON_DISPLACE_ENV) {
        Ok(v) => {
            let v = v.trim();
            !(v.eq_ignore_ascii_case("off")
                || v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no"))
        }
        Err(_) => true,
    }
}

/// True when the running daemon's published claim matches this build's
/// identity — package version **and** vendored zccache version, since the
/// latter is what versions the embedded store (soldr#2186; see
/// `broker_discovery::daemon_identity_claim`).
///
/// A missing claim (a pre-#1495 daemon that never wrote a manifest) is
/// treated as a mismatch — version-unknown is stale — so a newer client
/// always converges to a daemon it can name. A daemon claiming only a bare
/// package version, i.e. one built before this change, is stale for the
/// same reason, and is displaced once on first contact.
pub(crate) fn current_version_claim_matches(paths: &SoldrPaths) -> bool {
    crate::daemon::broker_discovery::read_claimed_service_version(paths)
        .is_some_and(|claimed| claimed == env!("CARGO_PKG_VERSION"))
        && crate::daemon::broker_discovery::store_version_claim_matches(paths)
}

/// Version-aware liveness: the daemon is live (passes the existing PID +
/// nonce/IPC probe, which already rejects a different `PROTOCOL_VERSION`)
/// **and** its package-version claim matches this build. Returns the PID
/// only when both hold. Used by the spawn path and the managed-build
/// preflight to decide whether the running daemon is the one this client
/// wants, or a stale version to displace.
pub fn is_live_current_version(paths: &SoldrPaths) -> Option<u32> {
    let pid = is_live(paths)?;
    current_version_claim_matches(paths).then_some(pid)
}

fn is_live_current_version_direct(paths: &SoldrPaths) -> Option<u32> {
    let pid = direct_pid_file_live(paths)?;
    current_version_claim_matches(paths).then_some(pid)
}

fn direct_status_current_version(paths: &SoldrPaths) -> Option<u32> {
    let pid = direct_pid_file_live(paths)?;
    let sock = crate::daemon::client::default_sock_path(paths);
    let status_pid = crate::daemon::client::status(&sock).ok()?.pid;
    preflight_identity_matches(
        Some(pid),
        Some(status_pid),
        current_version_claim_matches(paths),
    )
    .then_some(pid)
}

fn preflight_identity_matches(
    recorded_live_pid: Option<u32>,
    status_pid: Option<u32>,
    current_version_claim: bool,
) -> bool {
    current_version_claim && recorded_live_pid.is_some() && recorded_live_pid == status_pid
}

/// Version-blind occupancy check: is the singleton endpoint held by a
/// live soldr daemon process at all? Accepts both the sibling
/// `soldr-daemon` binary and the via-self `soldr` form (CI / slim
/// deployments spawn `soldr daemon start`). Returns its PID. This is the
/// signal that a stale daemon must be *displaced* before a new one can
/// bind — distinct from `is_live` (which additionally requires the IPC
/// probe to succeed, and so misses a protocol-mismatched daemon that is
/// nonetheless holding the socket).
pub fn stale_daemon_occupies_endpoint(paths: &SoldrPaths) -> Option<u32> {
    let (pid, _exe) = read_pid_file(paths)?;
    pid_is_soldr_daemon(pid).then_some(pid)
}

/// PID-recycling-safe identity gate for a signalled kill: the PID must be
/// alive and its running image must look like one of soldr's own daemon
/// process names. A recycled PID running an unrelated program fails the
/// stem check, so we never signal a stranger.
fn pid_is_soldr_daemon(pid: u32) -> bool {
    pid_is_alive(pid)
        && (pid_exe_stem_matches(
            pid,
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME,
        ) || pid_exe_stem_matches(pid, "soldr"))
}

/// Result of waiting for one acknowledged daemon generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownWaitOutcome {
    /// The acknowledged PID is no longer alive.
    Exited,
    /// The endpoint now reports a different daemon generation.
    Replaced,
    /// The acknowledged generation could not be proven gone in time.
    TimedOut,
}

impl ShutdownWaitOutcome {
    pub fn is_complete(self) -> bool {
        matches!(self, Self::Exited | Self::Replaced)
    }
}

fn classify_shutdown_observation(
    responder: crate::daemon::protocol::ShutdownAck,
    responder_pid_alive: bool,
    endpoint_identity: Option<(u32, u64)>,
) -> Option<ShutdownWaitOutcome> {
    if !responder_pid_alive {
        return Some(ShutdownWaitOutcome::Exited);
    }
    endpoint_identity
        .filter(|identity| *identity != (responder.pid, responder.generation))
        .map(|_| ShutdownWaitOutcome::Replaced)
}

/// Wait for the exact daemon that acknowledged shutdown.
///
/// Endpoint unavailability alone is not success: the accept loop stops before
/// the cache flush does. Conversely, a different status generation proves the
/// acknowledged responder was replaced even if its PID was reused.
pub fn wait_for_shutdown_responder(
    sock_path: &Path,
    responder: crate::daemon::protocol::ShutdownAck,
    timeout: Duration,
) -> ShutdownWaitOutcome {
    let started = Instant::now();
    // soldr#1838: this is the wait that #1828's macOS zombie-pid bug sat in
    // for the full 5 minutes with a single line printed *after* it expired.
    // It already polls, so the heartbeat is an in-loop check rather than the
    // watchdog thread the blocking IPC waits need.
    let mut next_heartbeat = SHUTDOWN_HEARTBEAT_INTERVAL;
    loop {
        if started.elapsed() >= next_heartbeat {
            eprintln!(
                "{}",
                crate::daemon::wait_heartbeat::heartbeat_message(
                    "daemon graceful shutdown",
                    started.elapsed(),
                    timeout,
                    None,
                )
            );
            next_heartbeat += SHUTDOWN_HEARTBEAT_INTERVAL;
        }
        let responder_pid_alive = pid_is_alive(responder.pid);
        if timeout.is_zero() || started.elapsed() >= timeout {
            return classify_shutdown_observation(responder, responder_pid_alive, None)
                .unwrap_or(ShutdownWaitOutcome::TimedOut);
        }
        let endpoint_identity = crate::daemon::client::status(sock_path)
            .ok()
            .map(|status| (status.pid, status.generation));
        if let Some(outcome) =
            classify_shutdown_observation(responder, responder_pid_alive, endpoint_identity)
        {
            return outcome;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return ShutdownWaitOutcome::TimedOut;
        }
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    }
}

/// Displace the stale daemon currently holding the endpoint so a
/// current-version daemon can take over (soldr#1495). Graceful IPC is always
/// attempted first, including for historical daemons whose executable was
/// accidentally named `rustc`. A verified-PID signal is permitted only when
/// no shutdown acknowledgement was received. Once acknowledged, the daemon
/// owns its graceful flush to completion and is never force-killed.
/// `source` attributes the request, so two `displace-kill-fallback`
/// records from different entry points stay distinguishable (soldr#1808).
pub fn displace_stale_daemon(paths: &SoldrPaths, source: Option<LifecycleSource>) -> bool {
    let verified_pid = stale_daemon_occupies_endpoint(paths);
    let recorded_pid = read_pid_file(paths).map(|(pid, _)| pid);
    let recorded_live_pid = recorded_pid.filter(|pid| pid_is_alive(*pid));
    append_lifecycle_event_with(
        paths,
        "displace-stale-requested",
        LifecycleDetails::requested(LifecycleReason::StaleVersion).from_current_process(source),
    );

    let sock = crate::daemon::client::default_sock_path(paths);
    match crate::daemon::client::shutdown(&sock) {
        Ok(responder) => {
            return wait_for_shutdown_responder(&sock, responder, Duration::from_secs(5))
                .is_complete();
        }
        Err(crate::daemon::client::ClientError::NotRunning) if recorded_live_pid.is_none() => {
            append_lifecycle_event_with(
                paths,
                "previous-daemon-vanished-without-ack",
                LifecycleDetails::vanished_without_ack(recorded_pid, LifecycleReason::StaleVersion)
                    .from_current_process(source),
            );
            return true;
        }
        Err(_) => {}
    }

    // No acknowledgement: a protocol-mismatched daemon can only be stopped
    // through a currently verified Soldr process identity.
    let Some(pid) = verified_pid else {
        return false;
    };
    if !pid_is_soldr_daemon(pid) {
        if !pid_is_alive(pid) {
            append_lifecycle_event_with(
                paths,
                "previous-daemon-vanished-without-ack",
                LifecycleDetails::vanished_without_ack(
                    Some(pid),
                    LifecycleReason::ProtocolMismatch,
                )
                .from_current_process(source),
            );
            return true;
        }
        return false;
    }

    // Reached only after the shutdown request produced no acknowledgement,
    // which is the protocol-mismatch case the block above describes.
    terminate_pid(pid, None);
    let exited = wait_for_pid_exit(pid, Duration::from_secs(5));
    append_lifecycle_event_with(
        paths,
        "displace-kill-fallback",
        LifecycleDetails::forced(pid, LifecycleReason::ProtocolMismatch)
            .with_outcome(if exited {
                LifecycleOutcome::Forced
            } else {
                LifecycleOutcome::Failed
            })
            .from_current_process(source),
    );
    exited
}

/// One-shot preflight for the managed-build front door (soldr#1495).
/// Mode 1 — a same-`PROTOCOL_VERSION` older release serving our compiles
/// — never fails the hot path, so `try_spawn_detached`'s displacement is
/// never reached during a build. Run this once at `soldr cargo` startup:
/// if a stale-version daemon holds the endpoint, displace it here so the
/// build's first wrapper call spawns a current-version daemon. A no-op
/// when displacement is disabled or the running daemon is already current.
pub fn preflight_displace_stale_daemon(paths: &SoldrPaths) {
    if !displacement_enabled() {
        return;
    }
    // Regression for #1832: use Soldr's bounded direct status handshake here.
    // The general `is_live_current_version` route probes both the optional
    // broker and a full executable identity hash; together those added tens
    // of seconds to every warm cargo invocation. Requiring the status PID to
    // match the live PID-file process prevents PID reuse from accepting an
    // unrelated same-stem process, while the normal status wire exchange
    // still proves that the current-protocol daemon owns the endpoint.
    if direct_status_current_version(paths).is_some() {
        return;
    }
    // Issue #1865: a `None` above is ambiguous — it means either "this daemon
    // is a stale version" or "the status probe did not finish inside
    // REPLY_TIMEOUT". Treating both as staleness displaced healthy daemons
    // that were merely too busy to answer a 2 s ping, which is how a build
    // could lose its warm daemon mid-run (and, per #1814, briefly end up with
    // two daemons contending for state.redb).
    //
    // Require positive evidence instead. A process that is alive, looks like
    // one of our daemons, and publishes *this exact* version claim cannot be
    // the stale-version daemon this preflight exists to displace, however
    // unresponsive it currently is. `is_live_current_version_direct` is the
    // right predicate and costs no IPC, so this stays latency-neutral for the
    // warm path #1832 tuned.
    //
    // Trade-off: a genuinely wedged current-version daemon is no longer
    // displaced *here*. That is outside preflight's remit (its job is version
    // skew) and is covered by the wedge/recovery ladder — and killing a
    // busy-but-healthy daemon is strictly worse than leaving it alone.
    let claim_proves_current = is_live_current_version_direct(paths);
    let recorded_process_is_alive = read_pid_file(paths).is_some_and(|(pid, _)| pid_is_alive(pid));
    #[cfg(unix)]
    let endpoint_artifact_exists = crate::cache_lib::daemon_sock_path(paths).exists();
    #[cfg(windows)]
    let endpoint_artifact_exists = false;
    if preflight_should_displace(
        claim_proves_current.is_some(),
        stale_daemon_occupies_endpoint(paths).is_some(),
        recorded_process_is_alive,
        endpoint_artifact_exists,
    ) {
        displace_stale_daemon(paths, Some(LifecycleSource::Preflight));
    } else if let Some(pid) = claim_proves_current {
        tracing::warn!(
            event = "preflight_displacement_declined",
            pid,
            "status probe did not answer, but the daemon is alive and claims the \
             current version — not displacing it (issue #1865)"
        );
    }
}

/// Pure policy for [`preflight_displace_stale_daemon`], so the
/// probe-failed-vs-actually-stale matrix is unit-testable without real
/// daemons or real timeouts (issue #1865).
///
/// `claim_proves_current` is the evidence that closes the #1865 hole: the
/// caller has already failed to get a status answer, and this says whether the
/// PID-file process is nonetheless alive, one of ours, and publishing this
/// exact version. When it is, no amount of endpoint occupancy justifies a
/// displacement — the daemon is current, just unresponsive right now.
fn preflight_should_displace(
    claim_proves_current: bool,
    stale_daemon_occupies: bool,
    recorded_process_is_alive: bool,
    endpoint_artifact_exists: bool,
) -> bool {
    if claim_proves_current {
        return false;
    }
    stale_daemon_occupies || recorded_process_is_alive || endpoint_artifact_exists
}

fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_is_alive(pid) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    }
    !pid_is_alive(pid)
}

/// Wait until a daemon PID is no longer alive. A zero timeout is an
/// instantaneous observation. Callers capture the PID before sending the
/// shutdown request so removal of the PID file cannot be mistaken for process
/// exit.
pub fn wait_for_daemon_exit(pid: u32, timeout: Duration) -> bool {
    wait_for_pid_exit(pid, timeout)
}

#[cfg(unix)]
fn terminate_pid(pid: u32, deadline: Option<Instant>) {
    // SAFETY: kill(2) with SIGTERM then (if needed) SIGKILL. The PID was
    // just verified alive + soldr-daemon by the caller.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let grace = deadline
        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(Duration::from_secs(3))
        .min(Duration::from_secs(3));
    if grace.is_zero() || wait_for_pid_exit(pid, grace) {
        return;
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return;
    }
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(windows)]
#[allow(non_snake_case)]
fn terminate_pid(pid: u32, _deadline: Option<Instant>) {
    use std::os::windows::raw::HANDLE;
    // Win32 API spelling — clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    const PROCESS_TERMINATE: DWORD = 0x0001;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn TerminateProcess(h: HANDLE, exit_code: DWORD) -> BOOL;
        fn CloseHandle(h: HANDLE) -> BOOL;
    }
    // SAFETY: OpenProcess for a verified soldr-daemon PID; TerminateProcess
    // is the Windows equivalent of SIGKILL (the daemon holds no
    // filesystem lock we need graceful about. Stale shared claims are
    // intentionally reclaimed only by the next startup, never by a retiring
    // daemon after a check-then-unlink race).
    let h = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if h.is_null() {
        return;
    }
    unsafe {
        TerminateProcess(h, 1);
        CloseHandle(h);
    }
}

/// Write the PID file for the running daemon. Overwrites any stale
/// content. Caller is responsible for ensuring `soldr_daemon_dir`
/// exists.
pub fn write_pid_file(paths: &SoldrPaths) -> Result<(), LifecycleError> {
    let exe = std::env::current_exe().map_err(|_| LifecycleError::NoExe)?;
    fs::create_dir_all(soldr_daemon_dir(paths))?;
    let pid = std::process::id();
    let contents = format!("{pid}\n{}\n", exe.display());
    fs::write(daemon_pid_path(paths), contents)?;
    Ok(())
}

/// Why a lifecycle transition happened.
///
/// soldr#1808 asks for typed details rather than event names assembled by
/// concatenating free-form values: `event` stays a stable identifier readers
/// can match on, and the circumstances travel in their own fields.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleReason {
    /// An operator or wrapper asked the daemon to stop.
    ExplicitStop,
    /// The running daemon does not claim the current package version.
    StaleVersion,
    /// The daemon could not be reached over IPC at a compatible version, so
    /// no graceful shutdown was possible.
    ProtocolMismatch,
    /// A spawn deadline expired while a stale daemon still held the endpoint.
    StartupDeadline,
}

/// Which entry point asked for a lifecycle transition.
///
/// soldr#1808 wants displacement attributable. Two records that both say
/// `displace-kill-fallback` are indistinguishable without this: one may be a
/// build's own preflight clearing a stale-version daemon, the other an
/// operator running `soldr daemon stop`. They have different causes and
/// different remedies.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleSource {
    /// A user-invoked command.
    Cli,
    /// The per-build preflight that clears a stale-version daemon.
    Preflight,
    /// The OS-observed peer on an accepted IPC connection.
    IpcPeer,
    /// The transport did not expose a trustworthy peer identity.
    Unknown,
}

/// How the transition ended.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleOutcome {
    /// Asked for; the result was not yet known when the record was written.
    Requested,
    /// The daemon acknowledged and owns its graceful flush to completion.
    Acknowledged,
    /// Terminated by signal after no acknowledgement arrived.
    Forced,
    /// The process was already gone before the transition completed.
    VanishedWithoutAck,
    /// Attempted, and did not succeed.
    Failed,
}

/// Optional attribution attached to a lifecycle record.
///
/// Every field is skipped when absent, so a record carrying no details
/// serializes byte-identically to the pre-soldr#1808 three-field shape --
/// including for the substring-matching reader in
/// `tests/cli_daemon_lifecycle.rs`, which looks for `"event":"spawn"`.
#[derive(Serialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct LifecycleDetails {
    /// OS-observed process that requested the transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_pid: Option<u32>,
    /// Best-effort executable path for `requester_pid`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_exe: Option<String>,
    /// The process acted upon, when it differs from the recording process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pid: Option<u32>,
    /// Target daemon generation, when an IPC response made it knowable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_generation: Option<u64>,
    /// Which entry point asked for this transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester_source: Option<LifecycleSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<LifecycleReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<LifecycleOutcome>,
}

impl LifecycleDetails {
    /// A forced termination of `target_pid`, for `reason`.
    pub fn forced(target_pid: u32, reason: LifecycleReason) -> Self {
        Self {
            requester_pid: None,
            requester_exe: None,
            target_pid: Some(target_pid),
            target_generation: None,
            requester_source: None,
            reason: Some(reason),
            outcome: Some(LifecycleOutcome::Forced),
        }
    }

    /// Attribute this record to the entry point that asked for it.
    pub fn from_source(mut self, source: Option<LifecycleSource>) -> Self {
        self.requester_source = source;
        self
    }

    /// Attribute this record to an OS-observed IPC peer.
    pub(crate) fn with_peer(mut self, peer: crate::daemon::ipc_peer::PeerIdentity) -> Self {
        self.requester_pid = peer.pid;
        self.requester_exe = peer.exe;
        self.requester_source = Some(peer.source);
        self
    }

    /// Attribute a local displacement to the process performing it.
    pub fn from_current_process(mut self, source: Option<LifecycleSource>) -> Self {
        self.requester_pid = Some(std::process::id());
        self.requester_exe = std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        self.requester_source = Some(source.unwrap_or(LifecycleSource::Unknown));
        self
    }

    /// Name the daemon generation receiving a graceful IPC request.
    pub(crate) fn for_target_generation(mut self, pid: u32, generation: u64) -> Self {
        self.target_pid = Some(pid);
        self.target_generation = Some(generation);
        self
    }

    /// Override the result after observing what actually happened.
    pub fn with_outcome(mut self, outcome: LifecycleOutcome) -> Self {
        self.outcome = Some(outcome);
        self
    }

    /// A previous daemon disappeared before acknowledging shutdown.
    pub fn vanished_without_ack(target_pid: Option<u32>, reason: LifecycleReason) -> Self {
        Self {
            target_pid,
            reason: Some(reason),
            outcome: Some(LifecycleOutcome::VanishedWithoutAck),
            ..Self::default()
        }
    }

    /// A transition that has been asked for but not yet resolved.
    pub fn requested(reason: LifecycleReason) -> Self {
        Self {
            reason: Some(reason),
            outcome: Some(LifecycleOutcome::Requested),
            ..Self::default()
        }
    }
}

#[derive(Serialize)]
struct LifecycleEvent<'a> {
    ts_ms: i64,
    pid: u32,
    event: &'a str,
    #[serde(flatten)]
    details: LifecycleDetails,
}

/// Record a lifecycle event carrying no attribution.
pub fn append_lifecycle_event(paths: &SoldrPaths, event: &str) {
    append_lifecycle_event_with(paths, event, LifecycleDetails::default());
}

/// Record a lifecycle event with typed attribution (soldr#1808).
pub fn append_lifecycle_event_with(paths: &SoldrPaths, event: &str, details: LifecycleDetails) {
    let pid = std::process::id();
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let line = match serde_json::to_string(&LifecycleEvent {
        ts_ms,
        pid,
        event,
        details,
    }) {
        Ok(s) => s,
        Err(_) => return,
    };
    let path = daemon_lifecycle_log_path(paths);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
}

/// Attempt to spawn a detached `soldr-daemon`. Resolves the daemon
/// binary as a sibling of the current `soldr` executable, **relocates
/// it into `~/.soldr/runtime/soldr-daemon/<hash>/`** so the long-lived
/// daemon doesn't hold a file lock on the original (worktree target/,
/// site-packages, package upgrade, etc.) — and only then spawns.
/// Mirrors the pattern `self_relocate.rs` already uses for `soldr`
/// itself, sharing the same hash / lock / periodic-GC machinery via a
/// sibling `runtime/soldr-daemon/` sub-tree.
///
/// Spawn-herd safety (issue #474): when a `soldr cargo build` fans out
/// hundreds of parallel rustc invocations, EVERY wrapper sees the
/// daemon missing simultaneously and races to spawn it. To keep that
/// from forking N children, we take an OS-level non-blocking exclusive
/// lock on `<cache>/soldr-daemon/.spawn.lock` before relocating /
/// spawning. Lock losers re-check liveness (the lock holder is racing
/// `write_pid_file`) and short-circuit with `Ok(())` once the daemon
/// shows up. If the lock is unobtainable AND no daemon ever appears
/// within a short window, the loser still returns `Ok(())` — the next
/// wrapper invocation will reprobe and try again.
///
/// Best-effort: returns Ok(()) on spawn success, Err otherwise. The
/// spawn owner keeps the herd lock briefly after spawn while waiting
/// for the daemon endpoint to become live; if readiness still races or
/// times out, callers must keep using their normal retry budget.
#[derive(Clone, Debug)]
pub struct PreparedDaemonSpawn {
    executable: PathBuf,
    via_self: bool,
    idle_timeout_secs: Option<u64>,
}

pub fn try_spawn_detached() -> Result<(), LifecycleError> {
    try_spawn_detached_until_with_idle_timeout(None, None).map(|_| ())
}

/// Spawn the managed daemon with an explicit inactivity timeout.
///
/// A value of zero preserves the normal long-lived daemon behavior.
pub fn try_spawn_detached_with_idle_timeout(idle_timeout_secs: u64) -> Result<(), LifecycleError> {
    let idle_timeout_secs = (idle_timeout_secs != 0).then_some(idle_timeout_secs);
    try_spawn_detached_until_with_idle_timeout(None, idle_timeout_secs).map(|_| ())
}

/// Spawn a daemon while honoring an optional absolute startup deadline.
///
/// On a successful owned spawn, returns the already-relocated image so a
/// caller can retry after an early child death without hashing or relocating
/// the executable again.
pub fn try_spawn_detached_until(
    deadline: Option<Instant>,
) -> Result<Option<PreparedDaemonSpawn>, LifecycleError> {
    try_spawn_detached_until_with_idle_timeout(deadline, None)
}

fn try_spawn_detached_until_with_idle_timeout(
    deadline: Option<Instant>,
    idle_timeout_secs: Option<u64>,
) -> Result<Option<PreparedDaemonSpawn>, LifecycleError> {
    ensure_startup_deadline_remaining(deadline)?;
    let current = std::env::current_exe().map_err(|_| LifecycleError::NoExe)?;
    // Prefer the sibling `soldr-daemon` binary (dev builds + maturin
    // wheels ship both). Fall back to the running soldr binary itself
    // invoked as `soldr daemon start --foreground` when the sibling
    // isn't present — this lets CI workflows and slimmed-down
    // deployments (which historically distributed only `soldr`) still
    // bring up the daemon now that Phase 5/7 made the embedded
    // backend mandatory. The daemon subcommand is already a clap-
    // matched verb in `cli_args.rs`; the `soldr-daemon` argv[0] alias
    // routes through the main binary.
    let configured = configured_daemon_executable(std::env::var_os(SOLDR_DAEMON_EXE_ENV_VAR));
    let sibling = crate::daemon::service_definition::sibling_daemon_binary(&current);
    let (daemon_src, daemon_via_self) = if let Some(configured) = configured {
        (configured, false)
    } else if sibling.exists() {
        (sibling, false)
    } else if executable_has_stem(&current, "soldr") {
        (current.clone(), true)
    } else {
        return Err(LifecycleError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to spawn soldr-daemon from compiler-named executable {}; \
                 the caller must provide a canonical image through {SOLDR_DAEMON_EXE_ENV_VAR}",
                current.display()
            ),
        )));
    };

    let paths = SoldrPaths::new().ok();
    let _spawn_lock = paths.as_ref().and_then(acquire_spawn_lock);
    // Re-check liveness while holding the lock (or after failing to
    // acquire it): if a daemon of THIS version already brought the
    // endpoint up we short-circuit before doing relocate + spawn.
    // soldr#1495: if a *stale-version* daemon holds the endpoint instead,
    // displace it (graceful shutdown → verified-PID kill) so our spawn
    // can bind — this is what breaks the version shadow, both for a
    // silently-serving older release and a protocol-mismatched daemon.
    if let Some(p) = paths.as_ref() {
        let current_is_live = if deadline.is_some() {
            is_live_current_version_direct(p).is_some()
        } else {
            is_live_current_version(p).is_some()
        };
        if current_is_live {
            return Ok(None);
        }
        if displacement_enabled() && stale_daemon_occupies_endpoint(p).is_some() {
            if let Some(deadline) = deadline {
                displace_stale_daemon_before(p, deadline);
            } else {
                displace_stale_daemon(p, Some(LifecycleSource::Preflight));
            }
        }
    }
    // Without the lock, another wrapper is currently mid-spawn. Don't
    // pile on — the next wrapper will reprobe.
    if paths.is_some() && _spawn_lock.is_none() {
        return Ok(None);
    }

    ensure_startup_deadline_remaining(deadline)?;
    // Compile-dispatch recovery is deadline-sensitive. Its source is already
    // a stable soldr/soldr-daemon runtime image, so avoid synchronous hashing,
    // relocation, and runtime GC here. Normal daemon startup still takes the
    // durable relocated path below.
    let executable = if deadline.is_some() {
        daemon_src
    } else {
        resolve_daemon_spawn_image(paths.as_ref(), &daemon_src)
    };
    let prepared = PreparedDaemonSpawn {
        executable,
        via_self: daemon_via_self,
        idle_timeout_secs,
    };

    spawn_prepared_daemon(&prepared, paths.as_ref(), deadline)?;
    Ok(Some(prepared))
}

fn configured_daemon_executable(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    (path.is_file()
        && executable_has_stem(
            &path,
            crate::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_NAME,
        ))
    .then_some(path)
}

fn executable_has_stem(path: &Path, expected: &str) -> bool {
    path.file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|stem| stem.eq_ignore_ascii_case(expected))
}

/// Retry a daemon spawn from an image prepared by
/// [`try_spawn_detached_until`]. This path deliberately skips executable
/// discovery, hashing, and relocation.
pub fn try_spawn_detached_prepared_until(
    prepared: &PreparedDaemonSpawn,
    deadline: Instant,
) -> Result<(), LifecycleError> {
    let paths = SoldrPaths::new().ok();
    let _spawn_lock = paths.as_ref().and_then(acquire_spawn_lock);
    if let Some(p) = paths.as_ref() {
        if is_live_current_version_direct(p).is_some() {
            return Ok(());
        }
        if displacement_enabled() && stale_daemon_occupies_endpoint(p).is_some() {
            displace_stale_daemon_before(p, deadline);
        }
    }
    if paths.is_some() && _spawn_lock.is_none() {
        return Ok(());
    }

    spawn_prepared_daemon(prepared, paths.as_ref(), Some(deadline))
}

fn spawn_prepared_daemon(
    prepared: &PreparedDaemonSpawn,
    paths: Option<&SoldrPaths>,
    deadline: Option<Instant>,
) -> Result<(), LifecycleError> {
    ensure_startup_deadline_remaining(deadline)?;

    if deadline.is_none()
        && !prepared.via_self
        && !crate::daemon::backend_handle_adoption::running_process_disabled()
    {
        let _ = crate::daemon::service_definition::install_service_definition(&prepared.executable);
    }
    let args = detached_spawn_args(prepared.via_self, prepared.idle_timeout_secs);
    let spawn_result = if prepared.via_self {
        spawn_detached_self_inner(&prepared.executable, &args).map_err(LifecycleError::Spawn)
    } else {
        spawn_detached_inner(&prepared.executable, &args).map_err(LifecycleError::Spawn)
    };
    spawn_result?;

    // Keep the spawn lock held until the daemon has written its PID file and
    // answered the active endpoint probe. Without this, a cargo fan-out on
    // Windows can acquire the lock sequentially in several rustc-wrapper
    // processes and spawn multiple `soldr daemon start --foreground` children
    // before the first one is ready.
    if let Some(paths) = paths {
        let readiness_timeout = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_secs(5))
            .min(Duration::from_secs(5));
        if !readiness_timeout.is_zero() {
            if deadline.is_some() {
                wait_for_spawned_daemon_ready_direct(paths, readiness_timeout);
            } else {
                wait_for_spawned_daemon_ready(paths, readiness_timeout);
            }
        }
    }

    Ok(())
}

fn detached_spawn_args(via_self: bool, idle_timeout_secs: Option<u64>) -> Vec<String> {
    let mut args = if via_self {
        vec!["daemon".into(), "start".into(), "--foreground".into()]
    } else {
        vec!["--foreground".into()]
    };
    if let Some(seconds) = idle_timeout_secs {
        args.push(if via_self {
            "--idle-timeout".into()
        } else {
            "--idle-timeout-secs".into()
        });
        args.push(seconds.to_string());
    }
    args
}

fn ensure_startup_deadline_remaining(deadline: Option<Instant>) -> Result<(), LifecycleError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(LifecycleError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "daemon startup deadline elapsed before spawn",
        )));
    }
    Ok(())
}

fn displace_stale_daemon_before(paths: &SoldrPaths, deadline: Instant) -> bool {
    let Some(pid) = stale_daemon_occupies_endpoint(paths) else {
        return true;
    };
    if Instant::now() >= deadline {
        return false;
    }

    // The deadline-sensitive recovery path avoids a potentially slow IPC
    // shutdown probe. The PID identity gate is the same one used by the normal
    // graceful-then-kill path, so an unrelated recycled PID is never signaled.
    if !pid_is_soldr_daemon(pid) {
        if !pid_is_alive(pid) {
            append_lifecycle_event_with(
                paths,
                "previous-daemon-vanished-without-ack",
                LifecycleDetails::vanished_without_ack(Some(pid), LifecycleReason::StartupDeadline)
                    .from_current_process(Some(LifecycleSource::Preflight)),
            );
            return true;
        }
        return false;
    }

    // This path skips the IPC probe entirely, so the kill is attributable to
    // the expiring spawn deadline rather than anything learned about daemon.
    terminate_pid(pid, Some(deadline));
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(5));
    let exited = if remaining.is_zero() {
        !pid_is_alive(pid)
    } else {
        wait_for_pid_exit(pid, remaining)
    };
    append_lifecycle_event_with(
        paths,
        "displace-kill-fallback",
        LifecycleDetails::forced(pid, LifecycleReason::StartupDeadline)
            .with_outcome(if exited {
                LifecycleOutcome::Forced
            } else {
                LifecycleOutcome::Failed
            })
            .from_current_process(Some(LifecycleSource::Preflight)),
    );
    exited
}

/// Resolve the on-disk image the daemon child will exec from.
///
/// Every spawn shape — the sibling `soldr-daemon` binary AND the
/// via-self `soldr daemon start --foreground` fallback — routes through
/// `ensure_daemon_relocated` into `~/.soldr/runtime/soldr-daemon/`
/// (issue #1516). Via-self used to skip relocation and pin whatever
/// `current_exe` resolved to; when the self-relocation guard env vars
/// leak in from a parent process (or on slim installs), that pinned the
/// package-manager-owned `Scripts\soldr.exe`, so `pip install
/// --force-reinstall` wedged with WinError 5 while the daemon lived.
/// Relocating decouples the long-lived daemon from the installed
/// binary; `ensure_daemon_relocated` still runs maturin-repaired-wheel
/// layouts in place (soldr#1300) and no-ops when the source already
/// lives under the daemon runtime root.
///
/// Relocation failures fall back to running the source in place — a
/// pinned daemon beats no daemon — but say so. soldr#1987: that fallback
/// is how a daemon ends up running from a `uv`/`pip` temp directory, which
/// is then deleted, leaving an orphan holding the root-ownership lock
/// indefinitely. Silently pinning the one image we were trying not to pin
/// is the step that turns a recoverable I/O error into a 28-hour outage,
/// so the trade is made loudly and names both the path and the reason.
fn resolve_daemon_spawn_image(paths: Option<&SoldrPaths>, daemon_src: &Path) -> PathBuf {
    match paths {
        Some(paths) => crate::self_relocate::ensure_daemon_relocated(paths, daemon_src)
            .inspect(|r| {
                crate::self_relocate::run_periodic_daemon_runtime_gc(paths, Some(r));
            })
            .unwrap_or_else(|error| {
                tracing::warn!(
                    event = "daemon_relocation_failed",
                    source = %daemon_src.display(),
                    %error,
                    "could not relocate the daemon into the runtime root; spawning                      from its original location, which will stay pinned for the                      daemon's lifetime (soldr#1987)"
                );
                eprintln!(
                    "soldr: could not relocate soldr-daemon into the runtime root ({error});                      spawning from {} instead. That path stays locked while the daemon runs                      -- if it is a temporary directory, deleting it will strand the daemon                      (soldr#1987).",
                    daemon_src.display()
                );
                daemon_src.to_path_buf()
            }),
        // No cache root resolved → run in place. The daemon itself
        // tries SoldrPaths::new() at startup and will surface the
        // same error there.
        None => daemon_src.to_path_buf(),
    }
}

fn wait_for_spawned_daemon_ready(paths: &SoldrPaths, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_live(paths).is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for_spawned_daemon_ready_direct(paths: &SoldrPaths, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // Recovery runs inside a strict compile-dispatch budget. Avoid the full
        // backend adoption probe here because its fallback can hash an image.
        // PID identity plus the current-version claim are sufficient to prove
        // that this just-spawned process reached daemon initialization.
        if is_live_current_version_direct(paths).is_some() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    }
    false
}

/// Acquire the spawn-herd lock. Returns `Some(file)` when the
/// non-blocking exclusive lock was claimed (this caller is the spawn
/// owner), `None` when another wrapper already holds it (this caller
/// is a loser and should bail). The lock is released when the returned
/// `File` is dropped — typically at the end of `try_spawn_detached`.
///
/// Errors creating/opening the lock file are treated as "no lock
/// available" so a broken filesystem doesn't gate progress; we'd
/// rather have the herd-spawn fallback than block the build.
///
/// Exposed as `pub(crate)` so the unit tests below can verify the
/// exclusivity invariant without spawning a real daemon binary.
pub(crate) fn acquire_spawn_lock(paths: &SoldrPaths) -> Option<std::fs::File> {
    use fs2::FileExt;
    let dir = crate::cache_lib::soldr_daemon_dir(paths);
    std::fs::create_dir_all(&dir).ok()?;
    let lock_path = dir.join(".spawn.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    match file.try_lock_exclusive() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

#[cfg(unix)]
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is a well-defined liveness probe — no
    // signal is delivered, the syscall just returns 0 if the pid
    // exists and the caller has permission to signal it.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    // Unix retains an exited child in the process table as a zombie until its
    // parent reaps it. `kill(pid, 0)` still succeeds for that entry, but the
    // daemon has definitively exited and cannot serve IPC. Treat zombie/dead
    // task states as stopped so synchronous shutdown does not deadlock with a
    // parent waiting to reap after this probe returns.
    !pid_is_zombie(pid)
}

/// True when `pid` names a process that has exited but is still awaiting
/// collection by its parent.
///
/// Every platform without a probe answers `false`, which degrades to the
/// pre-existing `kill(pid, 0)`-only behavior rather than reporting a live
/// daemon as stopped.
#[cfg(unix)]
fn pid_is_zombie(pid: u32) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // The comm field is parenthesized and may itself contain spaces, so the
        // state character is the first byte after the LAST ") ".
        let Some((_, tail)) = stat.rsplit_once(") ") else {
            return false;
        };
        matches!(tail.as_bytes().first(), Some(b'Z' | b'X'))
    }

    // macOS/iOS have no /proc. `proc_pidinfo(PROC_PIDTBSDINFO)` is the
    // supported libproc query for a process's BSD state; `pbi_status` reports
    // `SZOMB` for an unreaped child. Without this branch a daemon spawned as a
    // direct child stays "alive" to `kill(pid, 0)` forever, so every
    // synchronous shutdown wait burns its full timeout — the darwin-only CI
    // break where `daemon stop` sat out all 300s of
    // `GRACEFUL_SHUTDOWN_WAIT_TIMEOUT` on an already-exited daemon.
    #[cfg(target_vendor = "apple")]
    {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // `arg` MUST be non-zero: for PROC_PIDTBSDINFO the kernel only falls
        // back to `proc_find_zombref` when `arg != 0` (xnu bsd/kern/proc_info.c).
        // With `arg == 0` a zombie fails the `proc_find` lookup and the call
        // returns ESRCH, which would defeat the entire purpose of this probe.
        const FIND_ZOMBIE: u64 = 1;
        // SAFETY: `proc_pidinfo` writes at most `size` bytes into the buffer and
        // reports how many it wrote. The struct is plain-old-data and is only
        // read after a full-size write is confirmed.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                FIND_ZOMBIE,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return false;
        }
        // SAFETY: the call above filled the whole struct.
        unsafe { info.assume_init() }.pbi_status == libc::SZOMB
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        let _ = pid;
        false
    }
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms, non_snake_case)]
pub(crate) fn pid_is_alive(pid: u32) -> bool {
    use std::os::windows::raw::HANDLE;
    // Win32 API spelling — clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
    const STILL_ACTIVE: DWORD = 259;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn CloseHandle(h: HANDLE) -> BOOL;
        fn GetExitCodeProcess(h: HANDLE, code: *mut DWORD) -> BOOL;
    }
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut code: DWORD = 0;
    let ok = unsafe { GetExitCodeProcess(h, &mut code) };
    unsafe { CloseHandle(h) };
    ok != 0 && code == STILL_ACTIVE
}

#[cfg(unix)]
pub(crate) fn pid_exe_stem_matches(pid: u32, expected_stem: &str) -> bool {
    process_image_stem_matches(pid_process_image_path(pid).as_deref(), expected_stem)
}

/// Compare an inspected process image to the expected executable stem.
///
/// Absence is deliberately a mismatch: callers use this check immediately
/// before signalling a PID, so an unavailable image probe must never turn a
/// stale PID file into authority to terminate an unrelated process.
#[cfg(unix)]
fn process_image_stem_matches(image: Option<&Path>, expected_stem: &str) -> bool {
    image
        .and_then(Path::file_stem)
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == expected_stem)
}

/// Read a running process's executable path.
///
/// Linux exposes this directly through procfs. macOS and the BSDs do not, so
/// use their portable `ps` process-image query instead. Every probe failure
/// returns `None`, which the identity gate treats as unverified.
#[cfg(target_os = "linux")]
fn pid_process_image_path(pid: u32) -> Option<PathBuf> {
    let link = PathBuf::from(format!("/proc/{pid}/exe"));
    fs::read_link(link).ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn pid_process_image_path(pid: u32) -> Option<PathBuf> {
    use std::io::Read;

    let mut command = std::process::Command::new("/bin/ps");
    command.args(["-p", &pid.to_string(), "-o", "comm="]);
    let stdio = running_process::SpawnStdio {
        stdin: running_process::StdioSource::Null,
        stdout: running_process::StdioSource::Pipe,
        stderr: running_process::StdioSource::Null,
        drain_timeout: Some(Duration::from_secs(2)),
        show_console: false,
    };
    let mut child = running_process::spawn(&mut command, stdio).ok()?;
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    if child.wait().ok()? != 0 {
        return None;
    }
    let image = String::from_utf8(stdout).ok()?;
    let image = image.trim();
    (!image.is_empty()).then(|| PathBuf::from(image))
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms, non_snake_case)]
pub(crate) fn pid_exe_stem_matches(pid: u32, expected_stem: &str) -> bool {
    use std::os::windows::raw::HANDLE;
    // Win32 API spelling — clippy would rename to Dword.
    #[allow(clippy::upper_case_acronyms)]
    type DWORD = u32;
    #[allow(clippy::upper_case_acronyms)]
    type BOOL = i32;
    type WCHAR = u16;
    const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
    extern "system" {
        fn OpenProcess(desired_access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        fn CloseHandle(h: HANDLE) -> BOOL;
        fn QueryFullProcessImageNameW(
            h: HANDLE,
            flags: DWORD,
            buf: *mut WCHAR,
            size: *mut DWORD,
        ) -> BOOL;
    }
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return false;
    }
    let mut buf: Vec<WCHAR> = vec![0; 1024];
    let mut size: DWORD = buf.len() as DWORD;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) };
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return false;
    }
    let s: String = String::from_utf16_lossy(&buf[..size as usize]);
    Path::new(&s)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case(expected_stem))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests;
