#![cfg(unix)]
//! Unix-only SESSION endpoint bind tests: missing-socket-parent creation
//! and (macOS) bind-time permission tightening. Moved out of
//! `daemon/session_endpoint/tests.rs` when that module became
//! host-neutral (#2493); the bind mechanics live in the platform ipc
//! listener leaf.

use soldr_daemon::daemon::session_endpoint::bind_session_listener;
use soldr_daemon::timed_test;

timed_test!(session_listener_creates_missing_socket_parent, {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(async {
            let temp = tempfile::tempdir().expect("tempdir");
            let parent = temp.path().join("missing").join("runtime");
            let socket = parent.join("daemon.session");
            assert!(!parent.exists(), "test requires a missing socket parent");

            let listener = bind_session_listener(&socket.display().to_string())
                .expect("bind creates the missing socket parent");
            assert!(
                parent.is_dir(),
                "SESSION bind must create its socket parent"
            );
            drop(listener);
        });
});

#[cfg(target_os = "macos")]
timed_test!(macos_session_listener_restricts_socket_after_bind, {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("soldr-daemon.session.sock");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let _context = runtime.enter();
    let listener = bind_session_listener(&path.display().to_string()).expect("bind listener");
    let mode = std::fs::metadata(&path)
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    drop(listener);
});
