#![cfg(feature = "daemon")]
//! Phase 5 of #222 (#428) — integration tests for the `runpm.toml`
//! batch-start config file.
//!
//! Pure parser tests are duplicated here from the unit tests in
//! `src/runpm_config.rs` so the integration test file owns the
//! contract end-to-end (parser + daemon spawn). The end-to-end
//! `multi_app_config_starts_every_app_in_daemon` test spins up a real
//! `DaemonServer`, writes a 3-app TOML config to a tempdir, and
//! verifies the daemon registers all 3 services.

use std::path::{Path, PathBuf};

use running_process::daemon::client::DaemonClient;
use running_process::daemon::paths;
use running_process::daemon::server::DaemonServer;
use running_process::proto::daemon::{ServiceConfig, StatusCode};
use running_process::runpm_config::{RunpmConfig, RunpmConfigError};

/// Build a unique scope string for each test to avoid socket conflicts.
macro_rules! test_scope {
    () => {
        format!("runpm-toml-{}", line!())
    };
}

// ---------------------------------------------------------------------------
// Parser tests — these run on every platform, no daemon needed.
// ---------------------------------------------------------------------------

#[test]
fn parses_minimal_single_app_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("runpm.toml");
    std::fs::write(
        &path,
        r#"
[[app]]
name = "web"
cmd  = ["node", "server.js"]
"#,
    )
    .expect("write");

    let cfg = RunpmConfig::load(&path).expect("parse ok");
    assert_eq!(cfg.app.len(), 1);
    assert_eq!(cfg.app[0].name, "web");
    assert_eq!(cfg.app[0].cmd, vec!["node", "server.js"]);
    // Defaults
    assert!(cfg.app[0].autorestart);
    assert_eq!(cfg.app[0].max_restarts, None);
    assert!(cfg.app[0].env.is_empty());
    assert_eq!(cfg.app[0].cwd, None);
}

#[test]
fn parses_full_config_with_env_and_cwd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("runpm.toml");
    std::fs::write(
        &path,
        r#"
[[app]]
name = "web"
cmd  = ["node", "server.js"]
cwd  = "services/web"
env  = { NODE_ENV = "production", PORT = "8080" }
autorestart      = false
max_restarts     = 10
restart_delay_ms = 1000
min_uptime_ms    = 2000
kill_timeout_ms  = 7500
"#,
    )
    .expect("write");

    let cfg = RunpmConfig::load(&path).expect("parse ok");
    let app = &cfg.app[0];
    assert_eq!(app.cwd.as_deref(), Some("services/web"));
    assert_eq!(
        app.env.get("NODE_ENV").map(String::as_str),
        Some("production")
    );
    assert_eq!(app.env.get("PORT").map(String::as_str), Some("8080"));
    assert!(!app.autorestart);
    assert_eq!(app.max_restarts, Some(10));
    assert_eq!(app.restart_delay_ms, Some(1000));
    assert_eq!(app.min_uptime_ms, Some(2000));
    assert_eq!(app.kill_timeout_ms, Some(7500));

    // Relative cwd resolves against the config file's parent.
    let resolved = RunpmConfig::resolve_cwd(&path, &app.cwd).expect("resolve");
    assert!(
        Path::new(&resolved).is_absolute(),
        "resolved cwd should be absolute (parent + relative); got {resolved}"
    );
    assert!(resolved.ends_with("web"));
}

#[test]
fn rejects_empty_cmd_with_clear_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("runpm.toml");
    std::fs::write(
        &path,
        r#"
[[app]]
name = "broken"
cmd  = []
"#,
    )
    .expect("write");

    let err = RunpmConfig::load(&path).expect_err("must reject");
    assert!(matches!(err, RunpmConfigError::EmptyCmd { .. }));
    let msg = err.to_string();
    assert!(msg.contains("broken"), "must mention app name; got: {msg}");
}

#[test]
fn rejects_duplicate_app_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("runpm.toml");
    std::fs::write(
        &path,
        r#"
[[app]]
name = "web"
cmd  = ["a"]

[[app]]
name = "web"
cmd  = ["b"]
"#,
    )
    .expect("write");

    let err = RunpmConfig::load(&path).expect_err("must reject");
    assert!(matches!(err, RunpmConfigError::DuplicateName { .. }));
    let msg = err.to_string();
    assert!(msg.contains("web"), "must mention name; got: {msg}");
}

// ---------------------------------------------------------------------------
// End-to-end: spin up a real daemon, write a 3-app config, register them.
// ---------------------------------------------------------------------------

/// Pick a cross-platform long-lived command so the spawned children stay
/// alive long enough for `service_list` to observe them. Mirrors
/// `daemon_runpm_service_stubs.rs::long_lived_cmd()`.
fn long_lived_cmd() -> Vec<String> {
    #[cfg(windows)]
    {
        vec![
            "cmd".into(),
            "/C".into(),
            "ping -n 300 127.0.0.1 > NUL".into(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec!["sleep".into(), "300".into()]
    }
}

fn start_server(scope: &str) -> (tokio::task::JoinHandle<()>, String) {
    let socket = paths::socket_path(Some(scope));
    let db = paths::db_path(Some(scope)).to_string_lossy().into_owned();

    let server = DaemonServer::new(
        socket.clone(),
        db,
        "test".to_string(),
        scope.to_string(),
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    )
    .expect("failed to create DaemonServer");

    let handle = tokio::spawn(async move {
        server.run().await.expect("server.run() failed");
    });

    (handle, socket)
}

/// Render a 3-app `runpm.toml` to a path. Names + commands are
/// distinct so we can assert each one registered.
fn write_three_app_config(dir: &Path) -> PathBuf {
    let path = dir.join("runpm.toml");
    let cmd = long_lived_cmd();
    // TOML array-of-strings literal — quote each entry.
    let cmd_literal = cmd
        .iter()
        .map(|s| format!("{:?}", s))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        r#"
[[app]]
name = "alpha"
cmd  = [{cmd_literal}]
autorestart = false

[[app]]
name = "bravo"
cmd  = [{cmd_literal}]
autorestart = false
env  = {{ NODE_ENV = "test" }}

[[app]]
name = "charlie"
cmd  = [{cmd_literal}]
autorestart = false
max_restarts = 3
"#
    );
    std::fs::write(&path, body).expect("write three-app config");
    path
}

/// Mirror of the binary's batch-start helper, copy/pasted into the test
/// because `src/bin/runpm.rs` is a binary (not a lib) and can't be
/// imported directly. The helper itself is two trivial loops over the
/// daemon RPC — what we actually want to validate is the *config →
/// daemon* round-trip, and this is the cheapest way to do it.
fn batch_start(client: &mut DaemonClient, config_path: &Path, cfg: &RunpmConfig) -> (usize, usize) {
    let mut started = 0;
    let mut failed = 0;
    for app in &cfg.app {
        let cwd = RunpmConfig::resolve_cwd(config_path, &app.cwd).unwrap_or_default();
        let svc = ServiceConfig {
            name: app.name.clone(),
            cmd: app.cmd.clone(),
            cwd,
            env: app.env.clone(),
            autorestart: app.autorestart,
            max_restarts: app.max_restarts.unwrap_or(0),
            restart_delay_ms: app.restart_delay_ms.unwrap_or(0),
            kill_timeout_ms: app.kill_timeout_ms.unwrap_or(500),
            min_uptime_ms: app.min_uptime_ms.unwrap_or(0),
        };
        match client.service_start(svc) {
            Ok(resp) if resp.code == StatusCode::Ok as i32 => started += 1,
            Ok(resp) => {
                eprintln!("daemon refused {}: {}", app.name, resp.message);
                failed += 1;
            }
            Err(e) => {
                eprintln!("rpc error starting {}: {e}", app.name);
                failed += 1;
            }
        }
    }
    (started, failed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_app_config_starts_every_app_in_daemon() {
    let scope = test_scope!();
    let (server_handle, socket) = start_server(&scope);

    // Give the server a moment to bind the socket.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = write_three_app_config(dir.path());

    let result = tokio::task::spawn_blocking(move || {
        let cfg = RunpmConfig::load(&config_path).expect("config must parse");
        assert_eq!(cfg.app.len(), 3, "test fixture should declare 3 apps");

        let mut client = DaemonClient::connect_to(&socket).expect("failed to connect to daemon");

        let (started, failed) = batch_start(&mut client, &config_path, &cfg);
        assert_eq!(failed, 0, "every app should start");
        assert_eq!(started, 3, "exactly 3 apps should have started");

        // Daemon should now have all three services registered.
        let resp = client.service_list().expect("service_list failed");
        assert_eq!(
            resp.code,
            StatusCode::Ok as i32,
            "service_list should be OK"
        );
        let services = resp.service_list.expect("list payload").services;
        assert_eq!(services.len(), 3, "all 3 apps should be in the registry");

        let mut names: Vec<String> = services.iter().map(|s| s.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);

        // The bravo entry should have the env overlay applied.
        let bravo = services.iter().find(|s| s.name == "bravo").expect("bravo");
        let env = bravo
            .config
            .as_ref()
            .map(|c| c.env.clone())
            .unwrap_or_default();
        assert_eq!(env.get("NODE_ENV").map(String::as_str), Some("test"));

        // charlie should carry the max_restarts override.
        let charlie = services
            .iter()
            .find(|s| s.name == "charlie")
            .expect("charlie");
        assert_eq!(
            charlie.config.as_ref().map(|c| c.max_restarts).unwrap_or(0),
            3,
        );

        // Tear everything down so the supervised children stop.
        let _ = client.service_delete("all");

        let _ = client.shutdown(true, 5.0);
    })
    .await;
    result.expect("client task panicked");

    tokio::time::timeout(std::time::Duration::from_secs(10), server_handle)
        .await
        .expect("server did not stop within 10 seconds")
        .expect("server task panicked");
}
