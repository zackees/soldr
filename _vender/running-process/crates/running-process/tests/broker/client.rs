#![cfg(feature = "client")]

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use interprocess::local_socket::prelude::*;
use running_process::broker::client::{
    broker_disabled_by_env, connect_to_backend, BackendConnectionRoute, ConnectBackendRequest,
    RefusalKind, RUNNING_PROCESS_DISABLE_ENV,
};
use running_process::broker::protocol::ErrorCode;
// #433 R4: the fake-backend seam compiles only under `test-seams`. Its tests
// (and the symbols they alone use) are gated to the same feature.
#[cfg(feature = "test-seams")]
use running_process::broker::client::{BrokerClientError, RUNNING_PROCESS_FAKE_BACKEND_ENV};
use running_process::broker::protocol::{BrokerIsolation, ServiceDefinition};
use running_process::broker::server::{
    handle_hello_connection, HelloHandler, PeerIdentity, RegisteredBackend,
};

use crate::socket_common::{
    await_test_socket_ready, bind_ready_test_socket, cleanup_test_socket, unique_socket_name,
};

static DISABLE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn service_definition() -> ServiceDefinition {
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: "/usr/local/bin/zccache".into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: "/opt/zccache/versions".into(),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn handler(backend_endpoint: &str) -> HelloHandler {
    HelloHandler::new()
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: backend_endpoint.into(),
            server_capabilities: 0x01,
        })
        .unwrap()
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn remove(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, original }
    }

    fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn broker_disabled_by_env_is_false_when_unset() {
    let _lock = DISABLE_ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::remove(RUNNING_PROCESS_DISABLE_ENV);

    assert!(!broker_disabled_by_env().unwrap());
}

#[test]
fn broker_disabled_by_env_is_true_for_canonical_value() {
    let _lock = DISABLE_ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::set(RUNNING_PROCESS_DISABLE_ENV, "1");

    assert!(broker_disabled_by_env().unwrap());
}

#[test]
fn broker_disabled_by_env_rejects_unknown_values() {
    let _lock = DISABLE_ENV_LOCK.lock().unwrap();
    let _guard = EnvVarGuard::set(RUNNING_PROCESS_DISABLE_ENV, "true");

    let err = broker_disabled_by_env().unwrap_err();

    assert_eq!(err.value, "true");
}

#[test]
fn connect_to_backend_uses_cached_endpoint_when_versions_match() {
    let _watchdog = hang_watchdog("connect_to_backend_uses_cached_endpoint_when_versions_match");
    let cached_backend = unique_socket_name("cached-backend");
    let backend = spawn_accept_once(cached_backend.clone());

    let mut request = ConnectBackendRequest::new("missing-broker", "zccache", "1.11.20", "1.11.20");
    request.cached_backend_endpoint = Some(&cached_backend);
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, cached_backend);
    assert_eq!(connection.route, BackendConnectionRoute::HelloSkip);
    assert!(connection.negotiated.is_none());
    // Join before dropping — see `spawn_accept_once`.
    backend.join().unwrap().unwrap();
    drop(connection.stream);
}

#[test]
fn connect_to_backend_falls_back_to_broker_when_cache_missing() {
    let _watchdog = hang_watchdog("connect_to_backend_falls_back_to_broker_when_cache_missing");
    let broker_endpoint = unique_socket_name("broker");
    let backend_endpoint = unique_socket_name("backend");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());

    let request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "1.11.20", "1.11.20");
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, backend_endpoint);
    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(
        connection.negotiated.as_ref().unwrap().daemon_version,
        "1.11.20"
    );
    // Join before dropping — see `spawn_accept_once`.
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
    drop(connection.stream);
}

#[test]
fn connect_to_backend_falls_back_to_broker_when_cached_endpoint_is_stale() {
    let _watchdog =
        hang_watchdog("connect_to_backend_falls_back_to_broker_when_cached_endpoint_is_stale");
    let broker_endpoint = unique_socket_name("broker-stale-cache");
    let backend_endpoint = unique_socket_name("backend-stale-cache");
    let stale_cached_endpoint = unique_socket_name("stale-cached-backend");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());

    let mut request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "1.11.20", "1.11.20");
    request.cached_backend_endpoint = Some(&stale_cached_endpoint);
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, backend_endpoint);
    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(
        connection.negotiated.as_ref().unwrap().daemon_version,
        "1.11.20"
    );
    // Join before dropping — see `spawn_accept_once`.
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
    drop(connection.stream);
}

#[test]
fn connect_to_backend_does_not_skip_when_versions_differ() {
    let _watchdog = hang_watchdog("connect_to_backend_does_not_skip_when_versions_differ");
    let broker_endpoint = unique_socket_name("broker-mismatch");
    let backend_endpoint = unique_socket_name("backend-mismatch");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());

    let mut request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "1.11.20", "1.11.19");
    request.cached_backend_endpoint = Some("missing-cached-backend");
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    assert_eq!(connection.endpoint, backend_endpoint);
    // Join before dropping — see `spawn_accept_once`.
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
    drop(connection.stream);
}

#[cfg(feature = "test-seams")]
#[test]
fn connect_to_backend_uses_fake_backend_seam_when_set() {
    let _watchdog = hang_watchdog("connect_to_backend_uses_fake_backend_seam_when_set");
    let _lock = DISABLE_ENV_LOCK.lock().unwrap();
    let fake_backend = unique_socket_name("fake-backend");
    let backend = spawn_accept_once(fake_backend.clone());
    let _disable_guard = EnvVarGuard::remove(RUNNING_PROCESS_DISABLE_ENV);
    let _fake_guard = EnvVarGuard::set(RUNNING_PROCESS_FAKE_BACKEND_ENV, &fake_backend);

    // A cached endpoint that does not exist proves the seam takes
    // precedence over both the Hello-skip cache and the broker path.
    let mut request = ConnectBackendRequest::new("missing-broker", "zccache", "1.11.20", "1.11.20");
    request.cached_backend_endpoint = Some("missing-cached-backend");
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, fake_backend);
    assert_eq!(connection.route, BackendConnectionRoute::HelloSkip);
    assert!(connection.negotiated.is_none());
    // Join before dropping — see `spawn_accept_once`.
    backend.join().unwrap().unwrap();
    drop(connection.stream);
}

#[cfg(feature = "test-seams")]
#[test]
fn connect_to_backend_ignores_fake_backend_seam_when_broker_disabled() {
    let _watchdog =
        hang_watchdog("connect_to_backend_ignores_fake_backend_seam_when_broker_disabled");
    let _lock = DISABLE_ENV_LOCK.lock().unwrap();
    let broker_endpoint = unique_socket_name("broker-fake-disabled");
    let backend_endpoint = unique_socket_name("backend-fake-disabled");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());
    let _disable_guard = EnvVarGuard::set(RUNNING_PROCESS_DISABLE_ENV, "1");
    let _fake_guard = EnvVarGuard::set(RUNNING_PROCESS_FAKE_BACKEND_ENV, "missing-fake-backend");

    let request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "1.11.20", "1.11.20");
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, backend_endpoint);
    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    // Join before dropping — see `spawn_accept_once`.
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
    drop(connection.stream);
}

#[cfg(feature = "test-seams")]
#[test]
fn connect_to_backend_fake_backend_connect_error_does_not_fall_back() {
    let _lock = DISABLE_ENV_LOCK.lock().unwrap();
    let _disable_guard = EnvVarGuard::remove(RUNNING_PROCESS_DISABLE_ENV);
    let _fake_guard = EnvVarGuard::set(RUNNING_PROCESS_FAKE_BACKEND_ENV, "missing-fake-backend");

    let request = ConnectBackendRequest::new("missing-broker", "zccache", "1.11.20", "1.11.20");
    let error = connect_to_backend(request).unwrap_err();

    // BackendConnect (not BrokerConnect) proves the client tried the fake
    // endpoint and returned its error instead of falling back to the broker.
    assert!(
        matches!(error, BrokerClientError::BackendConnect(_)),
        "expected BackendConnect, got {error:?}"
    );
}

#[cfg(feature = "test-seams")]
#[test]
fn connect_to_backend_ignores_empty_fake_backend_seam() {
    let _watchdog = hang_watchdog("connect_to_backend_ignores_empty_fake_backend_seam");
    let _lock = DISABLE_ENV_LOCK.lock().unwrap();
    let broker_endpoint = unique_socket_name("broker-fake-empty");
    let backend_endpoint = unique_socket_name("backend-fake-empty");
    let backend = spawn_accept_once(backend_endpoint.clone());
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());
    let _disable_guard = EnvVarGuard::remove(RUNNING_PROCESS_DISABLE_ENV);
    let _fake_guard = EnvVarGuard::set(RUNNING_PROCESS_FAKE_BACKEND_ENV, "");

    let request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "1.11.20", "1.11.20");
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, backend_endpoint);
    assert_eq!(connection.route, BackendConnectionRoute::BrokerNegotiated);
    // Join before dropping — see `spawn_accept_once`.
    broker.join().unwrap().unwrap();
    backend.join().unwrap().unwrap();
    drop(connection.stream);
}

#[cfg(feature = "test-seams")]
#[test]
fn connect_to_backend_ignores_unset_fake_backend_seam() {
    let _watchdog = hang_watchdog("connect_to_backend_ignores_unset_fake_backend_seam");
    let _lock = DISABLE_ENV_LOCK.lock().unwrap();
    let cached_backend = unique_socket_name("cached-backend-no-fake");
    let backend = spawn_accept_once(cached_backend.clone());
    let _disable_guard = EnvVarGuard::remove(RUNNING_PROCESS_DISABLE_ENV);
    let _fake_guard = EnvVarGuard::remove(RUNNING_PROCESS_FAKE_BACKEND_ENV);

    let mut request = ConnectBackendRequest::new("missing-broker", "zccache", "1.11.20", "1.11.20");
    request.cached_backend_endpoint = Some(&cached_backend);
    let connection = connect_to_backend(request).unwrap();

    assert_eq!(connection.endpoint, cached_backend);
    assert_eq!(connection.route, BackendConnectionRoute::HelloSkip);
    // Join before dropping — see `spawn_accept_once`.
    backend.join().unwrap().unwrap();
    drop(connection.stream);
}

#[test]
fn refusal_kind_maps_wire_codes() {
    assert_eq!(
        RefusalKind::from_code(ErrorCode::ErrorVersionUnsupported),
        RefusalKind::VersionUnsupported
    );
    assert_eq!(
        RefusalKind::from_code(ErrorCode::ErrorVersionBlocked),
        RefusalKind::VersionBlocked
    );
    assert_eq!(
        RefusalKind::from_code(ErrorCode::ErrorServiceUnknown),
        RefusalKind::ServiceUnknown
    );
    assert_eq!(
        RefusalKind::from_code(ErrorCode::ErrorRateLimited),
        RefusalKind::RateLimited
    );
    assert_eq!(
        RefusalKind::from_code(ErrorCode::ErrorShuttingDown),
        RefusalKind::ShuttingDown
    );
    // Codes without a dedicated kind fall into Other, preserving the raw code.
    assert_eq!(
        RefusalKind::from_code(ErrorCode::ErrorInternal),
        RefusalKind::Other(ErrorCode::ErrorInternal)
    );
}

#[test]
fn connect_to_backend_classifies_unknown_service_refusal() {
    let _watchdog = hang_watchdog("connect_to_backend_classifies_unknown_service_refusal");
    let broker_endpoint = unique_socket_name("broker-refuse-unknown");
    let backend_endpoint = unique_socket_name("backend-refuse-unknown");
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());

    // The broker only registered "zccache"; an unknown service is refused.
    let request = ConnectBackendRequest::new(&broker_endpoint, "not-registered", "1.0.0", "1.0.0");
    let error = connect_to_backend(request).unwrap_err();

    assert_eq!(error.refusal_kind(), Some(RefusalKind::ServiceUnknown));
    broker.join().unwrap().unwrap();
}

#[test]
fn connect_to_backend_classifies_blocked_version_refusal() {
    let _watchdog = hang_watchdog("connect_to_backend_classifies_blocked_version_refusal");
    let broker_endpoint = unique_socket_name("broker-refuse-version");
    let backend_endpoint = unique_socket_name("backend-refuse-version");
    let broker = spawn_broker_once(broker_endpoint.clone(), backend_endpoint.clone());

    // "9.9.9" is not in the registered version_allow_list → blocked.
    let request = ConnectBackendRequest::new(&broker_endpoint, "zccache", "9.9.9", "9.9.9");
    let error = connect_to_backend(request).unwrap_err();

    assert_eq!(error.refusal_kind(), Some(RefusalKind::VersionBlocked));
    broker.join().unwrap().unwrap();
}

/// Turn a hang in these tests into a stack dump instead of a bare timeout.
///
/// Every test below hands a socket to a helper thread and then `join()`s it.
/// If the connection the helper is waiting to `accept()` never arrives, that
/// join blocks forever: there is no deadline on it, and `JoinHandle` has no
/// timed form. What the runner reports is "timed out after 120s" with no
/// indication of which side was stuck or why.
///
/// This has actually happened — see running-process#747, where
/// `connect_to_backend_uses_cached_endpoint_when_versions_match` hit
/// nextest's 120s kill during a full-suite run and left nothing to diagnose.
/// The watchdog fires first and writes an all-thread dump, so the next
/// occurrence names the blocked call instead of only its duration.
///
/// 30s is far above these tests' normal runtime (milliseconds) and well
/// under the 120s harness kill, so it never pre-empts a slow-but-working
/// run and always beats the runner to the evidence.
fn hang_watchdog(test_name: &'static str) -> test_watchdog::WatchdogGuard {
    test_watchdog::install(
        Duration::from_secs(30),
        format!("{test_name} appears to be hung waiting on a test socket"),
        None,
    )
}

/// Accept exactly one connection on `socket_name`.
///
/// # The caller must join this before dropping its end
///
/// The ready signal below fires once the listener is *bound*, not once it is
/// inside `accept()`. So a caller that connects and then disconnects can do
/// both before this thread ever calls `accept`.
///
/// That is fatal on Windows. `interprocess` documents it on
/// `PipeListener::accept`: a client that connects and disconnects with no
/// `accept` in between leaves a dead-on-arrival connection on the instance,
/// and `ConnectNamedPipe` reports `ERROR_NO_DATA`. The listener clears it and
/// blocks again — waiting for a second client that is never coming, because
/// the only one already came and went. The test then hangs in `join()`
/// forever.
///
/// Joining first keeps the connection alive across the `accept`, which
/// returns immediately via `ERROR_PIPE_CONNECTED`. See running-process#751.
fn spawn_accept_once(socket_name: String) -> thread::JoinHandle<io::Result<()>> {
    let display_name = socket_name.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&socket_name, &ready_tx)?;
        let _stream = listener.accept()?;
        cleanup_test_socket(&socket_name);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}

fn spawn_broker_once(
    broker_endpoint: String,
    backend_endpoint: String,
) -> thread::JoinHandle<io::Result<()>> {
    let display_name = broker_endpoint.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = bind_ready_test_socket(&broker_endpoint, &ready_tx)?;
        let mut stream = listener.accept()?;
        let peer = PeerIdentity {
            pid: std::process::id(),
            uid_or_sid: "test-peer".into(),
        };
        handle_hello_connection(&mut stream, &handler(&backend_endpoint), peer)
            .map_err(|err| io::Error::other(err.to_string()))?;
        cleanup_test_socket(&broker_endpoint);
        Ok(())
    });
    await_test_socket_ready(&ready_rx, &display_name);
    handle
}
