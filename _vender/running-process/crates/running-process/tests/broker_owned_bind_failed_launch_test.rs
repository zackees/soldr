//! A failed launch must not orphan the socket the broker bound (#500).
//!
//! Under broker-owned bind the broker creates the socket file before the
//! daemon exists. If the launch then fails, nothing is serving that endpoint —
//! the child was killed — so the file has to go with it. Leaving it behind
//! accumulates dead sockets in the runtime directory, one per failed launch,
//! and a later operator finds paths that look like live endpoints and are not.
//!
//! This lives in its own test binary because it sets the opt-in environment
//! variable. Tests inside one binary share a process, so an env-mutating test
//! races its siblings; a file with exactly one test cannot.
//!
//! Unix-only: `broker_owned_bind` is, so there is no broker-created socket to
//! orphan anywhere else.

#![cfg(unix)]

use std::path::PathBuf;

use running_process::broker::broker_owned_bind::LAUNCHER_OPT_IN_ENV;
use running_process::broker::protocol::ServiceDefinition;
use running_process::broker::server::backend_endpoint_allocator::BackendEndpointAllocator;
use running_process::broker::server::backend_launcher::{
    BackendLaunchRequest, BackendLauncher, CommandBackendLauncher,
};
use running_process::broker::server::{BackendKey, BrokerInstanceKey, TraceContext};

/// Locate a fixture binary built before the suite runs (see #747 for why this
/// does not build on demand).
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
fn a_failed_launch_leaves_no_socket_behind() {
    let sleeper = testbin_path("testbin-sleeper");

    // Safe here: this binary contains exactly one test, so nothing else is
    // reading the environment concurrently.
    std::env::set_var(LAUNCHER_OPT_IN_ENV, "1");

    let service_definition = ServiceDefinition {
        service_name: "never-binds".into(),
        binary_path: sleeper.display().to_string(),
        isolation: 1,
        explicit_instance: String::new(),
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

    let launcher = CommandBackendLauncher::new("0123456789abcdef");
    let dir = endpoint_dir();
    std::fs::create_dir_all(&dir).ok();
    let sockets_before = broker_sockets(&dir);

    let result = launcher.launch(&request);
    assert!(
        result.is_err(),
        "the fixture never binds, so the launch cannot succeed"
    );

    let sockets_after = broker_sockets(&dir);
    let leaked: Vec<_> = sockets_after
        .iter()
        .filter(|path| !sockets_before.contains(*path))
        .collect();
    assert!(
        leaked.is_empty(),
        "a failed launch orphaned {} socket(s): {leaked:?}",
        leaked.len()
    );
}

/// Every path currently sitting in the broker's endpoint directory.
///
/// Compared before and after rather than asserting the directory is empty: a
/// developer machine may legitimately have live brokers running, and a test
/// that fails because of someone else's daemon is worse than no test.
fn broker_sockets(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect()
}

/// The directory the allocator actually places endpoints in.
///
/// Obtained by allocating one and reading its parent, rather than
/// reconstructing `$XDG_RUNTIME_DIR/running-process/broker` here. An earlier
/// revision of this test guessed the path, looked in the wrong directory, saw
/// nothing either side of the launch, and passed while detecting nothing —
/// including when the leak it exists to catch was reintroduced on purpose.
fn endpoint_dir() -> PathBuf {
    let mut allocator = BackendEndpointAllocator::new("0123456789abcdef", "shared");
    let sample = allocator.allocate().expect("allocate a sample endpoint");
    PathBuf::from(&sample.path)
        .parent()
        .expect("an endpoint path has a parent directory")
        .to_path_buf()
}
