//! The launcher must give up on a daemon that never binds (#500 slice 34).
//!
//! This is the broker's core safety property: a backend whose daemon starts
//! but never reaches its `bind` must fail the launch within the probe budget,
//! not hang the caller. Everything else the broker does assumes `launch`
//! returns.
//!
//! `testbin-sleeper` is the fixture precisely because it is a well-behaved
//! process that is useless as a daemon — it starts, prints its pid, and sleeps
//! forever. A crash-on-start fixture would not test the same thing: the
//! interesting case is a live child that simply never serves, because that is
//! the one where "wait a bit longer" is always superficially plausible.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use running_process::broker::protocol::ServiceDefinition;
use running_process::broker::server::backend_launcher::{
    BackendLaunchError, BackendLaunchRequest, BackendLauncher, CommandBackendLauncher,
};
use running_process::broker::server::{BackendKey, BrokerInstanceKey, TraceContext};

/// Locate a fixture binary built before the suite runs.
///
/// Deliberately does not build on demand: that takes cargo's build-directory
/// lock once per test process and presented as an unexplained multi-second
/// hang (#747).
fn testbin_path(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture `{name}` is missing at {}.
         Build the fixtures first:  soldr cargo build -p testbins",
        path.display()
    );
    path
}

#[test]
fn launch_fails_if_endpoint_not_probe_able_within_budget() {
    let sleeper = testbin_path("testbin-sleeper");
    let service_definition = ServiceDefinition {
        service_name: "never-binds".into(),
        binary_path: sleeper.display().to_string(),
        isolation: 1,
        explicit_instance: String::new(),
        // The directory the fixture actually lives in. `canonical_backend_binary`
        // refuses a binary outside this root, and an earlier revision of this
        // test used "." — so the launcher rejected the binary in 351µs, before
        // spawning anything, and the test passed while asserting nothing about
        // the probe budget.
        per_version_binary_dir: sleeper
            .parent()
            .expect("fixture has a parent directory")
            .display()
            .to_string(),
        min_version: "0.0.0".into(),
        version_allow_list: vec!["1.0.0".into()],
        labels: Default::default(),
    };
    let key = BackendKey::new(BrokerInstanceKey::Shared, "never-binds", "1.0.0");
    let trace_context = TraceContext::default();
    let request = BackendLaunchRequest {
        key: &key,
        service_definition: &service_definition,
        trace_context: &trace_context,
    };

    // 16 hex characters: the shape `BackendEndpointAllocator` expects.
    let launcher = CommandBackendLauncher::new("0123456789abcdef");

    let started = Instant::now();
    let result = launcher.launch(&request);
    let elapsed = started.elapsed();

    let error = result
        .err()
        .unwrap_or_else(|| panic!("a daemon that never binds must not produce a usable handle"));
    // The point of the test is the *probe* giving up. An error raised before
    // the spawn would satisfy `is_err()` while proving nothing, which is
    // exactly what the first revision of this test did.
    assert!(
        matches!(error, BackendLaunchError::BackendHandle(_)),
        "expected the endpoint probe to fail, got {error:?} —          if this is a validation error the launcher never spawned anything"
    );

    // The budget is `DEFAULT_ENDPOINT_PROBE_TIMEOUT` (500 ms). The bound here
    // is deliberately loose — this asserts "bounded", not "fast". A tight
    // bound would turn a loaded CI runner into a failure, which is the exact
    // mistake #723 documents and #816 had to undo.
    assert!(
        elapsed < Duration::from_secs(30),
        "launch took {elapsed:?}; it must give up on a non-binding daemon, \
         and the failure was: {error:?}"
    );
}
