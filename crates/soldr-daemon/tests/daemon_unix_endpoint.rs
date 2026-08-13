#![cfg(unix)]
//! Unix-only daemon endpoint tests: control-endpoint claim exclusivity
//! and the retirement fence (a stale daemon must never unlink a
//! successor's live socket). Moved out of `daemon/server.rs` when that
//! file became host-neutral (#2493); the claim and identity primitives
//! now live in the platform ipc listener leaf.

use soldr_daemon::timed_test;

timed_test!(claimed_unix_endpoint_cannot_be_bound_by_a_second_daemon, {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let temp = tempfile::tempdir().unwrap();
            let socket = temp.path().join("soldr-daemon.control.sock");
            let (listener, _) = soldr_platform::ipc::listener::claim_control_endpoint_at(&socket)
                .expect("claim control endpoint");
            let second = tokio::net::UnixListener::bind(&socket);
            assert!(second.is_err(), "the endpoint claim must be exclusive");
            drop(listener);
        });
});

timed_test!(retiring_daemon_does_not_unlink_replacement_socket, {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("soldr.sock");

    let old_listener =
        std::os::unix::net::UnixListener::bind(&socket_path).expect("bind old socket");
    let old_identity =
        soldr_platform::ipc::listener::unix_socket_identity(&socket_path).expect("old identity");

    std::fs::remove_file(&socket_path).expect("unlink old socket name");
    let replacement_listener =
        std::os::unix::net::UnixListener::bind(&socket_path).expect("bind replacement");
    let replacement_identity = soldr_platform::ipc::listener::unix_socket_identity(&socket_path)
        .expect("replacement identity");
    assert_ne!(old_identity, replacement_identity);

    assert!(
        !soldr_platform::ipc::listener::remove_unix_socket_if_matches(&socket_path, old_identity)
            .expect("fenced old cleanup"),
        "old daemon must not remove the replacement socket"
    );
    assert!(socket_path.exists());
    assert!(
        soldr_platform::ipc::listener::remove_unix_socket_if_matches(
            &socket_path,
            replacement_identity
        )
        .expect("replacement cleanup")
    );

    drop(replacement_listener);
    drop(old_listener);
});
