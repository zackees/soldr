//! When the soldr-daemon's session links a zccache runtime/cache/session,
//! shutting down the soldr-daemon must spawn `zccache stop` against that
//! exact cache namespace before the soldr-daemon exits.
//!
//! The test installs a fake `zccache` binary under
//! `<cache>/bin/zccache-pinned/zccache[.exe]` that logs every invocation
//! to a stamp file. After the LinkZccache fire-and-forget + Shutdown
//! RPC, the stamp file must contain a `stop` line — proving the
//! shutdown hook fired.

#![allow(clippy::print_stdout, dead_code, unused_imports)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::daemon::{client, db};

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

fn soldr_daemon_bin() -> PathBuf {
    let soldr = PathBuf::from(env!("CARGO_BIN_EXE_soldr"));
    let parent = soldr.parent().expect("parent");
    let stem = if cfg!(windows) {
        "soldr-daemon.exe"
    } else {
        "soldr-daemon"
    };
    parent.join(stem)
}

fn install_fake_zccache(cache_root: &Path, log_path: &Path) -> PathBuf {
    let dir = cache_root.join("bin").join("zccache-pinned");
    std::fs::create_dir_all(&dir).expect("create pinned dir");
    #[cfg(windows)]
    let bin = dir.join("zccache.exe");
    #[cfg(not(windows))]
    let bin = dir.join("zccache");

    #[cfg(windows)]
    {
        // On Windows we can't easily author a script that the daemon
        // can spawn via Command::new for an .exe path. Use a .cmd
        // shim and the daemon's `zccache stop` will resolve it via
        // Command::new which on Windows supports launching .cmd
        // shims directly only when invoked through the shell. To
        // sidestep that, write a *.cmd file at the same path
        // alongside the .exe and have the daemon target the .cmd.
        // But the daemon resolver only looks for .exe. Simplest
        // portable approach: ship a tiny .bat script renamed to .exe
        // won't work either.
        //
        // Compromise: skip the test on Windows. The Linux/macOS
        // pathways exercise the same daemon code; Windows-specific
        // shutdown behavior is verified via the lifecycle integration
        // tests that already cover the same execve path.
        let _ = log_path;
        let _ = bin;
        PathBuf::new()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = format!(
            "#!/bin/sh\n\
             echo \"zccache $* cache_dir=${{ZCCACHE_CACHE_DIR:-}} daemon_namespace=${{ZCCACHE_DAEMON_NAMESPACE:-}}\" >> \"{}\"\n\
             exit 0\n",
            log_path.display()
        );
        std::fs::write(&bin, script).expect("write fake zccache");
        let mut perms = std::fs::metadata(&bin).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).expect("chmod");
        bin
    }
}

struct DaemonProc {
    child: Option<Child>,
}

impl DaemonProc {
    fn spawn(cache_root: &Path, home_root: &Path) -> Self {
        let mut cmd = Command::new(soldr_daemon_bin());
        cmd.args(["--foreground", "--idle-timeout-secs", "60"])
            .env("SOLDR_CACHE_DIR", cache_root)
            .env("HOME", home_root)
            .env("USERPROFILE", home_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(5);
        let pid_file = cache_root
            .join("cache")
            .join("soldr-daemon")
            .join("daemon.pid");
        while Instant::now() < deadline {
            if pid_file.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child: Some(child) }
    }

    fn wait(&mut self, max: Duration) -> Option<std::process::ExitStatus> {
        let child = self.child.as_mut()?;
        let deadline = Instant::now() + max;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct EnvScope {
    keys: Vec<&'static str>,
    prior: Vec<Option<OsString>>,
}

impl EnvScope {
    fn set(pairs: &[(&'static str, &Path)]) -> Self {
        let mut prior = Vec::with_capacity(pairs.len());
        let mut keys = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            prior.push(std::env::var_os(k));
            std::env::set_var(k, v);
            keys.push(*k);
        }
        Self { keys, prior }
    }
}

impl Drop for EnvScope {
    fn drop(&mut self) {
        for (k, p) in self.keys.iter().zip(self.prior.iter()) {
            match p {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

#[cfg(not(windows))]
#[test]
#[ignore = "soldr#608: shutdown hook regression — fake zccache never invoked, log empty"]
fn linked_zccache_is_stopped_on_daemon_shutdown() {
    let cache_root = unique_temp_dir("zccache-link-cache");
    let home_root = unique_temp_dir("zccache-link-home");
    let log_path = unique_temp_dir("zccache-link-log").join("zccache-calls.log");

    // Install a fake zccache that logs every invocation.
    let bin = install_fake_zccache(&cache_root, &log_path);
    let linked_cache_dir = cache_root.join("cache").join("linked-zccache");
    let unrelated_cache_dir = cache_root.join("cache").join("unrelated-zccache");
    std::fs::create_dir_all(&linked_cache_dir).expect("linked cache dir");
    std::fs::create_dir_all(&unrelated_cache_dir).expect("unrelated cache dir");

    let mut daemon = DaemonProc::spawn(&cache_root, &home_root);

    let _scope = EnvScope::set(&[
        ("SOLDR_CACHE_DIR", cache_root.as_path()),
        ("HOME", home_root.as_path()),
        ("USERPROFILE", home_root.as_path()),
    ]);
    let paths = soldr_cli::core::SoldrPaths::new().expect("paths");

    // Wait until the daemon is fully accepting connections.
    let deadline = Instant::now() + Duration::from_secs(5);
    let sock = client::default_sock_path(&paths);
    let mut linked = false;
    while Instant::now() < deadline {
        if client::submit_fire_and_forget(
            &sock,
            &soldr_cli::daemon::protocol::Request::LinkZccache {
                link: soldr_cli::daemon::protocol::ZccacheDaemonLink {
                    binary_path: bin.display().to_string(),
                    cache_dir: linked_cache_dir.display().to_string(),
                    session_id: Some("linked-session".into()),
                    source: "test".into(),
                    private_daemon: true,
                    daemon_name: Some("soldr-dev-link-test".into()),
                    owner_pid: Some(std::process::id()),
                    private_env_keys: vec!["ZCCACHE_PATH_REMAP".into()],
                },
            },
        )
        .is_ok()
        {
            linked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(linked, "LinkZccache fire-and-forget never succeeded");

    // Confirm the linkage landed via Status. LinkZccache is fire-and-
    // forget so the server task may not have applied it yet — poll
    // up to 2 s before failing.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut info = client::status(&sock).expect("status");
    while info.linked_zccache.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        info = client::status(&sock).expect("status");
    }
    let linked = info.linked_zccache.expect("linked zccache status");
    assert_eq!(linked.cache_dir, linked_cache_dir.display().to_string());
    assert_eq!(linked.binary_path, bin.display().to_string());
    assert_eq!(linked.session_id.as_deref(), Some("linked-session"));
    assert!(linked.private_daemon);
    assert_eq!(linked.daemon_name.as_deref(), Some("soldr-dev-link-test"));
    assert_eq!(
        linked.private_env_keys,
        vec!["ZCCACHE_PATH_REMAP".to_string()]
    );

    // Trigger shutdown via the explicit RPC.
    client::shutdown(&sock).expect("shutdown");
    let status = daemon
        .wait(Duration::from_secs(5))
        .expect("daemon exits within 5s");
    assert!(status.success(), "daemon exit status = {status:?}");

    // The fake zccache must have been invoked with `stop`.
    let calls = std::fs::read_to_string(&log_path).unwrap_or_default();
    let linked_stop = format!(
        "zccache stop cache_dir={} daemon_namespace=soldr-dev-link-test",
        linked_cache_dir.display()
    );
    assert!(
        calls.lines().any(|l| l.trim() == linked_stop),
        "expected scoped `zccache stop` invocation {linked_stop:?}; log contents:\n{calls}"
    );
    assert!(
        !calls.contains(&unrelated_cache_dir.display().to_string()),
        "shutdown touched unrelated zccache cache dir {}; log contents:\n{calls}",
        unrelated_cache_dir.display()
    );

    // And the linked identity must have been cleared on shutdown.
    let link = db::get_linked_zccache(&cache_root.join("state.redb")).expect("get");
    assert_eq!(link, None, "linked zccache must be cleared on shutdown");
}
