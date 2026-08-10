//! Unit tests for the v2 broker binary.
//!
//! Split out of `running-process-broker-v2.rs` when that file crossed the
//! repo's 1500-LOC ceiling. Kept as a `#[path]` module rather than moved to
//! `tests/` because these cover private helpers (`parse_cli`,
//! `build_hello_reply`, `resolve_socket_path`) that an integration test
//! cannot reach.
//!
//! The directory holds no `main.rs`, so cargo does not treat it as a second
//! binary target.

use super::*;
use running_process::broker::protocol_v2::ServiceDefinitionBuilder;
use tempfile::tempdir;

fn make_hello(service: &str, wanted: &str) -> Hello {
    Hello {
        client_min_protocol: ENVELOPE_VERSION as u32,
        client_max_protocol: ENVELOPE_VERSION as u32,
        service_name: service.to_string(),
        wanted_version: wanted.to_string(),
        client_version: "test".to_string(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "test".to_string(),
        connection_id: 42,
        peer_pid: 1234,
        client_lib_name: "test".to_string(),
        client_lib_version: "test".to_string(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 0,
    }
}

#[test]
fn accept_poll_exits_without_accepting_after_shutdown_request() {
    let shutdown = AtomicBool::new(true);
    let result = poll_accept_until_shutdown(&shutdown, || -> std::io::Result<()> {
        panic!("accept must not run after shutdown")
    })
    .unwrap();
    assert!(result.is_none());
}

/// Every console event that means "you are going away" must set the flag.
///
/// Missing one would leave the broker in the state this change exists to
/// fix: still polling a flag nothing sets, never draining, never
/// unbinding.
#[cfg(windows)]
#[test]
fn every_shutdown_console_event_requests_shutdown() {
    use winapi::um::wincon::{
        CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    for event in [
        CTRL_C_EVENT,
        CTRL_BREAK_EVENT,
        CTRL_CLOSE_EVENT,
        CTRL_LOGOFF_EVENT,
        CTRL_SHUTDOWN_EVENT,
    ] {
        CONSOLE_SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
        // SAFETY: calling the handler directly; it only stores an atomic.
        let handled = unsafe { console_ctrl_handler(event) };
        assert_eq!(
            handled,
            winapi::shared::minwindef::TRUE,
            "event {event} must be claimed, or Windows applies the default \
             terminate and the drain never runs"
        );
        assert!(
            CONSOLE_SHUTDOWN_REQUESTED.load(Ordering::Relaxed),
            "event {event} did not request shutdown"
        );
    }
    CONSOLE_SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
}

/// An event we do not recognize must be passed on, not swallowed.
/// Claiming it would suppress whatever handler comes next for an event
/// this broker has no opinion about.
#[cfg(windows)]
#[test]
fn an_unrecognized_console_event_is_declined() {
    CONSOLE_SHUTDOWN_REQUESTED.store(false, Ordering::Relaxed);
    // SAFETY: as above.
    let handled = unsafe { console_ctrl_handler(0xDEAD_BEEF) };
    assert_eq!(handled, winapi::shared::minwindef::FALSE);
    assert!(
        !CONSOLE_SHUTDOWN_REQUESTED.load(Ordering::Relaxed),
        "an unrelated event must not request shutdown"
    );
}

/// Registration has to actually succeed — the handler being correct is
/// worth nothing if Windows never calls it.
///
/// This is the boundary of what is verified here: that the handler is
/// installed and behaves correctly once invoked. That Windows delivers
/// Ctrl+C to a registered handler is OS-defined behavior and is not
/// re-tested, because generating a real console event would signal every
/// process sharing this console, including the test runner.
#[cfg(windows)]
#[test]
fn the_console_handler_installs() {
    install_shutdown_console_handler().expect("SetConsoleCtrlHandler should accept the handler");
    assert!(
        !CONSOLE_SHUTDOWN_REQUESTED.load(Ordering::Relaxed),
        "installing must start from a clear flag, or the loop would exit at once"
    );
}

#[test]
fn accept_poll_observes_shutdown_while_listener_would_block() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let setter = Arc::clone(&shutdown);
    let signaler = thread::spawn(move || {
        thread::sleep(ACCEPT_POLL_INTERVAL);
        setter.store(true, Ordering::Relaxed);
    });
    let start = Instant::now();
    let result = poll_accept_until_shutdown(&shutdown, || -> std::io::Result<()> {
        Err(std::io::ErrorKind::WouldBlock.into())
    })
    .unwrap();

    signaler.join().unwrap();
    assert!(result.is_none());
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn parse_cli_defaults() {
    let args = vec!["bin".to_owned()];
    let opts = parse_cli(&args).unwrap();
    assert!(!opts.no_bind);
    assert!(!opts.once);
    assert_eq!(opts.program, DEFAULT_PROGRAM);
}

#[test]
fn parse_cli_program_arg() {
    let args = vec![
        "bin".to_owned(),
        "--program".to_owned(),
        "zccache".to_owned(),
    ];
    let opts = parse_cli(&args).unwrap();
    assert_eq!(opts.program, "zccache");
}

#[test]
fn parse_cli_once_flag() {
    let args = vec!["bin".to_owned(), "--once".to_owned()];
    let opts = parse_cli(&args).unwrap();
    assert!(opts.once);
}

#[test]
fn parse_cli_program_missing_value_errs() {
    let args = vec!["bin".to_owned(), "--program".to_owned()];
    assert!(parse_cli(&args).is_err());
}

#[test]
fn parse_cli_unknown_arg_errs() {
    let args = vec!["bin".to_owned(), "--bogus".to_owned()];
    assert!(parse_cli(&args).is_err());
}

/// The HTTP surface is opt-in: starting the broker must not open a
/// listening TCP port on its own.
#[test]
fn the_http_surface_is_off_unless_asked_for() {
    let opts = parse_cli(&["bin".to_owned()]).unwrap();
    assert!(opts.http_port.is_none());
}

#[test]
fn http_port_accepts_dynamic_and_a_number() {
    assert_eq!(parse_http_port("dynamic").unwrap(), BrokerHttpPort::Dynamic);
    assert_eq!(parse_http_port("DYNAMIC").unwrap(), BrokerHttpPort::Dynamic);
    assert_eq!(
        parse_http_port("8080").unwrap(),
        BrokerHttpPort::StaticOrFallback { preferred: 8080 }
    );
}

/// Port 0 already means "OS-allocated" at the sockets layer. Reporting it
/// as `StaticOrFallback { preferred: 0 }` would describe a fallback that
/// can never trigger.
#[test]
fn http_port_zero_is_dynamic() {
    assert_eq!(parse_http_port("0").unwrap(), BrokerHttpPort::Dynamic);
}

/// A typo must not be silently reinterpreted as a default — that would
/// bind a port the operator did not ask for.
#[test]
fn http_port_rejects_a_non_port() {
    for bad in ["", "http", "-1", "65536", "80x"] {
        assert!(
            parse_http_port(bad).is_err(),
            "{bad:?} should not parse as a port"
        );
    }
}

#[test]
fn http_port_requires_a_value() {
    let args = vec!["bin".to_owned(), "--http-port".to_owned()];
    assert!(parse_cli(&args).is_err());
}

#[test]
fn parse_cli_threads_the_http_port_through() {
    let args = vec![
        "bin".to_owned(),
        "--http-port".to_owned(),
        "dynamic".to_owned(),
    ];
    let opts = parse_cli(&args).unwrap();
    assert_eq!(opts.http_port, Some(BrokerHttpPort::Dynamic));
}

/// The wiring, end to end: `start_http_surface` must bind, publish where
/// it actually landed, and leave something serving there.
///
/// This drives `start_http_surface` itself rather than re-doing its steps.
/// A test that bound and published on its own would keep passing if the
/// function stopped doing either — which is the failure this exists to
/// catch.
///
/// Uses a temp directory so a broker running on the developer's machine
/// cannot make it pass or fail by accident.
/// The page has to reflect backends the broker actually negotiated.
///
/// Before this, every piece worked in isolation and the chain was not
/// joined: `render_aggregator_page` was complete, but nothing outside
/// tests ever wrote to the registry, so a running broker served a page
/// that permanently read "no backends registered yet".
///
/// This drives the surface the binary actually builds and asserts on the
/// rendered HTML, rather than on the registry it just wrote to — which
/// would pass even if the server were handed a different registry.
#[test]
fn the_page_lists_a_backend_once_it_is_tracked() {
    use std::io::{Read as _, Write as _};

    let dir = tempdir().unwrap();
    let started = start_http_surface(BrokerHttpPort::Dynamic, "track-program", dir.path()).unwrap();

    // What the accept path does on a negotiated Hello.
    started.registry.track("zccache".to_string());

    let (ip, port) = broker_http_discovery::read_http_port(dir.path(), "track-program")
        .unwrap()
        .expect("endpoint published");
    let mut stream = std::net::TcpStream::connect(std::net::SocketAddr::new(ip, port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut body = Vec::new();
    stream.read_to_end(&mut body).unwrap();
    let page = String::from_utf8_lossy(&body);

    assert!(
        page.contains("zccache"),
        "backend missing from page: {page}"
    );
    assert!(
        !page.contains("no backends registered yet"),
        "page still claims nothing is registered: {page}"
    );
}

#[test]
fn starting_the_surface_publishes_an_endpoint_that_answers() {
    use std::io::{Read as _, Write as _};

    let dir = tempdir().unwrap();
    let started = start_http_surface(BrokerHttpPort::Dynamic, "test-program", dir.path()).unwrap();
    assert_eq!(started.program, "test-program");

    let (ip, port) = broker_http_discovery::read_http_port(dir.path(), "test-program")
        .unwrap()
        .expect("the surface publishes its endpoint");
    assert_ne!(port, 0, "a published port of 0 is not reachable");

    // Connect to the address as published. Anything else would test a
    // port we learned some other way, and would not prove the published
    // one is the live one.
    let mut stream = std::net::TcpStream::connect(std::net::SocketAddr::new(ip, port))
        .expect("the published endpoint accepts connections");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/"),
        "expected an HTTP response, got {text:?}"
    );
}

/// A stale file sends every reader to a port nobody is listening on,
/// which is worse than the absent file readers already handle.
#[test]
fn unpublishing_leaves_no_endpoint_behind() {
    let dir = tempdir().unwrap();
    broker_http_discovery::publish_http_port(
        dir.path(),
        "test-program",
        std::net::IpAddr::from([127, 0, 0, 1]),
        1234,
    )
    .unwrap();
    broker_http_discovery::unpublish_http_port(dir.path(), "test-program").unwrap();
    assert_eq!(
        broker_http_discovery::read_http_port(dir.path(), "test-program").unwrap(),
        None
    );
}

/// The pipe the broker hands back must be the one the daemon published.
///
/// Drives `resolve_backend_pipe` — the function the Hello path calls —
/// rather than reading the file directly, so this fails if resolution
/// stops consulting the published identity at all.
#[test]
fn a_published_daemon_identity_becomes_the_backend_pipe() {
    use running_process::broker::backend_lifecycle::identity::DaemonProcess;
    use running_process::broker::backend_sdk::{
        remove_daemon_identity_file, write_daemon_identity_file,
    };
    use running_process::broker::lifecycle::names_v2::daemon_identity_path;
    use running_process::broker::protocol::Endpoint;
    use running_process::broker::secure_dir::ensure_private_dir;

    let service = format!("resolve-test-{}", std::process::id());
    assert_eq!(
        resolve_backend_pipe(&service),
        "",
        "nothing published yet, so there is no pipe to report"
    );

    let path = daemon_identity_path(&service);
    ensure_private_dir(path.parent().expect("parent")).expect("private dir");
    let endpoint = Endpoint {
        namespace_id: String::new(),
        path: "daemon-endpoint-under-test".to_string(),
    };
    let daemon = DaemonProcess::current_process(endpoint, None).expect("identity");
    write_daemon_identity_file(&path, &daemon).expect("publish");

    assert_eq!(resolve_backend_pipe(&service), "daemon-endpoint-under-test");

    remove_daemon_identity_file(&path);
    assert_eq!(
        resolve_backend_pipe(&service),
        "",
        "a retracted daemon must stop being advertised"
    );
}

#[test]
fn build_hello_reply_refuses_unknown_service() {
    let dir = tempdir().unwrap();
    let loader = ServiceDefinitionLoader::new(dir.path());
    let hello = make_hello("nosuch", "1.0.0");
    let reply = build_hello_reply(&hello, &loader, "");
    match reply.result {
        Some(hello_reply::Result::Refused(r)) => {
            assert_eq!(r.code, ErrorCode::ErrorServiceUnknown as i32);
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn build_hello_reply_negotiates_registered_service() {
    let dir = tempdir().unwrap();
    ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache-daemon")
        .install_in(dir.path())
        .unwrap();
    let loader = ServiceDefinitionLoader::new(dir.path());
    let hello = make_hello("zccache", "1.0.0");
    let reply = build_hello_reply(&hello, &loader, "");
    match reply.result {
        Some(hello_reply::Result::Negotiated(n)) => {
            assert_eq!(n.connection_id, 42);
            // Empty here because this test passes no resolved pipe.
            // Resolution is the caller's job and is covered separately.
            assert!(n.backend_pipe.is_empty());
        }
        other => panic!("expected Negotiated, got {other:?}"),
    }
}

#[test]
fn build_hello_reply_blocks_below_min_version() {
    let dir = tempdir().unwrap();
    ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache-daemon")
        .min_version("2.0.0")
        .install_in(dir.path())
        .unwrap();
    let loader = ServiceDefinitionLoader::new(dir.path());
    let hello = make_hello("zccache", "1.0.0");
    let reply = build_hello_reply(&hello, &loader, "");
    match reply.result {
        Some(hello_reply::Result::Refused(r)) => {
            assert_eq!(r.code, ErrorCode::ErrorVersionBlocked as i32);
            assert!(r.reason.contains("min_version"), "got: {}", r.reason);
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn build_hello_reply_blocks_outside_version_allow_list() {
    let dir = tempdir().unwrap();
    ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache-daemon")
        .version_allow_list(["1.0.0", "1.1.0"])
        .install_in(dir.path())
        .unwrap();
    let loader = ServiceDefinitionLoader::new(dir.path());
    let hello = make_hello("zccache", "1.2.0");
    let reply = build_hello_reply(&hello, &loader, "");
    match reply.result {
        Some(hello_reply::Result::Refused(r)) => {
            assert_eq!(r.code, ErrorCode::ErrorVersionBlocked as i32);
            assert!(r.reason.contains("allow_list"), "got: {}", r.reason);
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn is_already_bound_error_classifies_addr_in_use() {
    let err = std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use");
    assert!(is_already_bound_error(&err));
}

#[test]
fn is_already_bound_error_classifies_would_block() {
    let err = std::io::Error::new(std::io::ErrorKind::WouldBlock, "would block");
    assert!(is_already_bound_error(&err));
}

#[test]
fn is_already_bound_error_classifies_permission_denied() {
    // PR #536 deliberately added `PermissionDenied` to the
    // is_already_bound_error matcher: on Windows, a double-bind
    // surfaces as `ERROR_ACCESS_DENIED` (raw os error 5) because
    // the existing pipe instance's ACL blocks the second bind —
    // not as `AddrInUse`. This test was added in PR #534 before
    // that classification was widened, expecting the negation;
    // PR #536 updated the impl but forgot the test, which then
    // cascade-failed every CI run until this fix. Rename +
    // invert to match the now-current contract.
    let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
    assert!(is_already_bound_error(&err));
}

#[test]
fn is_already_bound_error_does_not_misclassify_not_found() {
    let err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
    assert!(!is_already_bound_error(&err));
}

#[test]
fn build_hello_reply_allows_version_in_allow_list() {
    let dir = tempdir().unwrap();
    ServiceDefinitionBuilder::shared_broker("zccache", "/usr/bin/zccache-daemon")
        .version_allow_list(["1.0.0", "1.1.0"])
        .install_in(dir.path())
        .unwrap();
    let loader = ServiceDefinitionLoader::new(dir.path());
    let hello = make_hello("zccache", "1.1.0");
    let reply = build_hello_reply(&hello, &loader, "");
    assert!(matches!(
        reply.result,
        Some(hello_reply::Result::Negotiated(_))
    ));
}
