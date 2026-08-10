use crate::helpers::with_locked_env_var;
use crate::pid_tracking::{
    list_tracked_processes, native_list_active_processes, native_register_process,
    native_unregister_process,
};
use crate::registry::{
    register_active_process, tracked_process_db_path, unregister_active_process,
    ActiveProcessRecord,
};

// ── Process tracking tests (requires PyO3) ──

#[test]
fn process_registry_register_list_unregister() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let test_pid = 99999u32;
        // Register
        native_register_process(test_pid, "test", "test-command", None).unwrap();
        // List
        let list = native_list_active_processes();
        let found = list.iter().any(|(pid, _, _, _, _)| *pid == test_pid);
        assert!(found, "registered pid should appear in active list");
        // Unregister
        native_unregister_process(test_pid).unwrap();
        let list = native_list_active_processes();
        let found = list.iter().any(|(pid, _, _, _, _)| *pid == test_pid);
        assert!(!found, "unregistered pid should not appear in active list");
    });
}

// ── tracked_process_db_path tests ──

#[test]
fn tracked_process_db_path_returns_ok() {
    with_locked_env_var("RUNNING_PROCESS_PID_DB", None, || {
        let path = tracked_process_db_path();
        assert!(path.is_ok());
        let path = path.unwrap();
        assert_eq!(
            path.file_name(),
            Some(std::ffi::OsStr::new("tracked-pids.sqlite3")),
            "path should use the default tracked pid database filename: {:?}",
            path
        );
    });
}

// ── Process registry additional tests ──

#[test]
fn process_registry_register_with_cwd() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let test_pid = 99998u32;
        native_register_process(test_pid, "test", "test-cmd", Some("/tmp/test".to_string()))
            .unwrap();
        let list = native_list_active_processes();
        let entry = list.iter().find(|(pid, _, _, _, _)| *pid == test_pid);
        assert!(entry.is_some());
        let (_, kind, cmd, cwd, _) = entry.unwrap();
        assert_eq!(kind, "test");
        assert_eq!(cmd, "test-cmd");
        assert_eq!(cwd.as_deref(), Some("/tmp/test"));
        native_unregister_process(test_pid).unwrap();
    });
}

#[test]
fn process_registry_double_register_overwrites() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let test_pid = 99997u32;
        native_register_process(test_pid, "first", "cmd1", None).unwrap();
        native_register_process(test_pid, "second", "cmd2", None).unwrap();
        let list = native_list_active_processes();
        let entries: Vec<_> = list
            .iter()
            .filter(|(pid, _, _, _, _)| *pid == test_pid)
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, "second");
        native_unregister_process(test_pid).unwrap();
    });
}

#[test]
fn process_registry_unregister_nonexistent_no_error() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        // Should not error when unregistering a PID that doesn't exist
        let result = native_unregister_process(99996);
        assert!(result.is_ok());
    });
}

// ── list_tracked_processes tests ──

#[test]
fn list_tracked_processes_returns_ok() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|_py| {
        let result = list_tracked_processes();
        assert!(result.is_ok());
    });
}

// ── tracked_process_db_path additional tests ──

#[test]
fn tracked_process_db_path_with_env() {
    pyo3::Python::initialize();
    with_locked_env_var(
        "RUNNING_PROCESS_PID_DB",
        Some("/custom/path/db.sqlite3"),
        || {
            let result = tracked_process_db_path().unwrap();
            assert_eq!(result, std::path::PathBuf::from("/custom/path/db.sqlite3"));
        },
    );
}

#[test]
fn tracked_process_db_path_empty_env_falls_back() {
    pyo3::Python::initialize();
    with_locked_env_var("RUNNING_PROCESS_PID_DB", Some("   "), || {
        let result = tracked_process_db_path().unwrap();
        assert_eq!(
            result.file_name(),
            Some(std::ffi::OsStr::new("tracked-pids.sqlite3"))
        );
    });
}

// ── ActiveProcessRecord ──

#[test]
fn active_process_record_clone() {
    let record = ActiveProcessRecord {
        pid: 1234,
        kind: "test".to_string(),
        command: "echo".to_string(),
        cwd: Some("/tmp".to_string()),
        started_at: 1000.0,
    };
    let cloned = record.clone();
    assert_eq!(cloned.pid, 1234);
    assert_eq!(cloned.kind, "test");
    assert_eq!(cloned.command, "echo");
    assert_eq!(cloned.cwd, Some("/tmp".to_string()));
}

#[test]
fn register_and_list_active_processes() {
    let fake_pid = 777777u32;
    register_active_process(
        fake_pid,
        "test",
        "echo hello",
        Some("/tmp".to_string()),
        1000.0,
    );
    let items = native_list_active_processes();
    assert!(items.iter().any(|e| e.0 == fake_pid));
    unregister_active_process(fake_pid);
    let items = native_list_active_processes();
    assert!(!items.iter().any(|e| e.0 == fake_pid));
}
