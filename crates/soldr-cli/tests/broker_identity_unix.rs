//! Unix broker-endpoint resolution tests. This is a non-production test
//! target, so the whole file is cfg-gated to Unix hosts.

use soldr_cli::broker_identity::{
    authoritative_broker_executable, daemon_session_endpoint_from_executable,
    resolve_unix_for_executable, resolve_unix_for_home, BrokerEndpointFallback,
};

#[test]
fn linux_contract_mapping_is_exact() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let endpoint = resolve_unix_for_home(
        std::path::Path::new("/home/niteris"),
        std::path::Path::new("/run/user/1000"),
        Some(false),
        108,
    )
    .expect("endpoint");
    assert_eq!(
        endpoint.executable_path,
        std::path::PathBuf::from("/home/niteris/.soldr/broker/soldr-broker")
    );
    assert_eq!(
        endpoint.logical_socket_path,
        "/home/niteris/.soldr/broker/soldr-broker.sock"
    );
    assert_eq!(endpoint.bind_endpoint, endpoint.logical_socket_path);
    assert_eq!(endpoint.fallback, None);
}

#[test]
fn detached_broker_keeps_endpoint_beside_its_staged_executable() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let endpoint = resolve_unix_for_executable(
        std::path::Path::new("/mounted/home/.soldr/broker/soldr-broker"),
        std::path::Path::new("/run/user/1000"),
        Some(false),
        108,
    )
    .expect("endpoint");
    assert_eq!(
        endpoint.logical_socket_path,
        "/mounted/home/.soldr/broker/soldr-broker.sock"
    );
    assert_eq!(endpoint.bind_endpoint, endpoint.logical_socket_path);
}

#[test]
fn unix_daemon_session_mapping_is_executable_sibling() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let executable = temp.path().join("soldr-daemon");
    std::fs::write(&executable, b"daemon").expect("daemon image");
    let executable = std::fs::canonicalize(executable).expect("canonical daemon");
    let endpoint = daemon_session_endpoint_from_executable(&executable).expect("endpoint");
    assert_eq!(endpoint.namespace_id, executable.to_string_lossy());
    assert_eq!(
        endpoint.path,
        executable
            .with_file_name("soldr-daemon.session.sock")
            .to_string_lossy()
    );
    assert!(!endpoint.path.contains("sid"));
}

#[test]
fn unix_daemon_session_mapping_distinguishes_executable_leaves() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let first_executable = temp.path().join("soldr-daemon-a");
    let second_executable = temp.path().join("soldr-daemon-b");
    std::fs::write(&first_executable, b"a").expect("first image");
    std::fs::write(&second_executable, b"b").expect("second image");
    let first = daemon_session_endpoint_from_executable(&first_executable).expect("first endpoint");
    let second =
        daemon_session_endpoint_from_executable(&second_executable).expect("second endpoint");
    assert_ne!(first.path, second.path);
    assert!(first.path.ends_with("soldr-daemon-a.session.sock"));
    assert!(second.path.ends_with("soldr-daemon-b.session.sock"));
}

#[test]
fn canonical_existing_ancestor_collapses_symlinked_home_spelling() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let real = temp.path().join("real-home");
    let alias = temp.path().join("alias-home");
    std::fs::create_dir_all(&real).expect("real home");
    soldr_platform::fs::links::create(real.to_str().expect("UTF-8 real home"), &alias, true)
        .expect("home symlink");
    let resolved = authoritative_broker_executable(&alias, "soldr-broker");
    assert_eq!(
        resolved,
        std::fs::canonicalize(&real)
            .expect("canonical real home")
            .join(".soldr/broker/soldr-broker")
    );
}

#[test]
fn unix_daemon_session_overflow_uses_a_short_path_derived_name() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("long-route-segment".repeat(8));
    std::fs::create_dir_all(&directory).expect("long route directory");
    let executable = directory.join("soldr-daemon");
    std::fs::write(&executable, b"daemon").expect("daemon image");
    let executable = std::fs::canonicalize(executable).expect("canonical daemon");
    let first = daemon_session_endpoint_from_executable(&executable).expect("endpoint");
    let second = daemon_session_endpoint_from_executable(&executable).expect("endpoint");
    assert_eq!(first, second);
    assert!(soldr_platform::ipc::endpoint::socket_path_fits(
        std::path::Path::new(&first.path),
        soldr_platform::ipc::endpoint::sun_path_capacity()
    ));
    assert!(first.path.ends_with(".session.sock"));
    assert!(!first.path.contains("sid"));
}

#[test]
fn unix_fallback_order_is_overflow_then_filesystem() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let home = std::path::Path::new(
        "/very/long/home/profile/whose/logical/broker/socket/cannot/fit/in/sun_path",
    );
    let runtime = std::path::Path::new("/run/user/123");
    let overflow = resolve_unix_for_home(home, runtime, Some(true), 104).unwrap();
    assert_eq!(
        overflow.fallback,
        Some(BrokerEndpointFallback::UnixSunPathOverflow)
    );
    assert!(overflow
        .bind_endpoint
        .starts_with("/run/user/123/soldr/broker/soldr-broker-"));
    assert!(overflow.bind_endpoint.ends_with(".sock"));
    assert!(soldr_platform::ipc::endpoint::socket_path_fits(
        std::path::Path::new(&overflow.bind_endpoint),
        104
    ));

    let network = resolve_unix_for_home(home, runtime, Some(true), 4096).unwrap();
    assert_eq!(
        network.fallback,
        Some(BrokerEndpointFallback::UnixNonBindableFilesystem)
    );
    assert_eq!(overflow.logical_socket_path, network.logical_socket_path);
    assert_eq!(overflow.bind_endpoint, network.bind_endpoint);

    let other = resolve_unix_for_home(
        std::path::Path::new("/another/very/long/home/profile"),
        runtime,
        Some(true),
        104,
    )
    .unwrap();
    assert_ne!(overflow.bind_endpoint, other.bind_endpoint);
}

#[test]
fn unix_overflow_fallback_retries_under_tmp_when_runtime_dir_is_too_long() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let home = std::path::Path::new(
        "/very/long/home/profile/whose/logical/broker/socket/cannot/fit/in/sun_path",
    );
    let runtime =
        std::path::Path::new("/var/folders/vb/m6r1sg994rbgzbhtsm34bphh0000gn/T/soldr-501");
    let endpoint = resolve_unix_for_home(home, runtime, Some(false), 104).unwrap();
    assert_eq!(
        endpoint.fallback,
        Some(BrokerEndpointFallback::UnixSunPathOverflow)
    );
    assert!(endpoint.bind_endpoint.starts_with("/tmp/soldr-"));
    assert!(soldr_platform::ipc::endpoint::socket_path_fits(
        std::path::Path::new(&endpoint.bind_endpoint),
        104
    ));
    assert_eq!(
        endpoint.lease_database_path.parent(),
        std::path::Path::new(&endpoint.bind_endpoint).parent()
    );
}
