use super::*;

#[test]
fn infra_record_names_are_safe_and_percent_encoded() {
    assert_eq!(
        infra_record_name("soldr-cli::guards a/b%c"),
        "soldr-cli::guards a%2Fb%25c"
    );
    assert_eq!(infra_record_name("é"), "%C3%A9");
    // ci-test's percent decoder (`test_pressure::percent_decode`) reassembles
    // the exact UTF-8 from a byte-by-byte escape.
    assert_eq!(infra_record_name("tests::ü%"), "tests::%C3%BC%25");
    assert_eq!(
        infra_record_name("soldr-cli::guards cli/ci::test name"),
        "soldr-cli::guards cli%2Fci::test name"
    );
    let long = "x".repeat(300);
    let name = infra_record_name(&long);
    assert_eq!(name.len(), 160 + 1 + 16);
    assert!(name.starts_with(&"x".repeat(160)));
    assert_eq!(name.as_bytes()[160], b'~');
}

#[test]
fn memory_exhaustion_signatures_are_recognised() {
    for text in [
        &b"thread 'x' panicked: Cannot allocate memory"[..],
        b"memory allocation of 1024 bytes failed",
        b"Os { code: 12, kind: OutOfMemory }",
        b"spawn failed (os error 12)",
        b"OSError: [Errno 12] Cannot allocate memory: '/repo/.github/scripts'\n",
        b"Traceback (most recent call last):\nMemoryError\n",
    ] {
        assert!(memory_signature(text).is_some(), "{text:?}");
    }
    for text in [
        &b"assertion failed: left == right\n"[..],
        b"thread 'main' panicked at src/lib.rs:1:1\n",
        b"",
    ] {
        assert_eq!(memory_signature(text), None, "{text:?}");
    }
}

#[test]
fn statuses_name_their_signal() {
    assert_eq!(describe_status(None), "not started");
    assert_eq!(describe_status(Some(3)), "3");
    // SIGKILL is numbered alike on every POSIX host; SIGBUS (7) is not.
    assert_eq!(describe_status(Some(-9)), "killed by SIGKILL");
    if is_linux() {
        assert_eq!(describe_status(Some(-7)), "killed by SIGBUS");
    } else {
        assert_eq!(describe_status(Some(-7)), "killed by signal 7");
    }
    assert_eq!(describe_status(Some(-77)), "killed by signal 77");
}

#[test]
fn identity_falls_back_to_the_program_and_first_positional() {
    if std::env::var_os("NEXTEST_TEST_NAME").is_some() {
        return; // running under Nextest: identity comes from its env.
    }
    let command = ["/x/bin/test-binary", "--exact", "module::case"].map(String::from);
    assert_eq!(identity_of(&command), "test-binary module::case");
}

#[test]
fn wait_bounds_print_without_a_trailing_zero() {
    assert_eq!(seconds(30.0), "30");
    assert_eq!(seconds(2.5), "2.5");
}

#[test]
fn output_tail_keeps_only_the_most_recent_bytes() {
    let tail = OutputTail::with_limit(4);
    tail.feed(b"abc");
    tail.feed(b"defg");
    assert_eq!(tail.bytes(), b"defg");
}
