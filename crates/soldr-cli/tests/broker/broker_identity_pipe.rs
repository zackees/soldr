//! Portable Windows pipe-name derivation tests (issue #2476 / #2493).
//!
//! The sanitizer is pure string logic in the platform crate's neutral
//! facade, so the complete Windows matrix runs under the Linux dev
//! harness too — this integration target is deliberately NOT cfg-gated.
//!
//! Test declaration note: this is a non-production test target, so bare
//! `#[test]` is used with the same bare-`#\[test\]` convention as the
//! workspace lint expects.

use soldr_cli::broker_identity::{
    broker_windows_error, identity_key, windows_broker_pipe_from_executable, BrokerIdentityError,
};

#[test]
fn windows_contract_mapping_is_exact() {
    let endpoint =
        windows_broker_pipe_from_executable(r"C:\Users\niteris\.soldr\broker\soldr-broker.exe")
            .expect("endpoint");
    assert_eq!(
        endpoint.logical_socket_path,
        r"c:\users\niteris\.soldr\broker\soldr-broker.sock"
    );
    assert_eq!(
        endpoint.pipe_leaf,
        r"c%3A%5Cusers%5Cniteris%5C.soldr%5Cbroker%5Csoldr-broker.sock"
    );
    assert!(!endpoint.overflowed);
}

#[test]
fn windows_sanitizer_normalizes_supported_spellings() {
    let expected =
        windows_broker_pipe_from_executable(r"C:\Users\Me\soldr-broker.exe").expect("baseline");
    for spelling in [
        r"c:/users/me/soldr-broker.EXE",
        r"\\?\C:\Users\.\Me\soldr-broker.ExE",
        r"C:\\Users\Me\.\soldr-broker.exe",
    ] {
        assert_eq!(
            windows_broker_pipe_from_executable(spelling).expect(spelling),
            expected,
            "{spelling}"
        );
    }
}

#[test]
fn windows_sanitizer_normalizes_extended_unc() {
    let ordinary =
        windows_broker_pipe_from_executable(r"\\server\profiles\Me\.soldr\broker\soldr-broker.exe")
            .expect("ordinary");
    let extended = windows_broker_pipe_from_executable(
        r"\\?\UNC\SERVER\profiles\me\.soldr\broker\soldr-broker.EXE",
    )
    .expect("extended");
    assert_eq!(ordinary, extended);
    assert!(ordinary
        .logical_socket_path
        .starts_with(r"\\server\profiles"));
}

#[test]
fn windows_sanitizer_encodes_space_percent_and_non_ascii_bytes() {
    let endpoint = windows_broker_pipe_from_executable("C:\\Users\\Jöhn 100%\\soldr-broker.exe")
        .expect("endpoint");
    assert!(endpoint.pipe_leaf.contains("%20"));
    assert!(endpoint.pipe_leaf.contains("%25"));
    assert!(endpoint.pipe_leaf.contains("%C3%B6"));
    assert_eq!(
        endpoint.logical_socket_path, "c:\\users\\jöhn 100%\\soldr-broker.sock",
        "non-ASCII case is preserved while ASCII case folds"
    );
}

#[test]
fn windows_sanitizer_rejects_relative_and_parent_paths() {
    assert!(matches!(
        windows_broker_pipe_from_executable(r"Users\me\soldr-broker.exe"),
        Err(BrokerIdentityError::RelativeWindowsExecutable(_))
    ));
    assert!(matches!(
        windows_broker_pipe_from_executable(r"C:\Users\me\..\other\soldr-broker.exe"),
        Err(BrokerIdentityError::WindowsParentComponent(_))
    ));
}

#[test]
fn windows_overflow_fallback_is_deterministic_and_diagnostic() {
    let path = format!(r"C:\Users\{}\soldr-broker.exe", "long-profile-".repeat(30));
    let first = windows_broker_pipe_from_executable(&path).expect("first");
    let second = windows_broker_pipe_from_executable(&path).expect("second");
    assert_eq!(first, second);
    assert!(first.overflowed);
    assert!(first.pipe_leaf.starts_with("soldr-broker-ovf-"));
    assert_eq!(first.pipe_leaf.len(), "soldr-broker-ovf-".len() + 16);
    assert!(first.oversized_leaf.as_ref().is_some_and(|leaf| {
        soldr_platform::ipc::endpoint::WINDOWS_PIPE_PREFIX.len() + leaf.len()
            > soldr_platform::ipc::endpoint::WINDOWS_PIPE_NAME_LIMIT
    }));
}

#[test]
fn distinct_canonical_windows_paths_have_distinct_regular_leaves() {
    let cases = [
        r"C:\Users\a\soldr-broker.exe",
        r"C:\Users\b\soldr-broker.exe",
        r"D:\Users\a\soldr-broker.exe",
        r"\\server\share\a\soldr-broker.exe",
        "C:\\Users\\Ä\\soldr-broker.exe",
        "C:\\Users\\ä\\soldr-broker.exe",
    ];
    let mut logical = std::collections::HashSet::new();
    let mut leaves = std::collections::HashSet::new();
    for case in cases {
        let endpoint = windows_broker_pipe_from_executable(case).expect(case);
        assert!(
            !endpoint.overflowed,
            "fixture should exercise injective encoding"
        );
        assert!(logical.insert(endpoint.logical_socket_path));
        assert!(leaves.insert(endpoint.pipe_leaf));
    }
}

#[test]
fn different_profiles_produce_different_endpoints_without_sid_suffixes() {
    let a = windows_broker_pipe_from_executable(r"C:\Users\alice\.soldr\broker\soldr-broker.exe")
        .unwrap();
    let b = windows_broker_pipe_from_executable(r"C:\Users\bob\.soldr\broker\soldr-broker.exe")
        .unwrap();
    assert_ne!(a.pipe_leaf, b.pipe_leaf);
    assert!(!a.pipe_leaf.contains("sid"));
    assert!(!b.pipe_leaf.contains("sid"));
}

#[test]
fn resurrection_leases_are_partitioned_by_broker_executable_path() {
    let a = format!(
        "broker-lease-{}.sqlite3",
        identity_key(b"/profiles/a/.soldr/broker/soldr-broker.sock")
    );
    let b = format!(
        "broker-lease-{}.sqlite3",
        identity_key(b"/profiles/b/.soldr/broker/soldr-broker.sock")
    );
    assert_ne!(a, b);
    assert!(a.starts_with("broker-lease-"));
    assert!(a.ends_with(".sqlite3"));
}

#[test]
fn endpoint_identity_contains_no_route_or_version_inputs() {
    let first =
        windows_broker_pipe_from_executable(r"C:\Users\same\.soldr\broker\soldr-broker.exe")
            .unwrap();
    let second =
        windows_broker_pipe_from_executable(r"C:\Users\same\.soldr\broker\soldr-broker.exe")
            .unwrap();
    assert_eq!(first, second);
    for forbidden in ["rpb-v2", "soldr-daemon", "0.9", "route", "session-1"] {
        assert!(!first.pipe_leaf.contains(forbidden));
    }
}

#[test]
fn error_mapper_keeps_the_broker_vocabulary() {
    assert!(matches!(
        broker_windows_error("windows executable path must be absolute: x".to_string()),
        BrokerIdentityError::RelativeWindowsExecutable(_)
    ));
    assert!(matches!(
        broker_windows_error("windows executable path contains unresolved '..': x".to_string()),
        BrokerIdentityError::WindowsParentComponent(_)
    ));
    assert!(matches!(
        broker_windows_error("windows executable path must end in .exe: x".to_string()),
        BrokerIdentityError::MissingWindowsExeExtension(_)
    ));
}
