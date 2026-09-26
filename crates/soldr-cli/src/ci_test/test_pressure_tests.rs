use super::super::test_admission::{resolve, AdmissionInputs, MemorySource};
use super::*;
use std::sync::Mutex;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn controller() -> (PressureController, Instant) {
    (
        PressureController::new(GIB, 2 * GIB, Duration::from_secs(2)),
        Instant::now(),
    )
}

#[test]
fn high_pressure_pauses_and_only_low_pressure_resumes() {
    let (mut gate, start) = controller();
    assert_eq!(gate.observe(Some(3 * GIB), start), None);
    assert_eq!(
        gate.observe(Some(GIB - 1), start + Duration::from_secs(1)),
        Some(Transition::Pause)
    );
    // Inside the band: above the pause mark is not enough to resume.
    assert_eq!(
        gate.observe(Some(GIB + MIB), start + Duration::from_secs(5)),
        None
    );
    assert!(gate.paused());
    assert_eq!(
        gate.observe(Some(2 * GIB), start + Duration::from_secs(6)),
        Some(Transition::Resume)
    );
    assert!(!gate.paused());
}

#[test]
fn a_reading_oscillating_across_both_marks_cannot_flap_the_gate() {
    let (mut gate, start) = controller();
    let mut transitions = Vec::new();
    // Sampled every 250 ms for 10 s, swinging from well below the pause mark
    // to well above the resume mark on every sample.
    for step in 0..40u64 {
        let available = if step % 2 == 0 { GIB / 2 } else { 3 * GIB };
        let now = start + Duration::from_millis(250 * step);
        if let Some(transition) = gate.observe(Some(available), now) {
            transitions.push((now - start, transition));
        }
    }
    assert!(transitions.len() <= 5, "{transitions:?}");
    for pair in transitions.windows(2) {
        assert!(
            pair[1].0 - pair[0].0 >= Duration::from_secs(2),
            "{transitions:?}"
        );
        assert_ne!(
            pair[0].1, pair[1].1,
            "transitions alternate: {transitions:?}"
        );
    }
}

#[test]
fn unreadable_memory_never_changes_the_gate() {
    let (mut gate, start) = controller();
    assert_eq!(gate.observe(None, start), None);
    assert_eq!(gate.observe(Some(0), start), Some(Transition::Pause));
    assert_eq!(gate.observe(None, start + Duration::from_secs(60)), None);
    assert!(gate.paused());
}

#[test]
fn a_degenerate_band_still_has_hysteresis() {
    let mut gate = PressureController::new(GIB, GIB, Duration::ZERO);
    let now = Instant::now();
    assert_eq!(gate.observe(Some(GIB - 1), now), Some(Transition::Pause));
    assert_eq!(
        gate.observe(Some(GIB), now),
        None,
        "resume is strictly above pause"
    );
    assert_eq!(gate.observe(Some(GIB + 1), now), Some(Transition::Resume));
}

#[test]
fn percent_decoding_round_trips_the_wrapper_record_names() {
    assert_eq!(
        percent_decode("soldr-cli::guards cli%2Fci::test%25name"),
        "soldr-cli::guards cli/ci::test%name"
    );
    assert_eq!(percent_decode("trailing%2"), "trailing%2");
    assert_eq!(percent_decode("bad%zzhex"), "bad%zzhex");
    assert_eq!(percent_decode("ünïcode%2F"), "ünïcode/");
}

fn fixture_admission() -> TestAdmission {
    let mut inputs = AdmissionInputs::from_process();
    inputs.explicit = None;
    inputs.logical_cpus = 4;
    inputs.memory.available_bytes = Some(8 * GIB);
    inputs.memory.source = MemorySource::Override;
    inputs.per_test_budget_bytes = GIB;
    inputs.reserve_bytes = 2 * GIB;
    inputs.ceiling_bytes = Some(4 * GIB);
    resolve(&inputs)
}

fn reading(available: u64) -> MemoryReading {
    MemoryReading {
        available_bytes: Some(available),
        source: MemorySource::MemAvailable,
        limit_bytes: None,
        limit_unbounded: true,
        pids_current: None,
        pids_limit: None,
    }
}

#[test]
fn stage_env_names_the_directory_the_ceiling_and_the_summary() {
    let base = tempfile::tempdir().expect("tempdir");
    let admission = NextestAdmission::new(base.path(), fixture_admission());
    let env: std::collections::BTreeMap<_, _> = admission.stage_env().into_iter().collect();
    assert_eq!(
        env[ADMISSION_DIR_ENV],
        admission.dir().display().to_string()
    );
    assert_eq!(env[CEILING_ENV], (4 * GIB).to_string());
    assert!(env[SUMMARY_ENV].contains("effective=4 (measured)"));
}

#[test]
fn the_monitor_drives_the_paused_flag_and_cleans_up() {
    let base = tempfile::tempdir().expect("tempdir");
    let admission = NextestAdmission::new(base.path(), fixture_admission());
    // Scripted memory: pressure for the first samples, then recovery. The
    // dwell (2 s) separates the pause from the resume.
    let script = Arc::new(Mutex::new(
        std::iter::repeat_n(GIB / 2, 30)
            .chain(std::iter::repeat(4 * GIB))
            .map(reading),
    ));
    let probe_script = Arc::clone(&script);
    let monitor = admission
        .start_with(Duration::from_millis(10), move || {
            probe_script.lock().unwrap().next().unwrap()
        })
        .expect("start monitor");
    let flag = admission.dir().join(PAUSED_FLAG);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !flag.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(flag.exists(), "pressure must pause admissions");
    while flag.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!flag.exists(), "recovery must resume admissions");

    std::fs::create_dir_all(admission.dir().join(INFRA_DIR)).unwrap();
    std::fs::write(
        admission
            .dir()
            .join(INFRA_DIR)
            .join("soldr-cli::guards memory%3A%3Ahog"),
        b"",
    )
    .unwrap();
    let (stats, failures) = monitor.finish();
    assert_eq!(stats.pauses, 1);
    assert!(stats.paused_total >= MIN_DWELL);
    assert_eq!(stats.lowest_available, Some(GIB / 2));
    assert_eq!(failures, ["soldr-cli::guards memory::hog"]);
    assert!(
        !admission.dir().exists(),
        "the admission directory is removed"
    );
}

#[test]
fn running_tests_ignore_dead_and_malformed_slots() {
    let base = tempfile::tempdir().expect("tempdir");
    let active = base.path().join(ACTIVE_DIR);
    std::fs::create_dir_all(&active).unwrap();
    std::fs::write(active.join(std::process::id().to_string()), b"").unwrap();
    std::fs::write(active.join("4294967294"), b"").unwrap();
    std::fs::write(active.join("not-a-pid"), b"").unwrap();
    assert_eq!(running_tests(base.path()), 1);
}
