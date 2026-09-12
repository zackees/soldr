//! soldr#3163: the properties the cold-start diagnosis depends on.

use super::*;

fn parse(line: &str) -> serde_json::Value {
    serde_json::from_str(line).expect("record must be valid JSON")
}

#[test]
fn a_record_carries_the_fields_a_consumer_reads() {
    let record = parse(&render_record(
        4242,
        phase::ROUTE_CLAIM,
        17,
        950,
        1_700_000_000_123,
    ));

    assert_eq!(record["schema_version"], SCHEMA_VERSION);
    assert_eq!(record["event"], "daemon_bringup_phase");
    assert_eq!(record["pid"], 4242);
    assert_eq!(record["phase"], "route_claim");
    assert_eq!(record["phase_ms"], 17);
    assert_eq!(record["total_ms"], 950);
    assert_eq!(record["unix_ms"], 1_700_000_000_123_u64);
}

/// The event name distinguishes these records from the broker's, because both
/// files can be read together when diagnosing one cold start.
#[test]
fn the_event_name_does_not_collide_with_the_broker_bringup_record() {
    let record = parse(&render_record(1, phase::READY, 0, 0, 0));
    assert_eq!(record["event"], "daemon_bringup_phase");
    assert_ne!(record["event"], "broker_bringup_phase");
}

/// Records are one per line: the file is read with a line-oriented parser, and
/// an embedded newline would split one record into two unparseable halves.
#[test]
fn a_record_is_exactly_one_line() {
    let record = render_record(1, phase::COMPILE_SERVICE, 1, 2, 3);
    assert!(!record.contains('\n'), "record must not contain a newline");
}

/// Phase labels are the grep keys for the CI analysis this exists to enable,
/// so an accidental rename is a breaking change to a data contract.
#[test]
fn phase_labels_are_stable_snake_case_identifiers() {
    let labels = [
        phase::TOKIO_RUNTIME,
        phase::RESOLVE_PATHS,
        phase::ROOT_OWNERSHIP,
        phase::CONTROL_ENDPOINT,
        phase::DAEMON_IDENTITY,
        phase::SESSION_LISTENER,
        phase::LIFECYCLE_JOURNAL,
        phase::ROUTE_CLAIM,
        phase::ENDPOINT_SERVERS,
        phase::STATE_STORE,
        phase::COMPILE_SERVICE,
        phase::READY,
    ];
    for label in labels {
        assert!(!label.is_empty());
        assert!(
            label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "{label} must be lowercase snake_case"
        );
    }
    let unique: std::collections::BTreeSet<_> = labels.iter().collect();
    assert_eq!(unique.len(), labels.len(), "phase labels must be distinct");
}

/// The whole point of appending per phase rather than summarising at the end:
/// a daemon that hangs must leave behind everything it finished, with the last
/// line naming the phase it entered and never left.
#[test]
fn a_hang_leaves_the_completed_phases_on_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut recorder = BringupRecorder::new();
    recorder.attach_log(dir.path());

    recorder.phase(phase::RESOLVE_PATHS);
    recorder.phase(phase::ROOT_OWNERSHIP);
    // ...and here the daemon wedges inside `control_endpoint`, so nothing
    // further is ever recorded. The recorder is NOT dropped or flushed.

    let body = std::fs::read_to_string(dir.path().join("daemon-bringup.jsonl"))
        .expect("log must exist while the process is still running");
    let lines: Vec<_> = body.lines().collect();
    assert_eq!(lines.len(), 2, "both completed phases must be durable");
    assert_eq!(parse(lines[0])["phase"], "resolve_paths");
    assert_eq!(
        parse(lines[1])["phase"],
        "root_ownership",
        "the last line names the last phase that COMPLETED; the one that hung is the next one"
    );
}

/// `total_ms` is cumulative and `phase_ms` is per-phase, so a reader can
/// attribute a slow cold start to one phase rather than to the sum.
#[test]
fn total_ms_is_cumulative_while_phase_ms_is_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut recorder = BringupRecorder::new();
    recorder.attach_log(dir.path());

    recorder.phase(phase::RESOLVE_PATHS);
    std::thread::sleep(std::time::Duration::from_millis(25));
    recorder.phase(phase::ROOT_OWNERSHIP);

    let body = std::fs::read_to_string(dir.path().join("daemon-bringup.jsonl")).expect("log");
    let records: Vec<_> = body.lines().map(parse).collect();
    let first_total = records[0]["total_ms"].as_u64().expect("total_ms");
    let second_total = records[1]["total_ms"].as_u64().expect("total_ms");
    let second_phase = records[1]["phase_ms"].as_u64().expect("phase_ms");

    assert!(second_total >= first_total + 20, "total_ms must accumulate");
    assert!(second_phase >= 20, "the sleep must land in phase_ms");
    assert!(
        second_phase <= second_total,
        "a phase cannot be longer than the run that contains it"
    );
}

/// Observability must never be able to break bringup: an unopenable log
/// degrades to stderr-only rather than failing or panicking.
#[test]
fn an_unopenable_log_degrades_to_stderr_instead_of_failing() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A path whose "parent" is a regular file: `create_dir_all` cannot succeed.
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, b"x").expect("write blocker");

    let mut recorder = BringupRecorder::new();
    recorder.attach_log(&blocker);
    recorder.phase(phase::READY);

    assert!(recorder.log.is_none(), "no log should have been opened");
}

/// Phases completed before `SoldrPaths` resolves still get timed; they simply
/// are not backfilled into the file, because that would put records out of the
/// order they happened.
#[test]
fn phases_before_the_log_is_attached_are_not_backfilled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut recorder = BringupRecorder::new();

    recorder.phase(phase::TOKIO_RUNTIME);
    recorder.attach_log(dir.path());
    recorder.phase(phase::RESOLVE_PATHS);

    let body = std::fs::read_to_string(dir.path().join("daemon-bringup.jsonl")).expect("log");
    let lines: Vec<_> = body.lines().collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(parse(lines[0])["phase"], "resolve_paths");
}

/// `resuming` adopts a clock that started earlier, so the runtime build is
/// measured on the same timeline as everything after it.
#[test]
fn resuming_counts_time_that_elapsed_before_the_recorder_existed() {
    let started = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(25));
    let dir = tempfile::tempdir().expect("tempdir");
    let mut recorder = BringupRecorder::resuming(started);
    recorder.attach_log(dir.path());

    recorder.phase(phase::TOKIO_RUNTIME);

    let body = std::fs::read_to_string(dir.path().join("daemon-bringup.jsonl")).expect("log");
    let record = parse(body.lines().next().expect("one record"));
    assert!(
        record["phase_ms"].as_u64().expect("phase_ms") >= 20,
        "time before the recorder was constructed must still be attributed"
    );
}

/// soldr#3174: a sub-phase reports a breakdown of the phase still being timed,
/// so it must NOT advance the phase clock the way `phase` does.
#[test]
fn a_sub_phase_does_not_consume_the_phase_it_breaks_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut recorder = BringupRecorder::new();
    recorder.attach_log(dir.path());

    std::thread::sleep(std::time::Duration::from_millis(25));
    recorder.sub_phase(phase::COMPILE_SERVICE_ZCCACHE_START, 20);
    recorder.phase(phase::COMPILE_SERVICE);

    let body = std::fs::read_to_string(dir.path().join("daemon-bringup.jsonl")).expect("log");
    let records: Vec<_> = body.lines().map(parse).collect();
    assert_eq!(records[0]["phase"], "compile_service.zccache_start");
    assert_eq!(
        records[0]["phase_ms"], 20,
        "a sub-phase reports the duration it was given, not elapsed time"
    );
    assert!(
        records[1]["phase_ms"].as_u64().expect("phase_ms") >= 20,
        "the enclosing phase must still see the whole 25ms, not have it consumed \
         by the sub-phase before it"
    );
}

/// The breakdown can legitimately exceed the phase that encloses it.
///
/// `compile_service` times the *await* of a task spawned earlier and run
/// concurrently with the state-store open, so a task that finished while the
/// daemon was doing something else reports ~0 ms even though its work took
/// longer. Measured locally: aggregate 0 ms, `zccache_start` 58 ms. A reader
/// comparing the two must not treat that as an inconsistency -- it is the
/// concurrency working.
#[test]
fn a_sub_phase_may_exceed_the_phase_it_belongs_to() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut recorder = BringupRecorder::new();
    recorder.attach_log(dir.path());

    recorder.sub_phase(phase::COMPILE_SERVICE_ZCCACHE_START, 58);
    recorder.phase(phase::COMPILE_SERVICE);

    let body = std::fs::read_to_string(dir.path().join("daemon-bringup.jsonl")).expect("log");
    let records: Vec<_> = body.lines().map(parse).collect();
    let sub = records[0]["phase_ms"].as_u64().expect("phase_ms");
    let aggregate = records[1]["phase_ms"].as_u64().expect("phase_ms");
    assert!(sub > aggregate, "this is expected, not a bug");
}

/// Sub-phase labels are namespaced under the phase they decompose, so a
/// consumer can group them without a separate table.
#[test]
fn sub_phase_labels_are_namespaced_under_their_phase() {
    for label in [
        phase::COMPILE_SERVICE_PREPARE_ROOT,
        phase::COMPILE_SERVICE_SCRUB_JOURNALS,
        phase::COMPILE_SERVICE_ZCCACHE_START,
    ] {
        let (parent, leaf) = label.split_once('.').expect("sub-phases are dotted");
        assert_eq!(parent, phase::COMPILE_SERVICE);
        assert!(!leaf.is_empty());
    }
}
