//! Read-only `broker doctor` environment diagnostics (#354, v1.x-5 from #228).
//!
//! `doctor` inspects the local broker environment and reports a flat list of
//! PASS / WARN / FAIL checks. It never mutates anything: no files are
//! created, deleted, or rewritten, no processes are spawned, and no daemon
//! state is changed. Stale artifacts are *reported*, never repaired.
//!
//! Check areas:
//!
//! 1. Environment-variable sanity for every `RUNNING_PROCESS_*` knob,
//!    including a loud WARN when test-only seams are set.
//! 2. Broker endpoint reachability: derive the default per-user shared
//!    broker endpoint, attempt a connection, and — when something is
//!    listening — run a deadline-bounded Hello probe to report the daemon
//!    version, negotiated protocol, and decoded server capability bits.
//! 3. Service-definition directory health plus per-file `.servicedef`
//!    parse/validation results (same loader the broker Hello path uses).
//! 4. Unix socket hygiene: count stale `*.sock` files in the broker runtime
//!    directory (connect-refused ⇒ stale). Reported, not deleted.
//! 5. Platform path budget: derived pipe/socket path length against the
//!    platform limit (`MAX_PATH` on Windows, `sun_path` on Unix).
//! 6. systemd KillMode (#391): WARN when systemd-managed with
//!    `KillMode=control-group` (or undeterminable). The only check that may
//!    spawn a process — a read-only `systemctl show -p KillMode` query on
//!    Linux, and only when `INVOCATION_ID` indicates systemd management.
//! 7. Version/build info: crate version, negotiated protocol version, and
//!    framing version.
//!
//! Every check is fault-isolated: a panic inside one check is converted to
//! a FAIL for that check and the remaining checks still run.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use prost::Message;

use crate::broker::capabilities::CAP_HANDLE_PASSING;
use crate::broker::client::{
    broker_disabled_by_env, connect_local_socket, RUNNING_PROCESS_DISABLE_ENV,
    RUNNING_PROCESS_FAKE_BACKEND_ENV,
};
use crate::broker::lifecycle::names::{
    backend_pipe, shared_broker_pipe, PipePathError, LINUX_SUN_PATH_MAX, MACOS_SUN_PATH_MAX,
    WINDOWS_MAX_PATH,
};
use crate::broker::lifecycle::sid::user_sid_hash;
use crate::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame, write_frame, ErrorCode, Frame, FrameKind,
    Hello, HelloReply, PayloadEncoding, CONTROL_PAYLOAD_PROTOCOL, PROTOCOL_VERSION,
};
use crate::broker::server::service_def_loader::{
    service_definition_dir, ServiceDefinitionLoader, SERVICE_DEF_DIR_ENV, SERVICE_DEF_EXTENSION,
};
use crate::broker::{secure_dir, FRAMING_VERSION_V1};

/// Daemon-IPC tracking kill switch read by the Python layer and daemon
/// client. Defined here as a literal because the canonical constant lives
/// behind the `daemon` feature and doctor must stay `client`-only.
const NO_TRACKING_ENV: &str = "RUNNING_PROCESS_NO_TRACKING";
/// CWD-scoped daemon override used for test isolation.
const DAEMON_SCOPE_ENV: &str = "RUNNING_PROCESS_DAEMON_SCOPE";
/// Admin-socket override consumed by the `running-process-broker-v1` CLI.
const BROKER_SOCKET_ENV: &str = "RUNNING_PROCESS_BROKER_V1_SOCKET";

/// Wall-clock bound on the Hello probe so doctor can never hang on a
/// listener that accepts but never replies.
pub const DOCTOR_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Service name the reachability probe sends in `Hello.service_name`.
///
/// A real broker refuses it with `ERROR_SERVICE_UNKNOWN` (unless an
/// operator actually installed a service with this name), which still
/// proves framing, protocol negotiation, and the daemon protocol range.
pub const DOCTOR_PROBE_SERVICE: &str = "rp-doctor-probe";

/// Outcome of one doctor check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoctorStatus {
    /// Healthy.
    Pass,
    /// Suspicious or non-default but not fatal. Never affects exit code.
    Warn,
    /// Broken. Any FAIL makes the doctor exit code 1.
    Fail,
}

impl DoctorStatus {
    /// Stable uppercase label used in both text and JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            DoctorStatus::Pass => "PASS",
            DoctorStatus::Warn => "WARN",
            DoctorStatus::Fail => "FAIL",
        }
    }
}

/// One named check with its outcome and a one-line detail.
#[derive(Clone, Debug)]
pub struct DoctorCheck {
    /// Stable check identifier, e.g. `env:RUNNING_PROCESS_DISABLE`.
    pub name: String,
    /// PASS / WARN / FAIL.
    pub status: DoctorStatus,
    /// Human-readable one-line detail.
    pub detail: String,
}

impl DoctorCheck {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: DoctorStatus::Pass,
            detail: detail.into(),
        }
    }

    fn warn(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: DoctorStatus::Warn,
            detail: detail.into(),
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: DoctorStatus::Fail,
            detail: detail.into(),
        }
    }
}

/// Aggregated doctor run.
#[derive(Clone, Debug, Default)]
pub struct DoctorReport {
    /// Every check that ran, in execution order.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// True when at least one check FAILed. WARNs do not count.
    pub fn has_failures(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == DoctorStatus::Fail)
    }

    /// Process exit code contract: 0 when no FAIL, 1 otherwise.
    pub fn exit_code(&self) -> i32 {
        if self.has_failures() {
            1
        } else {
            0
        }
    }

    /// Stable machine-readable JSON document.
    ///
    /// Shape (frozen — only additive changes allowed):
    /// `{"schema_version":1,"command":"doctor","exit_code":0,
    ///   "checks":[{"check":"...","status":"PASS","detail":"..."}]}`
    pub fn to_json(&self) -> String {
        let checks: Vec<serde_json::Value> = self
            .checks
            .iter()
            .map(|check| {
                serde_json::json!({
                    "check": check.name,
                    "status": check.status.as_str(),
                    "detail": check.detail,
                })
            })
            .collect();
        serde_json::json!({
            "schema_version": 1,
            "command": "doctor",
            "exit_code": self.exit_code(),
            "checks": checks,
        })
        .to_string()
    }

    /// Human-readable table plus a one-line summary.
    pub fn render_text(&self) -> String {
        let name_width = self
            .checks
            .iter()
            .map(|check| check.name.len())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for check in &self.checks {
            out.push_str(&format!(
                "{:<4}  {:<name_width$}  {}\n",
                check.status.as_str(),
                check.name,
                check.detail,
            ));
        }
        let pass = self.count(DoctorStatus::Pass);
        let warn = self.count(DoctorStatus::Warn);
        let fail = self.count(DoctorStatus::Fail);
        out.push_str(&format!(
            "doctor: {} checks — {pass} pass, {warn} warn, {fail} fail\n",
            self.checks.len()
        ));
        out
    }

    fn count(&self, status: DoctorStatus) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == status)
            .count()
    }
}

/// Inputs for [`run_doctor`]. `Default` derives everything from the
/// environment exactly like a broker client would.
#[derive(Clone, Debug, Default)]
pub struct DoctorOptions {
    /// Probe this broker endpoint instead of the derived per-user shared
    /// broker endpoint.
    pub broker_endpoint: Option<String>,
    /// Inspect this service-definition directory instead of the resolved
    /// platform default (`paths.service_definition_dir` contract).
    pub service_definition_dir: Option<PathBuf>,
}

/// Run every doctor check and aggregate the report.
///
/// Read-only by contract. Each check area is individually fault-isolated:
/// a panic in one area becomes a FAIL entry and the rest still run.
pub fn run_doctor(options: &DoctorOptions) -> DoctorReport {
    let mut checks = Vec::new();
    checks.extend(isolated("env", env_var_checks));
    {
        let endpoint = options.broker_endpoint.clone();
        checks.extend(isolated("broker:endpoint", move || {
            vec![broker_endpoint_check(endpoint.as_deref())]
        }));
    }
    {
        let dir = options
            .service_definition_dir
            .clone()
            .unwrap_or_else(service_definition_dir);
        checks.extend(isolated("servicedef:dir", move || {
            service_definition_checks(&dir)
        }));
    }
    checks.extend(isolated("sockets:runtime-dir", || {
        vec![socket_hygiene_check()]
    }));
    checks.extend(isolated("filesystem:inodes", || {
        vec![inode_pressure_check()]
    }));
    checks.extend(isolated("platform:path-budget", || {
        vec![platform_path_budget_check()]
    }));
    checks.extend(isolated("platform:systemd-killmode", || {
        vec![systemd_killmode_check()]
    }));
    checks.extend(isolated("build:version", || vec![version_check()]));
    DoctorReport { checks }
}

/// Run one check area, converting a panic into a FAIL for that area.
fn isolated<F>(area: &str, body: F) -> Vec<DoctorCheck>
where
    F: FnOnce() -> Vec<DoctorCheck> + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(body) {
        Ok(checks) => checks,
        Err(payload) => vec![DoctorCheck::fail(
            area,
            format!("check panicked: {}", panic_message(payload.as_ref())),
        )],
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

// ---------------------------------------------------------------------------
// 1. Environment-variable sanity
// ---------------------------------------------------------------------------

/// Check every running-process environment knob.
pub fn env_var_checks() -> Vec<DoctorCheck> {
    let mut checks = vec![disable_env_check(), fake_backend_env_check()];
    checks.push(informational_env_check(
        NO_TRACKING_ENV,
        "unset (daemon IPC tracking enabled)",
        "daemon IPC tracking disabled",
    ));
    checks.push(informational_env_check(
        DAEMON_SCOPE_ENV,
        "unset (user-scoped daemon)",
        "CWD-scoped daemon (test-isolation mode)",
    ));
    checks.push(informational_env_check(
        SERVICE_DEF_DIR_ENV,
        "unset (platform default service-definition dir)",
        "service-definition dir overridden",
    ));
    checks.push(informational_env_check(
        BROKER_SOCKET_ENV,
        "unset (derived broker endpoint)",
        "broker admin endpoint overridden",
    ));
    checks
}

fn disable_env_check() -> DoctorCheck {
    let name = format!("env:{RUNNING_PROCESS_DISABLE_ENV}");
    match broker_disabled_by_env() {
        Ok(false) => DoctorCheck::pass(name, "unset (broker enabled)"),
        Ok(true) => DoctorCheck::warn(
            name,
            "set to \"1\" — broker disabled; consumers use their direct fallback path",
        ),
        Err(err) => DoctorCheck::fail(name, err.to_string()),
    }
}

fn fake_backend_env_check() -> DoctorCheck {
    let name = format!("env:{RUNNING_PROCESS_FAKE_BACKEND_ENV}");
    match std::env::var_os(RUNNING_PROCESS_FAKE_BACKEND_ENV) {
        None => DoctorCheck::pass(name, "unset"),
        Some(value) if value.is_empty() => {
            DoctorCheck::warn(name, "set but empty (seam ignored) — unset it")
        }
        Some(value) => DoctorCheck::warn(
            name,
            format!(
                "TEST-ONLY seam is set to {:?} — broker negotiation is bypassed; \
                 never set this in production",
                value.to_string_lossy()
            ),
        ),
    }
}

fn informational_env_check(env: &str, unset_detail: &str, set_description: &str) -> DoctorCheck {
    let name = format!("env:{env}");
    match std::env::var_os(env) {
        None => DoctorCheck::pass(name, unset_detail),
        Some(value) => DoctorCheck::warn(
            name,
            format!("set to {:?} — {set_description}", value.to_string_lossy()),
        ),
    }
}

// ---------------------------------------------------------------------------
// 2. Broker endpoint reachability
// ---------------------------------------------------------------------------

/// Derive the default per-user shared-broker endpoint string.
pub fn default_broker_endpoint() -> Result<String, String> {
    let sid_hash = user_sid_hash().map_err(|err| err.to_string())?;
    let pipe = shared_broker_pipe(&sid_hash).map_err(|err| err.to_string())?;
    pipe_path_string(pipe.windows, pipe.unix)
        .ok_or_else(|| "pipe path has no platform form".to_string())
}

fn pipe_path_string(windows: Option<String>, unix: Option<PathBuf>) -> Option<String> {
    windows.or_else(|| unix.map(|path| path.to_string_lossy().into_owned()))
}

/// Probe `endpoint` (or the derived default) for a listening broker.
pub fn broker_endpoint_check(endpoint: Option<&str>) -> DoctorCheck {
    const NAME: &str = "broker:endpoint";
    let endpoint = match endpoint {
        Some(endpoint) => endpoint.to_string(),
        None => match default_broker_endpoint() {
            Ok(endpoint) => endpoint,
            Err(err) => {
                return DoctorCheck::fail(NAME, format!("cannot derive broker endpoint: {err}"));
            }
        },
    };
    let stream = match connect_local_socket(&endpoint) {
        Ok(stream) => stream,
        Err(err) => {
            return DoctorCheck::warn(NAME, format!("no broker listening at {endpoint} ({err})"));
        }
    };
    match hello_probe(stream) {
        Ok(ProbeOutcome::Negotiated {
            daemon_version,
            negotiated_protocol,
            server_capabilities,
        }) => DoctorCheck::pass(
            NAME,
            format!(
                "broker listening at {endpoint}: daemon {daemon_version}, \
                 protocol v{negotiated_protocol}, capabilities {}",
                describe_capabilities(server_capabilities)
            ),
        ),
        Ok(ProbeOutcome::Refused {
            code,
            daemon_min_protocol,
            daemon_max_protocol,
        }) => DoctorCheck::pass(
            NAME,
            format!(
                "broker listening at {endpoint}: protocol v{daemon_min_protocol}..v{daemon_max_protocol}, \
                 probe refused with {code:?} (expected for the doctor probe service)"
            ),
        ),
        Err(err) => DoctorCheck::warn(
            NAME,
            format!("{endpoint} accepted a connection but the v1 Hello probe failed: {err}"),
        ),
    }
}

enum ProbeOutcome {
    Negotiated {
        daemon_version: String,
        negotiated_protocol: u32,
        server_capabilities: u64,
    },
    Refused {
        code: ErrorCode,
        daemon_min_protocol: u32,
        daemon_max_protocol: u32,
    },
}

/// Send one Hello for [`DOCTOR_PROBE_SERVICE`] and classify the reply.
///
/// Runs on a helper thread bounded by [`DOCTOR_PROBE_TIMEOUT`] because
/// local-socket streams have no portable read timeout; on timeout the
/// abandoned stream stays with the helper thread.
fn hello_probe(stream: interprocess::local_socket::Stream) -> Result<ProbeOutcome, String> {
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stream = stream;
        let _ = result_tx.send(hello_probe_blocking(&mut stream));
    });
    match result_rx.recv_timeout(DOCTOR_PROBE_TIMEOUT) {
        Ok(outcome) => outcome,
        Err(_) => Err(format!(
            "no HelloReply within {DOCTOR_PROBE_TIMEOUT:?} (listener is not a v1 broker?)"
        )),
    }
}

fn hello_probe_blocking(
    stream: &mut interprocess::local_socket::Stream,
) -> Result<ProbeOutcome, String> {
    let hello = Hello {
        client_min_protocol: PROTOCOL_VERSION,
        client_max_protocol: PROTOCOL_VERSION,
        service_name: DOCTOR_PROBE_SERVICE.into(),
        wanted_version: "0.0.0".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "doctor-probe".into(),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process-doctor".into(),
        client_lib_version: env!("CARGO_PKG_VERSION").into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 0,
    };
    let request_frame = Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: CONTROL_PAYLOAD_PROTOCOL,
        payload: hello.encode_to_vec(),
        request_id: 1,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    write_frame(stream, &request_frame.encode_to_vec())
        .map_err(|err| format!("failed to write Hello frame: {err}"))?;
    let response_bytes =
        read_frame(stream).map_err(|err| format!("failed to read HelloReply frame: {err}"))?;
    let response_frame = Frame::decode(response_bytes.as_slice())
        .map_err(|err| format!("failed to decode response Frame: {err}"))?;
    let reply = HelloReply::decode(response_frame.payload.as_slice())
        .map_err(|err| format!("failed to decode HelloReply: {err}"))?;
    match reply.result.ok_or("HelloReply carried no result")? {
        HelloReplyResult::Negotiated(negotiated) => Ok(ProbeOutcome::Negotiated {
            daemon_version: negotiated.daemon_version,
            negotiated_protocol: negotiated.negotiated_protocol,
            server_capabilities: negotiated.server_capabilities,
        }),
        HelloReplyResult::Refused(refused) => Ok(ProbeOutcome::Refused {
            code: ErrorCode::try_from(refused.code).unwrap_or(ErrorCode::Unspecified),
            daemon_min_protocol: refused.daemon_min_protocol,
            daemon_max_protocol: refused.daemon_max_protocol,
        }),
    }
}

/// Render a capability bitmap with the registry's known bit names.
pub fn describe_capabilities(bits: u64) -> String {
    if bits == 0 {
        return "none".to_string();
    }
    let mut names = Vec::new();
    if bits & CAP_HANDLE_PASSING != 0 {
        names.push("HANDLE_PASSING".to_string());
    }
    let unknown = bits & !CAP_HANDLE_PASSING;
    if unknown != 0 {
        names.push(format!("unknown:0x{unknown:x}"));
    }
    format!("0x{bits:x} [{}]", names.join(", "))
}

// ---------------------------------------------------------------------------
// 3. Service-definition directory + per-file validation
// ---------------------------------------------------------------------------

/// Check the service-definition directory and every `.servicedef` in it.
pub fn service_definition_checks(dir: &Path) -> Vec<DoctorCheck> {
    const DIR_CHECK: &str = "servicedef:dir";
    let display = dir.display();
    if !dir.exists() {
        return vec![DoctorCheck::warn(
            DIR_CHECK,
            format!("{display} does not exist (no service definitions installed)"),
        )];
    }
    if !dir.is_dir() {
        return vec![DoctorCheck::fail(
            DIR_CHECK,
            format!("{display} exists but is not a directory"),
        )];
    }
    match secure_dir::private_dir_permissions_are_private(dir) {
        Ok(true) => {}
        Ok(false) => {
            return vec![DoctorCheck::fail(
                DIR_CHECK,
                format!(
                    "{display} has insecure permissions (must be current-user-only); \
                     the broker refuses to load definitions from it"
                ),
            )];
        }
        Err(err) => {
            return vec![DoctorCheck::fail(
                DIR_CHECK,
                format!("cannot inspect permissions of {display}: {err}"),
            )];
        }
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            return vec![DoctorCheck::fail(
                DIR_CHECK,
                format!("cannot enumerate {display}: {err}"),
            )];
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .map(|ext| ext == SERVICE_DEF_EXTENSION)
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    let mut checks = vec![DoctorCheck::pass(
        DIR_CHECK,
        format!(
            "{display} (private, {} .{SERVICE_DEF_EXTENSION} file{})",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        ),
    )];

    let loader = ServiceDefinitionLoader::new(dir);
    for path in files {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let check_name = format!("servicedef:{file_name}");
        let Some(service_name) = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
        else {
            checks.push(DoctorCheck::fail(check_name, "file has no stem"));
            continue;
        };
        match loader.load(&service_name) {
            Ok(definition) => checks.push(DoctorCheck::pass(
                check_name,
                format!(
                    "valid (service {:?}, binary {:?})",
                    definition.service_name, definition.binary_path
                ),
            )),
            Err(err) => checks.push(DoctorCheck::fail(check_name, err.to_string())),
        }
    }
    checks
}

// ---------------------------------------------------------------------------
// 4. Socket/pipe hygiene
// ---------------------------------------------------------------------------

/// Report stale `*.sock` files in the broker runtime directory (Unix).
///
/// A socket file counts as stale when connecting to it is refused —
/// nothing is listening behind it. Doctor only reports the count; it
/// never deletes anything.
pub fn socket_hygiene_check() -> DoctorCheck {
    const NAME: &str = "sockets:runtime-dir";
    #[cfg(windows)]
    {
        DoctorCheck::pass(
            NAME,
            "not applicable on Windows (named pipes leave no filesystem residue)",
        )
    }
    #[cfg(unix)]
    {
        let Some(dir) = broker_runtime_dir() else {
            return DoctorCheck::fail(NAME, "cannot derive broker runtime directory");
        };
        let display = dir.display();
        if !dir.exists() {
            return DoctorCheck::pass(NAME, format!("{display} does not exist (no sockets)"));
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                return DoctorCheck::fail(NAME, format!("cannot enumerate {display}: {err}"));
            }
        };
        let mut total = 0usize;
        let mut stale = 0usize;
        for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
            if path.extension().map(|ext| ext == "sock").unwrap_or(false) {
                total += 1;
                let endpoint = path.to_string_lossy();
                if let Err(err) = connect_local_socket(&endpoint) {
                    if err.kind() == std::io::ErrorKind::ConnectionRefused {
                        stale += 1;
                    }
                }
            }
        }
        if stale == 0 {
            DoctorCheck::pass(
                NAME,
                format!("{display}: {total} socket file(s), none stale"),
            )
        } else {
            DoctorCheck::warn(
                NAME,
                format!(
                    "{display}: {stale} of {total} socket file(s) are stale \
                     (connect refused) — not deleted, doctor is read-only"
                ),
            )
        }
    }
}

/// Parent directory of the per-user broker sockets, derived from the
/// shared-broker pipe path (Unix only).
#[cfg(unix)]
fn broker_runtime_dir() -> Option<PathBuf> {
    let sid_hash = user_sid_hash().ok()?;
    let pipe = shared_broker_pipe(&sid_hash).ok()?;
    pipe.unix
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

// ---------------------------------------------------------------------------
// 4b. Inode pressure on the daemon data dir filesystem (#390)
// ---------------------------------------------------------------------------

/// Free-inode fraction below which the check WARNs.
const INODE_WARN_FREE_RATIO: f64 = 0.05;
/// Free-inode fraction below which the check FAILs.
const INODE_FAIL_FREE_RATIO: f64 = 0.01;

/// Report inode usage/headroom of the daemon data dir filesystem.
///
/// Windows filesystems have no fixed inode table, so the check PASSes as
/// not-applicable there instead of faking numbers. Same for Unix
/// filesystems reporting a zero inode total (e.g. btrfs).
pub fn inode_pressure_check() -> DoctorCheck {
    const NAME: &str = "filesystem:inodes";
    let dir = crate::client::paths::data_dir();
    let display = dir.display();
    match crate::broker::fs_health::daemon_data_dir_inode_usage() {
        Ok(Some(usage)) => {
            let free_ratio = if usage.total == 0 {
                1.0
            } else {
                usage.free as f64 / usage.total as f64
            };
            let detail = format!(
                "{display}: {} of {} inodes free ({:.1}% used)",
                usage.free,
                usage.total,
                usage.used_ratio() * 100.0
            );
            if free_ratio < INODE_FAIL_FREE_RATIO {
                DoctorCheck::fail(
                    NAME,
                    format!("{detail} — inode exhaustion imminent; daemon writes will ENOSPC"),
                )
            } else if free_ratio < INODE_WARN_FREE_RATIO {
                DoctorCheck::warn(NAME, format!("{detail} — low inode headroom"))
            } else {
                DoctorCheck::pass(NAME, detail)
            }
        }
        Ok(None) => DoctorCheck::pass(
            NAME,
            if cfg!(windows) {
                format!("not applicable on Windows ({display} has no fixed inode table)")
            } else {
                format!("{display}: filesystem reports no fixed inode table (not applicable)")
            },
        ),
        Err(err) => DoctorCheck::warn(
            NAME,
            format!("cannot probe inode usage of {display}: {err}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// 5. Platform path budget
// ---------------------------------------------------------------------------

/// Slack (bytes) below the platform path limit that triggers a WARN.
const PATH_BUDGET_WARN_SLACK: usize = 8;

/// Check the longest standard pipe name (a backend pipe) against the
/// platform path-length limit. This bit the test suite repeatedly on
/// macOS, where `sun_path` is only 104 bytes.
pub fn platform_path_budget_check() -> DoctorCheck {
    const NAME: &str = "platform:path-budget";
    let (limit, limit_label) = if cfg!(windows) {
        (WINDOWS_MAX_PATH, "Windows MAX_PATH")
    } else if cfg!(target_os = "macos") {
        (MACOS_SUN_PATH_MAX, "macOS sun_path")
    } else {
        (LINUX_SUN_PATH_MAX, "Linux/Unix sun_path")
    };
    let sid_hash = match user_sid_hash() {
        Ok(hash) => hash,
        Err(err) => {
            return DoctorCheck::fail(NAME, format!("cannot compute user SID hash: {err}"));
        }
    };
    // Backend pipes carry the longest standard suffix (32 hex chars), so
    // they exhaust the budget first.
    match backend_pipe(&sid_hash, &[0u8; 16]) {
        Ok(pipe) => {
            let Some(path) = pipe_path_string(pipe.windows, pipe.unix) else {
                return DoctorCheck::fail(NAME, "derived pipe path has no platform form");
            };
            let len = path.len();
            let detail =
                format!("backend pipe path is {len} of {limit} bytes ({limit_label}): {path}");
            if len + PATH_BUDGET_WARN_SLACK >= limit {
                DoctorCheck::warn(
                    NAME,
                    format!("{detail} — within {PATH_BUDGET_WARN_SLACK} bytes of the limit"),
                )
            } else {
                DoctorCheck::pass(NAME, detail)
            }
        }
        Err(err @ PipePathError::PathTooLong { .. }) => DoctorCheck::fail(
            NAME,
            format!("derived backend pipe path exceeds the {limit_label} budget: {err}"),
        ),
        Err(err) => DoctorCheck::fail(NAME, format!("cannot derive backend pipe path: {err}")),
    }
}

// ---------------------------------------------------------------------------
// 6. systemd KillMode (#391)
// ---------------------------------------------------------------------------

/// WARN when running under a systemd unit whose KillMode would reap
/// spawned children on unit stop (`control-group`, systemd's default), or
/// when systemd-managed but the KillMode cannot be determined.
pub fn systemd_killmode_check() -> DoctorCheck {
    const NAME: &str = "platform:systemd-killmode";
    use crate::systemd_killmode::{probe, KillModeAssessment};
    let assessment = probe();
    match assessment.warning() {
        Some(warning) => DoctorCheck::warn(NAME, warning),
        None => match assessment {
            KillModeAssessment::Safe { unit, kill_mode } => DoctorCheck::pass(
                NAME,
                format!("systemd unit {unit} uses KillMode={kill_mode} (children survive stop)"),
            ),
            _ => DoctorCheck::pass(
                NAME,
                if cfg!(target_os = "linux") {
                    "not running under systemd"
                } else {
                    "not applicable on this platform"
                },
            ),
        },
    }
}

// ---------------------------------------------------------------------------
// 7. Version/build info
// ---------------------------------------------------------------------------

/// Report crate, protocol, and framing versions. Always PASS.
pub fn version_check() -> DoctorCheck {
    DoctorCheck::pass(
        "build:version",
        format!(
            "running-process {} — protocol v{PROTOCOL_VERSION}, framing v{FRAMING_VERSION_V1}",
            env!("CARGO_PKG_VERSION")
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(status: DoctorStatus) -> DoctorCheck {
        DoctorCheck {
            name: "test:check".into(),
            status,
            detail: "detail".into(),
        }
    }

    #[test]
    fn exit_code_is_zero_without_failures() {
        let report = DoctorReport {
            checks: vec![check(DoctorStatus::Pass), check(DoctorStatus::Warn)],
        };
        assert!(!report.has_failures());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn exit_code_is_one_with_any_failure() {
        let report = DoctorReport {
            checks: vec![check(DoctorStatus::Pass), check(DoctorStatus::Fail)],
        };
        assert!(report.has_failures());
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn isolated_converts_panics_into_fail_checks() {
        let checks = isolated("area:test", || panic!("boom"));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, DoctorStatus::Fail);
        assert!(checks[0].detail.contains("boom"));
    }

    #[test]
    fn describe_capabilities_names_known_bits() {
        assert_eq!(describe_capabilities(0), "none");
        assert_eq!(describe_capabilities(1), "0x1 [HANDLE_PASSING]");
        let mixed = describe_capabilities(0b11);
        assert!(mixed.contains("HANDLE_PASSING"));
        assert!(mixed.contains("unknown:0x2"));
    }

    #[test]
    fn render_text_includes_summary_line() {
        let report = DoctorReport {
            checks: vec![check(DoctorStatus::Pass), check(DoctorStatus::Warn)],
        };
        let text = report.render_text();
        assert!(text.contains("PASS"));
        assert!(text.contains("WARN"));
        assert!(text.contains("doctor: 2 checks — 1 pass, 1 warn, 0 fail"));
    }
}
