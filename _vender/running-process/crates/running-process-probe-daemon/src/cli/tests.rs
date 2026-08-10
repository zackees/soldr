//! Tests for the `rpprobe` command surface (S14 / #643).
//!
//! Argument parsing and rendering are exercised in-process. The end-to-end
//! path — a real daemon, a real socket, a real capture selection — lives in
//! `tests/rpprobe_cli_test.rs`, because it needs a daemon process and these
//! do not.

use clap::Parser as _;

use super::*;

// --- argument parsing -----------------------------------------------------

#[test]
fn ps_accepts_a_name_glob_and_a_limit() {
    let cli = Cli::parse_from(["rpprobe", "ps", "--name", "*worker*", "--limit", "5"]);
    match cli.command {
        Command::Ps { name, limit, .. } => {
            assert_eq!(name.as_deref(), Some("*worker*"));
            assert_eq!(limit, Some(5));
        }
        other => panic!("expected Ps, got {other:?}"),
    }
}

#[test]
fn dump_takes_either_a_pid_or_a_selection() {
    let by_pid = Cli::parse_from(["rpprobe", "dump", "4242"]);
    match by_pid.command {
        Command::Dump { pid, all, .. } => {
            assert_eq!(pid, Some(4242));
            assert!(!all);
        }
        other => panic!("expected Dump, got {other:?}"),
    }

    let by_name = Cli::parse_from(["rpprobe", "dump", "--name", "*worker*", "--all"]);
    match by_name.command {
        Command::Dump { pid, name, all, .. } => {
            assert_eq!(pid, None);
            assert_eq!(name.as_deref(), Some("*worker*"));
            assert!(all);
        }
        other => panic!("expected Dump, got {other:?}"),
    }
}

#[test]
fn global_flags_work_after_the_subcommand() {
    // `--json` is global, so an operator who types it where it reads naturally
    // (at the end) gets JSON rather than a parse error.
    let cli = Cli::parse_from(["rpprobe", "crashes", "--class", "clud", "--json"]);
    assert!(cli.json);
    match cli.command {
        Command::Crashes { class, stats, .. } => {
            assert_eq!(class.as_deref(), Some("clud"));
            assert!(!stats);
        }
        other => panic!("expected Crashes, got {other:?}"),
    }
}

#[test]
fn crashes_stats_is_a_flag_not_a_separate_subcommand() {
    let cli = Cli::parse_from(["rpprobe", "crashes", "--stats", "--class-like", "clud%"]);
    match cli.command {
        Command::Crashes {
            stats, class_like, ..
        } => {
            assert!(stats);
            assert_eq!(class_like.as_deref(), Some("clud%"));
        }
        other => panic!("expected Crashes, got {other:?}"),
    }
}

#[test]
fn the_cli_definition_is_internally_consistent() {
    // clap's own invariant check. It catches duplicate long flags and
    // conflicting shorts at test time rather than when an operator types the
    // one combination nobody tried.
    use clap::CommandFactory as _;
    Cli::command().debug_assert();
}

// --- rendering ------------------------------------------------------------

#[test]
fn a_table_aligns_columns_to_their_widest_cell() {
    let rendered = render::processes(
        &[
            wire_process(1, "a", "clud", "/work"),
            wire_process(222222, "much-longer-name", "clud-worker", "/srv"),
        ],
        false,
    );
    let lines: Vec<&str> = rendered.lines().collect();
    assert!(lines[0].starts_with("PID"));
    // The short pid is padded out to the width of the long one, so the NAME
    // column starts at the same offset on both rows.
    let name_column = lines[0].find("NAME").expect("NAME header");
    assert_eq!(&lines[1][name_column..name_column + 1], "a");
    assert_eq!(&lines[2][name_column..name_column + 4], "much");
}

#[test]
fn json_output_is_machine_readable() {
    let rendered = render::processes(&[wire_process(7, "svc", "clud", "/work")], true);
    let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid json");
    assert_eq!(parsed[0]["pid"].as_u64(), Some(7));
    assert_eq!(parsed[0]["name"].as_str(), Some("svc"));
}

#[test]
fn an_empty_result_says_so_rather_than_printing_a_bare_header() {
    assert_eq!(render::processes(&[], false), "no processes match\n");
    assert_eq!(render::crashes(&[], false), "no crashes match\n");
}

#[test]
fn a_timestamp_renders_as_utc() {
    // 2021-01-01T00:00:00Z. A wrong epoch or a wrong leap-year rule shows up
    // here immediately.
    assert_eq!(render::millis(1_609_459_200_000), "2021-01-01 00:00:00Z");
    // A leap day, which the civil-calendar conversion has to get right.
    assert_eq!(render::millis(1_582_934_400_000), "2020-02-29 00:00:00Z");
    assert_eq!(render::millis(0), "-");
}

#[test]
fn byte_counts_use_binary_units() {
    assert_eq!(render::bytes(512), "512 B");
    assert_eq!(render::bytes(2048), "2.0 KiB");
    assert_eq!(render::bytes(5 * 1024 * 1024), "5.0 MiB");
}

#[test]
fn a_doctor_report_marks_each_failing_check() {
    let report = render::doctor(
        &[
            ("discovery file".into(), true, "/tmp/x".into()),
            ("symbolizer".into(), false, "not found".into()),
        ],
        false,
    );
    assert!(report.contains("ok"));
    assert!(report.contains("FAIL"));
    assert!(report.contains("not found"));
}

#[test]
fn a_json_doctor_report_is_parseable_and_keeps_the_verdicts() {
    let report = render::doctor(&[("socket".into(), false, "refused".into())], true);
    let parsed: serde_json::Value = serde_json::from_str(&report).expect("valid json");
    assert_eq!(parsed[0]["ok"].as_bool(), Some(false));
    assert_eq!(parsed[0]["check"].as_str(), Some("socket"));
}

fn wire_process(
    pid: u64,
    name: &str,
    app_class: &str,
    cwd: &str,
) -> running_process_probe::probe_diag::v1::ProcessInfo {
    running_process_probe::probe_diag::v1::ProcessInfo {
        key: Some(running_process_probe::probe_diag::v1::ProcessKey {
            pid,
            start_time: Some(1),
            boot_id: None,
        }),
        name: name.to_string(),
        app_class: app_class.to_string(),
        cwd: cwd.to_string(),
        registered: true,
        ..Default::default()
    }
}
