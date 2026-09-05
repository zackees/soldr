//! soldr#1813 end-of-build log-path summary tests.
//!
//! Split out of the sibling `tests.rs` (soldr#2493): converting the retired
//! watchdog-macro call sites to plain `#[test] fn` costs one line per test,
//! which pushed that
//! already-over-ceiling file further over. This block is self-contained -- one
//! fixture builder plus the four tests that use it -- so it is the natural
//! seam.

use super::*;

// ---------------------------------------------------------------------------
// soldr#1813 — end-of-build log-path summary
// ---------------------------------------------------------------------------

/// Every path-bearing field of `BuildLogPaths` populated, so a test can assert
/// the summary omits none of them.
fn all_log_paths() -> crate::daemon::protocol::BuildLogPaths {
    crate::daemon::protocol::BuildLogPaths {
        zccache_session_id: Some("session-abc".to_string()),
        cache_dir: Some(r"C:\state\zccache".to_string()),
        session_log_path: Some(r"C:\state\zccache\logs\last-session.log".to_string()),
        journal_path: Some(r"C:\state\zccache\logs\last-session.jsonl".to_string()),
        session_stats_path: Some(r"C:\state\zccache\logs\last-session-stats.json".to_string()),
        compile_journal_path: Some(r"C:\state\zccache\logs\compile_journal.jsonl".to_string()),
        archived_session_log_path: Some(r"C:\state\hist\7\last-session.log".to_string()),
        archived_journal_path: Some(r"C:\state\hist\7\last-session.jsonl".to_string()),
        archived_session_stats_path: Some(r"C:\state\hist\7\last-session-stats.json".to_string()),
        archived_compile_journal_path: Some(r"C:\state\hist\7\compile_journal.jsonl".to_string()),
        private_daemon_name: Some("legacy-name".to_string()),
    }
}

#[test]
fn log_summary_lists_exactly_the_recorded_build_log_paths() {
    let log_paths = all_log_paths();
    let logs = log_summary::SessionLogs {
        build_log: Some(PathBuf::from(r"C:\state\logs\builds\build-7.xml")),
        build_log_paths: Some(log_paths.clone()),
        compile_fallback_log: Some(PathBuf::from(
            r"C:\state\logs\compile-daemon-fallbacks.jsonl",
        )),
    };
    let summary = log_summary::summary_message(&logs, false).expect("summary for a logged build");

    // Every recorded log path appears.
    for path in [
        log_paths.session_log_path.as_deref(),
        log_paths.journal_path.as_deref(),
        log_paths.session_stats_path.as_deref(),
        log_paths.compile_journal_path.as_deref(),
        log_paths.archived_session_log_path.as_deref(),
        log_paths.archived_journal_path.as_deref(),
        log_paths.archived_session_stats_path.as_deref(),
        log_paths.archived_compile_journal_path.as_deref(),
    ] {
        let path = path.expect("fixture populates every path field");
        assert!(summary.contains(path), "summary omitted {path}:\n{summary}");
    }
    assert!(summary.contains(r"C:\state\logs\builds\build-7.xml"));
    assert!(summary.contains(r"C:\state\logs\compile-daemon-fallbacks.jsonl"));

    // ...and nothing that isn't a log file does. The session id and cache dir
    // are correlation data, not paths the user should be pointed at, and
    // private_daemon_name is a retired field kept only for old records.
    assert!(!summary.contains("session-abc"), "summary:\n{summary}");
    assert!(!summary.contains("legacy-name"), "summary:\n{summary}");

    // One line per entry, plus the header and the closing hint.
    assert_eq!(summary.lines().count(), 10 + 2, "summary:\n{summary}");
    assert!(summary.contains("soldr logs paths"));
}

#[test]
fn log_summary_omits_paths_the_build_never_wrote() {
    // The common case: the embedded service no longer touches zccache's fixed
    // `last-session` files, so those fields are None and must not be printed
    // as if they were fresh.
    let logs = log_summary::SessionLogs {
        build_log: Some(PathBuf::from("/state/logs/builds/build-1.xml")),
        build_log_paths: Some(crate::daemon::protocol::BuildLogPaths {
            session_log_path: None,
            journal_path: None,
            archived_session_log_path: None,
            archived_journal_path: None,
            ..all_log_paths()
        }),
        compile_fallback_log: None,
    };
    let summary = log_summary::summary_message(&logs, false).expect("summary");
    assert!(!summary.contains("last-session.log"), "summary:\n{summary}");
    assert!(
        !summary.contains("last-session.jsonl"),
        "summary:\n{summary}"
    );
    assert!(
        !summary.contains("compile-daemon-fallbacks"),
        "summary:\n{summary}"
    );
    assert!(
        summary.contains("last-session-stats.json"),
        "summary:\n{summary}"
    );
}

#[test]
fn log_summary_is_absent_when_no_logs_were_written() {
    // A build that wrote nothing must print nothing at all — not an empty
    // header with a dangling hint.
    let summary = log_summary::summary_message(&log_summary::SessionLogs::default(), false);
    assert!(summary.is_none(), "unexpected summary: {summary:?}");
}

#[test]
fn log_summary_colorizes_only_when_asked() {
    let logs = log_summary::SessionLogs {
        build_log: Some(PathBuf::from("/state/logs/builds/build-1.xml")),
        ..log_summary::SessionLogs::default()
    };
    let plain = log_summary::summary_message(&logs, false).expect("summary");
    assert!(
        !plain.contains('\x1b'),
        "NO_COLOR output must be plain: {plain:?}"
    );
    let colored = log_summary::summary_message(&logs, true).expect("summary");
    assert!(
        colored.contains('\x1b'),
        "colored output must use ANSI: {colored:?}"
    );
    // Color is decoration only — the path itself is unchanged.
    assert!(colored.contains("/state/logs/builds/build-1.xml"));
}

#[test]
fn summary_prints_on_failure_or_terminal_only() {
    // A red build always names its logs; a green one only on a terminal,
    // never into a pipe where an orchestrator repeats it per nested call.
    assert!(log_summary::summary_wanted(1, false));
    assert!(log_summary::summary_wanted(-1, false));
    assert!(log_summary::summary_wanted(0, true));
    assert!(!log_summary::summary_wanted(0, false));
}
