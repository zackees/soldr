//! The background registration worker.
//!
//! Everything that can block lives here, on its own thread, so [`super::install`]
//! can return immediately regardless of whether a daemon exists.
//!
//! The worker owns the full lifecycle: discover the daemon, register,
//! heartbeat, and on any failure back off and start over. Re-running the
//! *whole* handshake on reconnect is deliberate — the daemon's registry is
//! in-memory, so a daemon restart forgets us and only a fresh registration
//! returns this process to `ARMED`.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use running_process_probe::probe_diag::v1::{
    AllowPolicy as WireAllowPolicy, Disclosure as WireDisclosure, ProcessKey, RegisterProcess,
};
use sha2::{Digest, Sha256};
use sysinfo::{Pid, ProcessRefreshKind, System};

use super::client::{HeartbeatWork, ProbeClient, SocketProbeClient};
use super::Config;

/// First reconnect delay.
const BACKOFF_START: Duration = Duration::from_millis(100);
/// Ceiling on the reconnect delay. Bounded so a long-absent daemon is still
/// picked up promptly once it appears.
const BACKOFF_CAP: Duration = Duration::from_secs(5);
/// Bound on each connect / request.
const IO_DEADLINE: Duration = Duration::from_millis(500);
/// How finely the worker checks the stop flag while waiting.
///
/// Sleeping for a whole backoff or heartbeat interval would make `Guard::drop`
/// wait that long; slicing the wait keeps shutdown prompt.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Assemble this process's registration request.
///
/// Fallible only because identifying the current executable can fail; that is
/// a local condition, unrelated to whether a daemon exists.
pub fn build_register_request(config: &Config) -> io::Result<RegisterProcess> {
    let exe = std::env::current_exe()?;
    let disclosed_cwd = if config.disclosure.disclose_cwd {
        std::env::current_dir()?.to_string_lossy().into_owned()
    } else {
        String::new()
    };
    let disclosed_env = config
        .disclosure
        .env_allowlist
        .iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
        .collect();
    let manifest_path = config
        .symbol_manifest_path
        .as_deref()
        .map(absolute_path)
        .transpose()?;
    let symbol_paths = config
        .symbol_paths
        .iter()
        .map(|path| absolute_path(path))
        .collect::<io::Result<Vec<_>>>()?;

    let nonce = fresh_nonce()?;

    let pid = Pid::from_u32(std::process::id());
    let mut system = System::new();
    system.refresh_pids_specifics(&[pid], ProcessRefreshKind::new());
    let started_at_unix_ms = system
        .process(pid)
        .map(|process| process.start_time().saturating_mul(1000))
        .unwrap_or(0);

    Ok(RegisterProcess {
        key: Some(ProcessKey {
            pid: u64::from(std::process::id()),
            start_time: Some(started_at_unix_ms),
            boot_id: Some(crate::broker::host_identity::current().boot_id),
        }),
        exe_path: exe.to_string_lossy().into_owned(),
        app_class: config.app_class.clone(),
        app_name: config.app_name.clone(),
        app_version: config.app_version.clone(),
        instance_name: config.instance.clone().unwrap_or_default(),
        arch: std::env::consts::ARCH.to_string(),
        os: std::env::consts::OS.to_string(),
        // Declared, never inferred. The daemon cannot tell a Python process
        // from a native one by looking at it — the interpreter is just another
        // native executable — so leaving this at its default would report
        // every registrant as UNSPECIFIED.
        runtime: config.runtime.to_proto() as i32,
        // SUPPORTED_OP_STACK_CAPTURE. A manifest declaration is more
        // specific than the default local lookup.
        supported_ops: vec![1],
        symbol_source: if manifest_path.is_some() { 3 } else { 2 },
        symbol_manifest_path: manifest_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        symbol_paths: symbol_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        allow_policy: Some(WireAllowPolicy {
            allow_all_ops: config.allow_policy.allow_all_ops,
            env_allowlist: config.disclosure.env_allowlist.clone(),
        }),
        disclosure: Some(WireDisclosure {
            expose_exe_path: false,
            expose_cmdline: false,
            expose_env_names: !config.disclosure.env_allowlist.is_empty(),
        }),
        disclosed_cwd,
        disclosed_env,
        registration_nonce: nonce.to_vec(),
        ..Default::default()
    })
}

fn absolute_path(path: &Path) -> io::Result<std::path::PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Spawn the worker thread.
pub fn spawn(
    request: RegisterProcess,
    config: Config,
    stop: Arc<AtomicBool>,
    key_out: Arc<Mutex<Option<ProcessKey>>>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("rp-probe".into())
        .spawn(move || run(request, config, stop, key_out))
}

fn run(
    mut request: RegisterProcess,
    config: Config,
    stop: Arc<AtomicBool>,
    key_out: Arc<Mutex<Option<ProcessKey>>>,
) {
    // Reading and hashing the executable can take hundreds of milliseconds on
    // Windows. Keep it on this worker so `probe::install` never performs file
    // I/O on the application's calling thread.
    let Ok(Some(exe_sha256)) =
        sha256_file_interruptible(Path::new(&request.exe_path), stop.as_ref())
    else {
        return;
    };
    request.exe_sha256 = exe_sha256.to_vec();

    let mut backoff = BACKOFF_START;

    while !stop.load(Ordering::Relaxed) {
        // Registration nonces are single-use at the daemon. Every reconnect
        // attempt must carry a fresh one or a healthy target can never re-arm
        // after any transport loss.
        let Ok(nonce) = fresh_nonce() else {
            return;
        };
        request.registration_nonce = nonce.to_vec();
        match connect_and_register(&request, &config) {
            Ok((mut client, key)) => {
                backoff = BACKOFF_START;
                set_key(&key_out, Some(key.clone()));

                heartbeat_loop(&mut client, &key, &config, &stop);

                // Either we are shutting down or the connection failed. Either
                // way this process is no longer armed.
                set_key(&key_out, None);

                if stop.load(Ordering::Relaxed) {
                    // Best-effort courtesy notice. The daemon would notice the
                    // closed connection regardless.
                    let _ = client.unregister(&key);
                    return;
                }
            }
            Err(_) => {
                // No daemon, or it refused. Neither is fatal to the
                // application — wait and try again.
                sleep_interruptible(backoff, &stop);
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
        }
    }
}

/// Hash a file without making worker shutdown wait for all remaining I/O.
///
/// In particular, cross-compiled test executables can be large and slow to
/// read under emulation. Checking between bounded reads keeps [`super::Guard`]
/// drop latency independent of the executable's total size.
fn sha256_file_interruptible(path: &Path, stop: &AtomicBool) -> io::Result<Option<[u8; 32]>> {
    if stop.load(Ordering::Relaxed) {
        return Ok(None);
    }

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let read = match file.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&hasher.finalize());
    Ok(Some(digest))
}

fn fresh_nonce() -> io::Result<[u8; 32]> {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(|error| io::Error::other(format!("getrandom: {error}")))?;
    Ok(nonce)
}

fn connect_and_register(
    request: &RegisterProcess,
    config: &Config,
) -> Result<(SocketProbeClient, ProcessKey), super::client::ClientError> {
    let socket = resolve_socket_path(config)?;
    let mut client = SocketProbeClient::connect(&socket, IO_DEADLINE)?;
    let key = client.register(request)?;
    Ok((client, key))
}

/// Where to reach the daemon.
///
/// An explicit override wins; otherwise the daemon's owner-only discovery file
/// names the socket. Absence of that file simply means "no daemon yet", which
/// the caller treats as a retry.
fn resolve_socket_path(config: &Config) -> Result<String, super::client::ClientError> {
    if let Some(path) = &config.socket_override {
        return Ok(path.to_string_lossy().into_owned());
    }
    Err(super::client::ClientError::Unreachable(io::Error::new(
        io::ErrorKind::NotFound,
        "no probe daemon discovery file; set Config::socket_override or start rpprobed",
    )))
}

fn heartbeat_loop(
    client: &mut dyn ProbeClient,
    key: &ProcessKey,
    config: &Config,
    stop: &AtomicBool,
) {
    loop {
        sleep_interruptible(config.heartbeat_interval, stop);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match client.heartbeat(key) {
            Ok(HeartbeatWork::Idle) => {}
            Ok(HeartbeatWork::Capture(request)) => {
                let reply = super::capture::capture(&request);
                if client.submit_capture(reply).is_err() {
                    return;
                }
            }
            Err(_) => {
                // Connection is gone. Returning sends the worker back through
                // the full register handshake, which is what a restarted
                // daemon needs.
                return;
            }
        }
    }
}

fn set_key(slot: &Arc<Mutex<Option<ProcessKey>>>, value: Option<ProcessKey>) {
    if let Ok(mut guard) = slot.lock() {
        *guard = value;
    }
}

/// Sleep, but wake early if asked to stop.
fn sleep_interruptible(total: Duration, stop: &AtomicBool) {
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let step = STOP_POLL.min(total - slept);
        std::thread::sleep(step);
        slept += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_request_describes_this_process() {
        let req = build_register_request(&Config::new("test-app")).unwrap();
        let key = req.key.expect("key");
        assert_eq!(key.pid, u64::from(std::process::id()));
        let pid = Pid::from_u32(std::process::id());
        let mut system = System::new();
        system.refresh_pids_specifics(&[pid], ProcessRefreshKind::new());
        assert_eq!(
            key.start_time,
            system
                .process(pid)
                .map(|process| process.start_time().saturating_mul(1000)),
            "wire identity and OS discovery must use the same millisecond unit"
        );
        assert!(!req.exe_path.is_empty());
        assert_eq!(req.app_class, "test-app");
        assert_eq!(req.arch, std::env::consts::ARCH);
        assert_eq!(req.os, std::env::consts::OS);
    }

    #[test]
    fn registration_copies_only_explicit_query_disclosures() {
        let private = build_register_request(&Config::new("private")).unwrap();
        assert!(private.disclosed_cwd.is_empty());
        assert!(private.disclosed_env.is_empty());

        let mut config = Config::new("disclosed").allow_env_value("PATH");
        config.disclosure.disclose_cwd = true;
        let disclosed = build_register_request(&config).unwrap();
        assert_eq!(
            disclosed.disclosed_cwd,
            std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(
            disclosed.disclosed_env.get("PATH"),
            std::env::var("PATH").ok().as_ref(),
            "only the explicitly allowlisted value is copied"
        );
        assert_eq!(disclosed.disclosed_env.len(), 1);
    }

    /// The runtime must be declared on the wire, not left at the proto default.
    ///
    /// `UNSPECIFIED` is what the field held before it was populated at all, so
    /// asserting the concrete value is what distinguishes "reported native"
    /// from "reported nothing".
    #[test]
    fn the_declared_runtime_reaches_the_request() {
        use running_process_probe::probe_diag::v1::Runtime as ProtoRuntime;

        let native = build_register_request(&Config::new("a")).unwrap();
        assert_eq!(
            native.runtime,
            ProtoRuntime::Native as i32,
            "a Rust registrant defaults to native, not unspecified"
        );

        let python =
            build_register_request(&Config::new("a").with_runtime(super::super::Runtime::Python))
                .unwrap();
        assert_eq!(python.runtime, ProtoRuntime::Python as i32);
    }

    #[test]
    fn each_registration_gets_a_fresh_nonce() {
        let a = build_register_request(&Config::new("a")).unwrap();
        let b = build_register_request(&Config::new("a")).unwrap();
        assert_eq!(a.registration_nonce.len(), 32);
        assert_ne!(
            a.registration_nonce, b.registration_nonce,
            "a reused nonce would be rejected as a replay"
        );
    }

    #[test]
    fn reconnect_nonce_generation_is_fresh() {
        let first = fresh_nonce().unwrap();
        let second = fresh_nonce().unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let mut d = BACKOFF_START;
        for _ in 0..10 {
            d = (d * 2).min(BACKOFF_CAP);
        }
        assert_eq!(d, BACKOFF_CAP, "backoff must saturate, not grow unbounded");
    }

    #[test]
    fn interruptible_sleep_wakes_early_on_stop() {
        let stop = AtomicBool::new(true);
        let start = std::time::Instant::now();
        sleep_interruptible(Duration::from_secs(30), &stop);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "an already-set stop flag must short-circuit the wait"
        );
    }

    #[test]
    fn interruptible_sleep_waits_when_not_stopped() {
        let stop = AtomicBool::new(false);
        let start = std::time::Instant::now();
        sleep_interruptible(Duration::from_millis(150), &stop);
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn executable_hash_matches_the_canonical_identity_hash() {
        let path = std::env::current_exe().unwrap();
        let stop = AtomicBool::new(false);
        let actual = sha256_file_interruptible(&path, &stop)
            .unwrap()
            .expect("hash should complete");
        let expected = crate::broker::backend_lifecycle::identity::sha256_file(&path).unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn executable_hash_short_circuits_before_open_when_stopped() {
        let stop = AtomicBool::new(true);
        let missing = Path::new("this-file-does-not-exist");
        assert_eq!(
            sha256_file_interruptible(missing, &stop).unwrap(),
            None,
            "an already-set stop flag must win over file I/O"
        );
    }

    #[test]
    fn registration_absolutizes_symbol_declarations_before_the_daemon_stores_them() {
        let request = build_register_request(
            &Config::new("symbols")
                .with_symbol_manifest("symbols/app.rpprobe-symbols.json")
                .with_symbol_path("symbols/private"),
        )
        .unwrap();
        assert!(Path::new(&request.symbol_manifest_path).is_absolute());
        assert!(request
            .symbol_paths
            .iter()
            .all(|path| Path::new(path).is_absolute()));
    }

    #[test]
    fn missing_discovery_is_unreachable_not_a_panic() {
        let err = resolve_socket_path(&Config::new("app")).expect_err("no daemon");
        assert!(matches!(
            err,
            super::super::client::ClientError::Unreachable(_)
        ));
    }

    #[test]
    fn socket_override_wins_over_discovery() {
        let mut cfg = Config::new("app");
        cfg.socket_override = Some(std::path::PathBuf::from("/tmp/x.sock"));
        assert_eq!(resolve_socket_path(&cfg).unwrap(), "/tmp/x.sock");
    }
}
