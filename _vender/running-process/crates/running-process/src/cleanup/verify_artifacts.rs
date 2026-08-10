//! Exhaustive daemon-artifact reconciliation for `cleanup verify` (#391,
//! part of #354).
//!
//! Enumerates every artifact class the daemon can leave behind — IPC
//! socket, pid file, `.servicedef` files, the SQLite registry database
//! (plus WAL/SHM sidecars), log files, the ENOSPC emergency reserve, and
//! shadow-dir contents — and reports each location as clean, active,
//! present, stale, or orphaned. READ-ONLY by contract: nothing is created,
//! deleted, or rewritten. The documented operator checklist lives in
//! `docs/v1-troubleshooting.md` ("Cleanup Verification").
//!
//! Every check is a pure function over injected paths/probes so tests can
//! exercise each class with temp dirs on all platforms; only the
//! `ArtifactPaths::from_environment` constructor and the default probes
//! in `run` touch the real environment.

use std::path::{Path, PathBuf};

use crate::broker::backend_lifecycle::verify_pid::process_is_alive;
use crate::broker::server::service_def_loader::SERVICE_DEF_EXTENSION;
use crate::client::paths;

/// Leaf name of the ENOSPC emergency reserve file (#390). Mirrors
/// `daemon::emergency_reserve::EMERGENCY_RESERVE_FILE_NAME`, duplicated
/// here because this module is `client`-only while the canonical constant
/// is gated behind the `daemon` feature.
pub const EMERGENCY_RESERVE_FILE_NAME: &str = "emergency-reserve.bin";
/// Expected size of a fully-armed emergency reserve. Mirrors
/// `daemon::emergency_reserve::EMERGENCY_RESERVE_BYTES`.
pub const EMERGENCY_RESERVE_BYTES: u64 = 32 * 1024 * 1024;

/// Reconciliation outcome for one artifact location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactStatus {
    /// Expected location is empty — nothing left behind.
    Clean,
    /// Artifact exists and belongs to a live daemon.
    Active,
    /// Artifact exists and is expected to persist across daemon runs
    /// (databases, service definitions, armed reserve, shadow copies).
    Present,
    /// Artifact exists but its owner is gone (dead pid, refused socket,
    /// WAL left by an unclean shutdown, truncated reserve).
    Stale,
    /// Unexpected file in a managed location.
    Orphaned,
    /// The location could not be inspected.
    Error,
}

impl ArtifactStatus {
    /// Stable uppercase label used in both text and JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactStatus::Clean => "CLEAN",
            ArtifactStatus::Active => "ACTIVE",
            ArtifactStatus::Present => "PRESENT",
            ArtifactStatus::Stale => "STALE",
            ArtifactStatus::Orphaned => "ORPHANED",
            ArtifactStatus::Error => "ERROR",
        }
    }

    /// True when this status flags residue worth operator attention.
    pub fn is_finding(self) -> bool {
        matches!(
            self,
            ArtifactStatus::Stale | ArtifactStatus::Orphaned | ArtifactStatus::Error
        )
    }
}

/// One reconciled artifact location.
#[derive(Clone, Debug)]
pub struct ArtifactCheck {
    /// Stable artifact class identifier, e.g. `socket`, `pid-file`.
    pub class: &'static str,
    /// Location inspected (path or pipe name).
    pub location: String,
    /// Reconciliation outcome.
    pub status: ArtifactStatus,
    /// Human-readable one-line detail.
    pub detail: String,
}

impl ArtifactCheck {
    fn new(
        class: &'static str,
        location: impl Into<String>,
        status: ArtifactStatus,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            class,
            location: location.into(),
            status,
            detail: detail.into(),
        }
    }
}

/// Aggregated artifact reconciliation report.
#[derive(Clone, Debug, Default)]
pub struct ArtifactReport {
    /// Every artifact location inspected, in execution order.
    pub checks: Vec<ArtifactCheck>,
}

impl ArtifactReport {
    /// Number of STALE/ORPHANED/ERROR entries.
    pub fn finding_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status.is_finding())
            .count()
    }

    /// Process exit code contract (mirrors `broker doctor`): 0 unless a
    /// location could not be inspected. Stale/orphaned residue is reported
    /// but does not fail the command — verification is advisory.
    pub fn exit_code(&self) -> i32 {
        if self
            .checks
            .iter()
            .any(|check| check.status == ArtifactStatus::Error)
        {
            1
        } else {
            0
        }
    }

    /// Stable machine-readable JSON value (additive-only shape).
    pub fn to_json_value(&self) -> serde_json::Value {
        let checks: Vec<serde_json::Value> = self
            .checks
            .iter()
            .map(|check| {
                serde_json::json!({
                    "class": check.class,
                    "location": check.location,
                    "status": check.status.as_str(),
                    "detail": check.detail,
                })
            })
            .collect();
        serde_json::json!({
            "schema_version": 1,
            "exit_code": self.exit_code(),
            "findings": self.finding_count(),
            "checks": checks,
        })
    }

    /// Human-readable table plus a one-line summary.
    pub fn render_text(&self) -> String {
        let class_width = self
            .checks
            .iter()
            .map(|check| check.class.len())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for check in &self.checks {
            out.push_str(&format!(
                "{:<8}  {:<class_width$}  {}  {}\n",
                check.status.as_str(),
                check.class,
                check.location,
                check.detail,
            ));
        }
        out.push_str(&format!(
            "cleanup verify: {} location(s) — {} finding(s)\n",
            self.checks.len(),
            self.finding_count()
        ));
        out
    }
}

/// Where the daemon socket lives on this platform.
#[derive(Clone, Debug)]
pub enum SocketLocation {
    /// Unix-domain socket file.
    File(PathBuf),
    /// Windows named pipe (no filesystem residue).
    NamedPipe(String),
}

/// Every expected daemon artifact location. Fully injectable for tests;
/// [`Self::from_environment`] derives the real platform locations without
/// creating any directory.
#[derive(Clone, Debug)]
pub struct ArtifactPaths {
    /// Daemon IPC endpoint.
    pub socket: SocketLocation,
    /// Daemon pid/identity file.
    pub pid_file: PathBuf,
    /// SQLite registry database.
    pub db: PathBuf,
    /// Daemon data directory (scanned for log files).
    pub data_dir: PathBuf,
    /// ENOSPC emergency reserve file (#390).
    pub emergency_reserve: PathBuf,
    /// Expected byte size of a fully-armed reserve.
    pub emergency_reserve_bytes: u64,
    /// Service-definition directory (`*.servicedef`, #364).
    pub service_definition_dir: PathBuf,
    /// Shadow-copy directory for relocated daemon binaries.
    pub shadow_dir: PathBuf,
}

impl ArtifactPaths {
    /// Derive every expected location from the environment, read-only.
    pub fn from_environment(scope_hash: Option<&str>) -> Self {
        let endpoint = paths::socket_path_view(scope_hash);
        let socket = if cfg!(windows) {
            SocketLocation::NamedPipe(endpoint)
        } else {
            SocketLocation::File(PathBuf::from(endpoint))
        };
        let data_dir = paths::data_dir();
        Self {
            socket,
            pid_file: paths::pid_file_path_view(scope_hash),
            db: paths::db_path_view(scope_hash),
            emergency_reserve: data_dir.join(EMERGENCY_RESERVE_FILE_NAME),
            emergency_reserve_bytes: EMERGENCY_RESERVE_BYTES,
            data_dir,
            service_definition_dir:
                crate::broker::server::service_def_loader::service_definition_dir(),
            shadow_dir: paths::shadow_dir_view(),
        }
    }
}

/// Reconcile every artifact class against the live environment.
pub fn run(paths: &ArtifactPaths) -> ArtifactReport {
    let connect =
        |endpoint: &str| crate::broker::client::connect_local_socket(endpoint).map(|_stream| ());
    run_with_probes(paths, &process_is_alive, &connect)
}

/// `run` with injected liveness/connect probes (test seam).
pub fn run_with_probes(
    paths: &ArtifactPaths,
    pid_is_alive: &dyn Fn(u32) -> bool,
    connect: &dyn Fn(&str) -> std::io::Result<()>,
) -> ArtifactReport {
    let pid_check = check_pid_file(&paths.pid_file, pid_is_alive);
    let daemon_alive = pid_check.status == ArtifactStatus::Active;
    let mut checks = vec![check_socket(&paths.socket, connect), pid_check];
    checks.extend(check_service_definitions(&paths.service_definition_dir));
    checks.extend(check_database(&paths.db, daemon_alive));
    checks.push(check_logs(&paths.data_dir));
    checks.push(check_emergency_reserve(
        &paths.emergency_reserve,
        paths.emergency_reserve_bytes,
    ));
    checks.push(check_shadow_dir(&paths.shadow_dir, daemon_alive));
    ArtifactReport { checks }
}

/// Reconcile the daemon IPC endpoint.
pub fn check_socket(
    socket: &SocketLocation,
    connect: &dyn Fn(&str) -> std::io::Result<()>,
) -> ArtifactCheck {
    const CLASS: &str = "socket";
    match socket {
        SocketLocation::NamedPipe(name) => match connect(name) {
            Ok(()) => ArtifactCheck::new(
                CLASS,
                name.clone(),
                ArtifactStatus::Active,
                "named pipe accepts connections",
            ),
            Err(_) => ArtifactCheck::new(
                CLASS,
                name.clone(),
                ArtifactStatus::Clean,
                "no listener (named pipes leave no filesystem residue)",
            ),
        },
        SocketLocation::File(path) => {
            if !path.exists() {
                return ArtifactCheck::new(
                    CLASS,
                    path.display().to_string(),
                    ArtifactStatus::Clean,
                    "socket file absent",
                );
            }
            match connect(&path.to_string_lossy()) {
                Ok(()) => ArtifactCheck::new(
                    CLASS,
                    path.display().to_string(),
                    ArtifactStatus::Active,
                    "socket file exists and accepts connections",
                ),
                Err(err) => ArtifactCheck::new(
                    CLASS,
                    path.display().to_string(),
                    ArtifactStatus::Stale,
                    format!("socket file exists but nothing is listening ({err})"),
                ),
            }
        }
    }
}

/// Reconcile the daemon pid file against process liveness.
pub fn check_pid_file(path: &Path, pid_is_alive: &dyn Fn(u32) -> bool) -> ArtifactCheck {
    const CLASS: &str = "pid-file";
    let location = path.display().to_string();
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return ArtifactCheck::new(CLASS, location, ArtifactStatus::Clean, "pid file absent");
        }
        Err(err) => {
            return ArtifactCheck::new(
                CLASS,
                location,
                ArtifactStatus::Error,
                format!("cannot read pid file: {err}"),
            );
        }
    };
    match contents.trim().parse::<u32>() {
        Ok(pid) if pid_is_alive(pid) => ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Active,
            format!("daemon pid {pid} is alive"),
        ),
        Ok(pid) => ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Stale,
            format!("pid {pid} is not alive"),
        ),
        Err(_) => ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Stale,
            format!("unparsable pid file contents {:?}", contents.trim()),
        ),
    }
}

/// Reconcile the service-definition directory: `.servicedef` files are
/// expected persistent config; anything else in the directory is orphaned.
pub fn check_service_definitions(dir: &Path) -> Vec<ArtifactCheck> {
    const CLASS: &str = "servicedef";
    let location = dir.display().to_string();
    if !dir.exists() {
        return vec![ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Clean,
            "service-definition directory absent (no services installed)",
        )];
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            return vec![ArtifactCheck::new(
                CLASS,
                location,
                ArtifactStatus::Error,
                format!("cannot enumerate directory: {err}"),
            )];
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    let mut definitions = 0usize;
    let mut checks = Vec::new();
    for path in &paths {
        let is_definition = path
            .extension()
            .map(|ext| ext == SERVICE_DEF_EXTENSION)
            .unwrap_or(false);
        if is_definition {
            definitions += 1;
        } else {
            checks.push(ArtifactCheck::new(
                CLASS,
                path.display().to_string(),
                ArtifactStatus::Orphaned,
                format!("unexpected non-.{SERVICE_DEF_EXTENSION} entry in service-definition dir"),
            ));
        }
    }
    checks.insert(
        0,
        ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Present,
            format!("{definitions} .{SERVICE_DEF_EXTENSION} file(s) (persistent config, expected)"),
        ),
    );
    checks
}

/// Reconcile the SQLite registry database and its WAL/SHM sidecars.
pub fn check_database(db: &Path, daemon_alive: bool) -> Vec<ArtifactCheck> {
    const CLASS: &str = "database";
    let mut checks = Vec::new();
    let location = db.display().to_string();
    if db.exists() {
        let size = std::fs::metadata(db).map(|meta| meta.len()).unwrap_or(0);
        checks.push(ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Present,
            format!("registry database exists ({size} bytes; persists across daemon runs)"),
        ));
    } else {
        checks.push(ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Clean,
            "registry database absent (daemon never ran in this scope)",
        ));
    }
    for suffix in ["-wal", "-shm"] {
        let mut name = db.as_os_str().to_os_string();
        name.push(suffix);
        let sidecar = PathBuf::from(name);
        if !sidecar.exists() {
            continue;
        }
        let location = sidecar.display().to_string();
        if daemon_alive {
            checks.push(ArtifactCheck::new(
                CLASS,
                location,
                ArtifactStatus::Active,
                format!("sqlite {suffix} sidecar held by the live daemon"),
            ));
        } else {
            checks.push(ArtifactCheck::new(
                CLASS,
                location,
                ArtifactStatus::Stale,
                format!(
                    "sqlite {suffix} sidecar left behind with no live daemon (unclean shutdown)"
                ),
            ));
        }
    }
    checks
}

/// Reconcile log files (`*.log`) in the daemon data directory. None are
/// expected by default; any found are reported, never deleted.
pub fn check_logs(data_dir: &Path) -> ArtifactCheck {
    const CLASS: &str = "logs";
    let location = data_dir.display().to_string();
    if !data_dir.exists() {
        return ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Clean,
            "data directory absent (no log files)",
        );
    }
    let entries = match std::fs::read_dir(data_dir) {
        Ok(entries) => entries,
        Err(err) => {
            return ArtifactCheck::new(
                CLASS,
                location,
                ArtifactStatus::Error,
                format!("cannot enumerate data directory: {err}"),
            );
        }
    };
    let mut count = 0usize;
    let mut bytes = 0u64;
    for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
        if path.extension().map(|ext| ext == "log").unwrap_or(false) {
            count += 1;
            bytes += std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        }
    }
    if count == 0 {
        ArtifactCheck::new(CLASS, location, ArtifactStatus::Clean, "no *.log files")
    } else {
        ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Present,
            format!("{count} *.log file(s), {bytes} bytes total (reported, not deleted)"),
        )
    }
}

/// Reconcile the 32 MiB ENOSPC emergency reserve (#390).
pub fn check_emergency_reserve(path: &Path, expected_bytes: u64) -> ArtifactCheck {
    const CLASS: &str = "emergency-reserve";
    let location = path.display().to_string();
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() == expected_bytes => ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Present,
            format!("armed at {expected_bytes} bytes (recreated at every daemon startup)"),
        ),
        Ok(meta) => ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Stale,
            format!(
                "unexpected size {} bytes (expected {expected_bytes}); partial pre-allocation \
                 from a crashed startup",
                meta.len()
            ),
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Clean,
            "absent (released after ENOSPC or daemon never ran; re-armed at next startup)",
        ),
        Err(err) => ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Error,
            format!("cannot inspect reserve file: {err}"),
        ),
    }
}

/// Reconcile shadow-dir contents (relocated daemon binaries).
pub fn check_shadow_dir(dir: &Path, daemon_alive: bool) -> ArtifactCheck {
    const CLASS: &str = "shadow";
    let location = dir.display().to_string();
    if !dir.exists() {
        return ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Clean,
            "shadow directory absent",
        );
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            return ArtifactCheck::new(
                CLASS,
                location,
                ArtifactStatus::Error,
                format!("cannot enumerate shadow directory: {err}"),
            );
        }
    };
    let count = entries.filter_map(|entry| entry.ok()).count();
    if count == 0 {
        ArtifactCheck::new(CLASS, location, ArtifactStatus::Clean, "empty")
    } else if daemon_alive {
        ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Active,
            format!(
                "{count} entr{} (may include the running daemon's shadow copy)",
                if count == 1 { "y" } else { "ies" }
            ),
        )
    } else {
        ArtifactCheck::new(
            CLASS,
            location,
            ArtifactStatus::Present,
            format!(
                "{count} entr{} with no live daemon (shadow copies persist by design; \
                 prune manually if disk space matters)",
                if count == 1 { "y" } else { "ies" }
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rp-verify-artifacts-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pid_file_absent_is_clean() {
        let dir = temp_dir("pid-clean");
        let check = check_pid_file(&dir.join("daemon.pid"), &|_| true);
        assert_eq!(check.status, ArtifactStatus::Clean);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pid_file_live_pid_is_active_dead_pid_is_stale() {
        let dir = temp_dir("pid-live");
        let path = dir.join("daemon.pid");
        std::fs::write(&path, "4242\n").unwrap();
        assert_eq!(
            check_pid_file(&path, &|pid| pid == 4242).status,
            ArtifactStatus::Active
        );
        assert_eq!(
            check_pid_file(&path, &|_| false).status,
            ArtifactStatus::Stale
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pid_file_garbage_is_stale() {
        let dir = temp_dir("pid-garbage");
        let path = dir.join("daemon.pid");
        std::fs::write(&path, "not-a-pid").unwrap();
        let check = check_pid_file(&path, &|_| true);
        assert_eq!(check.status, ArtifactStatus::Stale);
        assert!(check.detail.contains("unparsable"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn socket_file_states() {
        let dir = temp_dir("socket");
        let path = dir.join("daemon.sock");
        let absent = SocketLocation::File(path.clone());
        assert_eq!(
            check_socket(&absent, &|_| Ok(())).status,
            ArtifactStatus::Clean
        );

        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            check_socket(&SocketLocation::File(path.clone()), &|_| Ok(())).status,
            ArtifactStatus::Active
        );
        let refused = |_endpoint: &str| -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
        };
        let check = check_socket(&SocketLocation::File(path), &refused);
        assert_eq!(check.status, ArtifactStatus::Stale);
        assert!(check.detail.contains("nothing is listening"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn named_pipe_states() {
        let pipe = SocketLocation::NamedPipe(r"\\.\pipe\rp-test".into());
        assert_eq!(
            check_socket(&pipe, &|_| Ok(())).status,
            ArtifactStatus::Active
        );
        let gone = |_endpoint: &str| -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        assert_eq!(check_socket(&pipe, &gone).status, ArtifactStatus::Clean);
    }

    #[test]
    fn service_definitions_report_files_and_orphans() {
        let dir = temp_dir("servicedef");
        std::fs::write(dir.join("svc.servicedef"), b"x").unwrap();
        std::fs::write(dir.join("stray.txt"), b"x").unwrap();
        let checks = check_service_definitions(&dir);
        assert_eq!(checks[0].status, ArtifactStatus::Present);
        assert!(checks[0].detail.contains("1 .servicedef"));
        let orphan = checks
            .iter()
            .find(|check| check.status == ArtifactStatus::Orphaned)
            .expect("stray file flagged");
        assert!(orphan.location.contains("stray.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn service_definition_dir_absent_is_clean() {
        let dir = temp_dir("servicedef-absent");
        let checks = check_service_definitions(&dir.join("missing"));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, ArtifactStatus::Clean);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn database_and_sidecars_reconcile_against_liveness() {
        let dir = temp_dir("db");
        let db = dir.join("tracked-pids.sqlite3");
        assert_eq!(check_database(&db, false)[0].status, ArtifactStatus::Clean);

        std::fs::write(&db, b"db").unwrap();
        std::fs::write(dir.join("tracked-pids.sqlite3-wal"), b"wal").unwrap();
        std::fs::write(dir.join("tracked-pids.sqlite3-shm"), b"shm").unwrap();

        let dead = check_database(&db, false);
        assert_eq!(dead[0].status, ArtifactStatus::Present);
        assert_eq!(dead.len(), 3);
        assert!(dead[1..]
            .iter()
            .all(|check| check.status == ArtifactStatus::Stale));

        let alive = check_database(&db, true);
        assert!(alive[1..]
            .iter()
            .all(|check| check.status == ArtifactStatus::Active));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logs_counted_not_deleted() {
        let dir = temp_dir("logs");
        assert_eq!(check_logs(&dir).status, ArtifactStatus::Clean);
        std::fs::write(dir.join("daemon.log"), b"0123456789").unwrap();
        let check = check_logs(&dir);
        assert_eq!(check.status, ArtifactStatus::Present);
        assert!(check.detail.contains("1 *.log file(s), 10 bytes"));
        assert!(dir.join("daemon.log").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emergency_reserve_states() {
        let dir = temp_dir("reserve");
        let path = dir.join(EMERGENCY_RESERVE_FILE_NAME);
        assert_eq!(
            check_emergency_reserve(&path, 1024).status,
            ArtifactStatus::Clean
        );
        std::fs::write(&path, vec![0u8; 1024]).unwrap();
        assert_eq!(
            check_emergency_reserve(&path, 1024).status,
            ArtifactStatus::Present
        );
        let check = check_emergency_reserve(&path, 2048);
        assert_eq!(check.status, ArtifactStatus::Stale);
        assert!(check.detail.contains("1024"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shadow_dir_states() {
        let dir = temp_dir("shadow");
        assert_eq!(
            check_shadow_dir(&dir.join("missing"), false).status,
            ArtifactStatus::Clean
        );
        assert_eq!(check_shadow_dir(&dir, false).status, ArtifactStatus::Clean);
        std::fs::write(dir.join("daemon-abc123.exe"), b"x").unwrap();
        assert_eq!(
            check_shadow_dir(&dir, false).status,
            ArtifactStatus::Present
        );
        assert_eq!(check_shadow_dir(&dir, true).status, ArtifactStatus::Active);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_exit_code_and_findings() {
        let mut report = ArtifactReport::default();
        report.checks.push(ArtifactCheck::new(
            "pid-file",
            "p",
            ArtifactStatus::Stale,
            "d",
        ));
        assert_eq!(report.finding_count(), 1);
        assert_eq!(report.exit_code(), 0);
        report
            .checks
            .push(ArtifactCheck::new("logs", "p", ArtifactStatus::Error, "d"));
        assert_eq!(report.exit_code(), 1);
        let json = report.to_json_value();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["findings"], 2);
        assert_eq!(json["checks"].as_array().unwrap().len(), 2);
        let text = report.render_text();
        assert!(text.contains("cleanup verify: 2 location(s) — 2 finding(s)"));
    }

    #[test]
    fn from_environment_creates_nothing() {
        // Read-only contract: deriving paths must not create directories.
        let paths = ArtifactPaths::from_environment(Some("0123456789abcdef"));
        assert!(paths
            .pid_file
            .to_string_lossy()
            .contains("0123456789abcdef"));
        assert!(paths.db.to_string_lossy().contains("0123456789abcdef"));
    }

    #[cfg(feature = "daemon")]
    #[test]
    fn reserve_constants_match_daemon_module() {
        assert_eq!(
            EMERGENCY_RESERVE_FILE_NAME,
            crate::daemon::emergency_reserve::EMERGENCY_RESERVE_FILE_NAME
        );
        assert_eq!(
            EMERGENCY_RESERVE_BYTES,
            crate::daemon::emergency_reserve::EMERGENCY_RESERVE_BYTES
        );
    }
}
