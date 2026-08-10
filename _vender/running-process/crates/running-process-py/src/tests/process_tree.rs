use sysinfo::System;

use crate::helpers::{descendant_pids, system_pid};
use crate::process_tree::{
    kill_process_tree_impl, native_get_process_tree_info, native_launch_detached,
};
use crate::registry::{process_created_at, same_process_identity};

// ── kill_process_tree_impl tests ──

#[test]
fn kill_process_tree_nonexistent_pid_no_panic() {
    // Should not panic when given a PID that doesn't exist
    kill_process_tree_impl(99999999, 0.1);
}

// ── descendant_pids tests ──

#[test]
fn descendant_pids_returns_empty_for_unknown_pid() {
    let system = System::new();
    let pid = system_pid(99999999);
    let descendants = descendant_pids(&system, pid);
    assert!(descendants.is_empty());
}

// ── same_process_identity tests ──

#[test]
fn same_process_identity_nonexistent_pid() {
    assert!(!same_process_identity(99999999, 0.0, 1.0));
}

// ── Iteration 3: Utility function tests ──

#[test]
fn kill_process_tree_nonexistent_pid_is_noop() {
    kill_process_tree_impl(999999, 0.5);
}

#[test]
fn get_process_tree_info_current_pid() {
    let pid = std::process::id();
    let info = native_get_process_tree_info(pid);
    assert!(info.contains(&format!("{}", pid)));
}

#[test]
fn get_process_tree_info_nonexistent_pid() {
    let info = native_get_process_tree_info(999999);
    assert!(info.contains("Could not get process info"));
}

#[test]
fn process_created_at_current_process_returns_some() {
    let created = process_created_at(std::process::id());
    assert!(created.is_some());
    assert!(created.unwrap() > 0.0);
}

#[test]
fn process_created_at_nonexistent_returns_none() {
    assert!(process_created_at(999999).is_none());
}

#[test]
fn same_process_identity_current_process_matches() {
    let pid = std::process::id();
    let created = process_created_at(pid).unwrap();
    assert!(same_process_identity(pid, created, 2.0));
}

#[test]
fn same_process_identity_wrong_time_no_match() {
    assert!(!same_process_identity(std::process::id(), 0.0, 1.0));
}

// ── native_launch_detached tests ──

#[test]
fn native_launch_detached_rejects_empty_command_without_daemon() {
    pyo3::Python::initialize();
    pyo3::Python::attach(|py| {
        let err = native_launch_detached(py, "   ".to_string(), None, None, None)
            .expect_err("empty commands should be rejected before daemon IPC");
        assert!(err.is_instance_of::<pyo3::exceptions::PyValueError>(py));
    });
}
