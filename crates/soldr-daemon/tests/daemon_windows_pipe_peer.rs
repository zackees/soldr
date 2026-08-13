#![cfg(windows)]
//! Windows-only integration tests for named-pipe peer identity and
//! shutdown-request attribution. Moved out of `daemon/ipc_peer.rs`
//! when that file became host-neutral (#2493): the pipe machinery this
//! exercises is inherently Windows-specific.

use soldr_daemon::timed_test;

timed_test!(accepted_pipe_reports_the_os_observed_client, {
    use soldr_daemon::cache_lib::daemon_lifecycle_log_path;
    use soldr_daemon::core::SoldrPaths;
    use soldr_daemon::daemon::ipc_peer::PeerIdentity;
    use soldr_daemon::daemon::lifecycle::LifecycleSource;
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let pipe_name = format!(
            r"\\.\pipe\soldr-ipc-peer-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .expect("server");
        let _client = ClientOptions::new().open(&pipe_name).expect("client");
        server.connect().await.expect("connect");

        let mut server = server;
        let peer = PeerIdentity::from_windows_pipe_server(&mut server);
        assert_eq!(peer.pid, Some(std::process::id()));
        assert_eq!(peer.exe, None, "the hot accept path must not resolve exe");
        assert_eq!(peer.source, LifecycleSource::IpcPeer);
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("root"));
        peer.record_shutdown_requested(&paths, 99);
        let lifecycle =
            std::fs::read_to_string(daemon_lifecycle_log_path(&paths)).expect("lifecycle");
        let event: serde_json::Value = serde_json::from_str(lifecycle.trim()).expect("event json");
        let expected = std::env::current_exe()
            .expect("current exe")
            .canonicalize()
            .expect("canonical current exe");
        let observed =
            std::path::PathBuf::from(event["requester_exe"].as_str().expect("peer executable"))
                .canonicalize()
                .expect("canonical peer executable");
        assert_eq!(observed, expected);
    });
});
