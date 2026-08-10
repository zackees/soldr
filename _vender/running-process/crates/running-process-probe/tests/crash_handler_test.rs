//! Platform-real crash acceptance for #636.
//!
//! Every fault occurs in `testbin-probe-crasher`; a regression cannot take
//! down this harness or strand the rest of the suite.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use running_process_probe::crash::spool::{parse, RawCrashReport, RECORD_SIZE};

fn fixture() -> PathBuf {
    let debug_dir = std::env::current_exe()
        .expect("test exe")
        .parent()
        .and_then(Path::parent)
        .expect("target debug dir")
        .to_path_buf();
    debug_dir.join(if cfg!(windows) {
        "testbin-probe-crasher.exe"
    } else {
        "testbin-probe-crasher"
    })
}

fn run_crasher(spool: &Path, extra: &[&str], opt_out: bool) -> Output {
    #[cfg(unix)]
    if spool.exists() {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(spool, std::fs::Permissions::from_mode(0o700))
            .expect("make fixture spool owner-private");
    }
    let fixture = fixture();
    assert!(
        fixture.exists(),
        "missing fixture {}; build it first with `soldr cargo build -p testbins --bin testbin-probe-crasher`",
        fixture.display()
    );
    let mut command = Command::new(fixture);
    command.arg("--spool").arg(spool).args(extra);
    if opt_out {
        command.env("RUNNING_PROCESS_PROBE_NO_CRASH_HANDLER", "1");
    }
    command.output().expect("spawn crash fixture")
}

fn only_report(spool: &Path) -> RawCrashReport {
    let entries = std::fs::read_dir(spool)
        .expect("spool dir")
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "expected one record: {entries:?}");
    let bytes = std::fs::read(&entries[0]).unwrap();
    assert_eq!(
        bytes.len(),
        RECORD_SIZE,
        "callback must write one full record"
    );
    parse(&bytes).expect("parse crash record")
}

#[test]
fn crash_capture_produces_app_tagged_all_thread_raw_report() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_crasher(temp.path(), &["--mode", "segv", "--stress"], false);
    assert!(
        !output.status.success(),
        "faulting child unexpectedly survived"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("READY"),
        "fixture faulted before arming: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = only_report(temp.path());
    assert_eq!(report.metadata.app_class, "crash-fixture");
    assert_eq!(report.metadata.app_version, "636");
    assert_ne!(report.fault_code, 0);
    assert!(!report.raw_context.is_empty());
    assert!(
        !report.modules.is_empty(),
        "sample lost its pre-ASLR module inventory"
    );
    assert!(
        report
            .threads
            .iter()
            .flat_map(|thread| &thread.frames)
            .any(|frame| frame.module_index.is_some()),
        "sample contains no module-relative frames"
    );
    assert!(
        report.threads.len() >= 8,
        "allocator-pressure workers missing from all-thread report: {}",
        report.threads.len()
    );
}

#[test]
fn dropping_metadata_owner_retags_an_immediate_fault() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_crasher(
        temp.path(),
        &["--mode", "segv", "--reselect-metadata"],
        false,
    );
    assert!(!output.status.success());
    let report = only_report(temp.path());
    assert_eq!(
        report.metadata.app_class, "crash-fixture",
        "fatal-visible metadata was not published before Guard::drop returned"
    );
}

#[test]
fn prior_native_handler_still_runs_after_probe_callback() {
    let temp = tempfile::tempdir().unwrap();
    let spool = temp.path().join("spool");
    let sentinel = temp.path().join("prior-ran");
    let output = run_crasher(
        &spool,
        &[
            "--mode",
            "segv",
            "--prior",
            sentinel.to_str().expect("utf8 temp path"),
        ],
        false,
    );
    assert!(!output.status.success());
    let report = only_report(&spool);
    assert_ne!(report.fault_code, 0, "probe callback did not run");
    assert_eq!(
        std::fs::read(&sentinel).expect("prior handler sentinel"),
        b"prior-handler-ran",
        "probe swallowed the app's prior handler"
    );
}

#[test]
fn prior_abort_handler_still_runs_after_probe_callback() {
    let temp = tempfile::tempdir().unwrap();
    let spool = temp.path().join("spool");
    let sentinel = temp.path().join("prior-ran");
    let output = run_crasher(
        &spool,
        &[
            "--mode",
            "abort",
            "--prior",
            sentinel.to_str().expect("utf8 temp path"),
        ],
        false,
    );
    assert!(!output.status.success());
    let report = only_report(&spool);
    assert_ne!(report.fault_code, 0, "probe callback did not run");
    assert_eq!(
        std::fs::read(&sentinel).expect("prior abort handler sentinel"),
        b"prior-handler-ran",
        "probe swallowed the app's prior abort handler"
    );
}

#[test]
fn environment_opt_out_touches_no_crash_sink() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_crasher(temp.path(), &["--mode", "segv"], true);
    assert!(
        !output.status.success(),
        "OS default action must still crash"
    );
    let entries = std::fs::read_dir(temp.path())
        .map(|items| items.count())
        .unwrap_or(0);
    assert_eq!(entries, 0, "opt-out unexpectedly created a crash record");
}

#[test]
fn allocator_pressure_does_not_deadlock_or_truncate_handler_write() {
    for attempt in 0..3 {
        let temp = tempfile::tempdir().unwrap();
        let output = run_crasher(temp.path(), &["--mode", "segv", "--stress"], false);
        assert!(!output.status.success(), "attempt {attempt} survived");
        let report = only_report(temp.path());
        assert!(
            report.raw_context.len() >= 64,
            "attempt {attempt} wrote no usable context"
        );
    }
}
