//! Real-rustc SESSION anchor (soldr#2388 Step 8 data path): a **real compile**
//! flows client → in-process broker relay → **real** daemon SESSION endpoint
//! (embedded zccache) → real rustc, and the cache actually engages — the first
//! run is a **miss**, an identical second run is a **hit**.
//!
//! This stitches the three separately-proven pieces into the production data
//! path (everything but process-spawning): the transport (Step 7 mock relay
//! e2e), the daemon codec-bridge endpoint with real rustc (Step 6d
//! `session_endpoint` tests), and the client (`run_session_compile`). It proves
//! the actual value proposition — cached compiles over SESSION.

use std::sync::Arc;
use std::time::Duration;

use running_process::broker::backend_handle::DaemonProcess;
use running_process::broker::protocol::Endpoint;
use running_process::broker::protocol_v2::SessionEnvVar;

use soldr_cli::core::SoldrPaths;
use soldr_cli::daemon::session_endpoint::{
    bind_session_listener, serve_session_endpoint, soldr_session_endpoint_mux,
};
use soldr_cli::session_transport::{run_session_compile_with_detailed, spawn_session_relay};
use soldr_cli::zccache_embedded::SoldrZccacheService;

/// `CacheOutcome` discriminants (see `soldr_daemon::zccache_embedded::encode_cache_outcome`).
const CACHE_HIT: i32 = 1;
const CACHE_MISS: i32 = 2;

/// A unique daemon SESSION endpoint path for this test.
fn unique_endpoint_path() -> (String, Option<tempfile::TempDir>) {
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon-session.sock");
        (path.display().to_string(), Some(dir))
    }
    #[cfg(windows)]
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        (
            format!("soldr-session-real-e2e-{}-{}", std::process::id(), nanos),
            None,
        )
    }
}

soldr_cli::timed_test!(
    session_real_compile_miss_then_hit,
    Duration::from_secs(300),
    {
        // Real rustc + the repo's pinned toolchain (mirrors the Step 6d harness).
        let current_dir = std::env::current_dir().expect("cwd");
        let repo = current_dir
            .ancestors()
            .find(|c| c.join("rust-toolchain.toml").is_file())
            .expect("find repo rust-toolchain.toml");
        let pinned = soldr_cli::core::read_rust_toolchain_manifest(repo)
            .expect("read rust-toolchain.toml")
            .channel
            .expect("rust-toolchain.toml declares a channel");
        // Resolve rustc as the sibling of the `CARGO` binary cargo sets in the
        // test env — host-agnostic and dep-free (no rustup on PATH here, no
        // test-support dep). It is the pinned toolchain's rustc, and the daemon
        // runs it with RUSTUP_TOOLCHAIN=pinned set below.
        let rustc = {
            let cargo = std::env::var_os("CARGO").expect("CARGO set by cargo test");
            std::path::Path::new(&cargo)
                .with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX))
        };
        assert!(rustc.is_file(), "sibling rustc not found at {rustc:?}");

        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("workspace");
        std::fs::create_dir_all(project.join("src")).expect("create src dir");
        std::fs::write(
            project.join("src/lib.rs"),
            "pub fn session_real_answer() -> u32 { 4242 }\n",
        )
        .expect("write source");
        let root = temp.path().join("root");

        let program = format!("soldr-session-real-{}", std::process::id());
        let (endpoint_path, _guard) = unique_endpoint_path();

        // Daemon SESSION endpoint on its own thread + runtime: bind the listener
        // early (so the relay's dial always finds it), start the real embedded
        // zccache service, then serve.
        let endpoint_for_thread = endpoint_path.clone();
        let _endpoint_thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("endpoint runtime");
            rt.block_on(async move {
                let listener =
                    bind_session_listener(&endpoint_for_thread).expect("bind daemon endpoint");
                let identity = Endpoint {
                    namespace_id: "shared".into(),
                    path: "rpb-session-real-e2e".into(),
                };
                let daemon =
                    DaemonProcess::current_process(identity, Some(300)).expect("daemon identity");
                let paths = SoldrPaths::with_root(root);
                let service = Arc::new(
                    SoldrZccacheService::start(&paths, &daemon)
                        .await
                        .expect("start embedded zccache service"),
                );
                let mux = Arc::new(soldr_session_endpoint_mux(daemon));
                serve_session_endpoint(listener, service, paths, mux)
                    .await
                    .expect("serve daemon SESSION endpoint");
            });
        });

        // Broker companion relay → the daemon endpoint.
        spawn_session_relay(&program, endpoint_path.clone()).expect("spawn session relay");

        // Identical compile args for both runs (so the second is a cache hit).
        let args: Vec<String> = vec![
            "--edition".into(),
            "2021".into(),
            "--crate-type".into(),
            "lib".into(),
            "--crate-name".into(),
            "soldr_session_real".into(),
            "--emit=metadata".into(),
            "-C".into(),
            "metadata=sreal1".into(),
            "--out-dir".into(),
            "target/debug/deps".into(),
            "src/lib.rs".into(),
        ];
        let argv: Vec<String> = std::iter::once(rustc.as_path().display().to_string())
            .chain(args)
            .collect();
        let mut env: Vec<SessionEnvVar> = std::env::vars()
            .filter(|(k, _)| k != "RUSTUP_TOOLCHAIN")
            .map(|(key, value)| SessionEnvVar { key, value })
            .collect();
        env.push(SessionEnvVar {
            key: "RUSTUP_TOOLCHAIN".into(),
            value: pinned,
        });
        let cwd = project.display().to_string();

        // First compile: retry the dial until the relay + endpoint are up, then
        // assert a cache MISS.
        let mut first = None;
        for attempt in 0..80 {
            match run_session_compile_with_detailed(&program, &argv, cwd.clone(), env.clone()) {
                Ok(outcome) => {
                    first = Some(outcome);
                    break;
                }
                Err(err) => {
                    assert!(
                        attempt < 79 && !err.output_started,
                        "first SESSION compile never reached the daemon: {err}"
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        let first = first.expect("first compile outcome");
        assert_eq!(
            first.exit_code, 0,
            "real rustc compile must succeed over SESSION"
        );
        assert_eq!(
            first.cache_outcome,
            Some(CACHE_MISS),
            "first compile is a cache miss"
        );

        // Second identical compile: a cache HIT.
        let second = run_session_compile_with_detailed(&program, &argv, cwd, env)
            .expect("second compile outcome");
        assert_eq!(second.exit_code, 0, "warm compile must also succeed");
        assert_eq!(
            second.cache_outcome,
            Some(CACHE_HIT),
            "identical second compile is a cache hit over SESSION"
        );
    }
);
