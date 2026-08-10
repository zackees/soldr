#![cfg(feature = "client")]

use std::{fs, path::Path, time::Duration};

use running_process::broker::lifecycle::process_tree::{
    macos_supervisor_contract, MacosKqueueFilter, MacosKqueueNote, MacosSupervisorExitAction,
    MacosSupervisorRaceGuard, MacosSupervisorRegistrationBarrier, MacosSupervisorWatchPid,
    ProcessTreeCleanup, MACOS_SUPERVISOR_KILL_DEADLINE,
};

#[test]
fn macos_supervisor_contract_models_required_kqueue_cleanup() {
    let contract = macos_supervisor_contract();

    assert_eq!(contract.watch_pid, MacosSupervisorWatchPid::BrokerParent);
    assert_eq!(contract.kqueue_filter, MacosKqueueFilter::Process);
    assert_eq!(contract.kqueue_note, MacosKqueueNote::Exit);
    assert_eq!(
        contract.registration_barrier,
        MacosSupervisorRegistrationBarrier::BeforeBackendPipePublication
    );
    assert_eq!(
        contract.race_guard,
        MacosSupervisorRaceGuard::RecheckBrokerAliveAfterRegistration
    );
    assert_eq!(
        contract.exit_action,
        MacosSupervisorExitAction::SigkillBackend
    );
    assert_eq!(contract.kill_deadline, Duration::from_secs(5));
    assert_eq!(contract.kill_deadline, MACOS_SUPERVISOR_KILL_DEADLINE);
    assert_eq!(contract.kqueue_filter_name(), "EVFILT_PROC");
    assert_eq!(contract.kqueue_note_name(), "NOTE_EXIT");
    assert_eq!(contract.termination_signal_name(), "SIGKILL");
}

#[test]
fn macos_process_tree_target_is_concrete_not_planned_or_noop() {
    let target = ProcessTreeCleanup::MacosKqueueSupervisorContract;
    let debug_label = format!("{target:?}");

    assert_ne!(target, ProcessTreeCleanup::UnsupportedNoop);
    assert!(!debug_label.to_ascii_lowercase().contains("planned"));
}

#[test]
fn backend_lifecycle_doc_pins_macos_kqueue_supervisor_contract() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let doc_path = manifest_dir.join("../../docs/v1-backend-lifecycle.md");
    let doc = fs::read_to_string(&doc_path).expect("backend lifecycle doc exists");
    let macos_row = doc
        .lines()
        .find(|line| line.starts_with("| macOS |"))
        .expect("macOS parent-death cleanup row exists");

    assert!(macos_row.contains("EVFILT_PROC"));
    assert!(macos_row.contains("NOTE_EXIT"));
    assert!(macos_row.contains("SIGKILL"));
    assert!(macos_row.contains("MacosKqueueSupervisorContract"));
    assert!(!macos_row.to_ascii_lowercase().contains("planned"));
}
