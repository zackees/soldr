//! soldr#3389: the always-forward / always-log / failures-carry-cause
//! contract of the shared small-tool helper.

use super::*;

fn read_records(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .expect("small-tool log written")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json record"))
        .collect()
}

#[test]
fn bytes_excerpt_truncates_and_trims() {
    let long = vec![b'x'; 2000];
    assert_eq!(bytes_excerpt(&long).len(), STDERR_EXCERPT_BYTES);
    assert_eq!(bytes_excerpt(b"  hi \n"), "hi");
    assert_eq!(bytes_excerpt(b""), "<empty>");
}

#[test]
fn failure_error_carries_stderr_marker_and_is_logged() {
    let temp = tempfile::tempdir().unwrap();
    let tool = write_fake_tool(temp.path(), "faketool", "", "MARKER_FAIL_3389", 1);
    let log = temp.path().join("logs").join("small-tools.jsonl");
    let mut forwarded = Vec::new();
    let mut command = Command::new(&tool);
    let err = run_small_tool_with_sinks(
        &mut command,
        "faketool --probe",
        None,
        ToolSinks {
            stderr: &mut forwarded,
            log_path: Some(log.clone()),
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("MARKER_FAIL_3389"), "{err}");
    let forwarded = String::from_utf8(forwarded).unwrap();
    assert!(
        forwarded.contains("faketool --probe: MARKER_FAIL_3389"),
        "{forwarded}"
    );
    let records = read_records(&log);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["exit_code"], 1);
    assert!(records[0]["stderr"]
        .as_str()
        .unwrap()
        .contains("MARKER_FAIL_3389"));
}

#[test]
fn success_forwards_stderr_and_logs_both_streams() {
    let temp = tempfile::tempdir().unwrap();
    let tool = write_fake_tool(temp.path(), "faketool", "OUT_OK", "MARKER_OK_3389", 0);
    let log = temp.path().join("small-tools.jsonl");
    let mut forwarded = Vec::new();
    let mut command = Command::new(&tool);
    let output = run_small_tool_with_sinks(
        &mut command,
        "faketool",
        None,
        ToolSinks {
            stderr: &mut forwarded,
            log_path: Some(log.clone()),
        },
    )
    .expect("exit 0 succeeds");
    assert!(String::from_utf8_lossy(&output.stdout).contains("OUT_OK"));
    let forwarded = String::from_utf8(forwarded).unwrap();
    assert!(forwarded.contains("faketool: MARKER_OK_3389"), "{forwarded}");
    let records = read_records(&log);
    assert_eq!(records[0]["exit_code"], 0);
    assert!(records[0]["stdout"].as_str().unwrap().contains("OUT_OK"));
    assert!(records[0]["stderr"]
        .as_str()
        .unwrap()
        .contains("MARKER_OK_3389"));
    assert!(records[0]["argv"][0]
        .as_str()
        .unwrap()
        .contains("faketool"));
    assert!(records[0]["duration_ms"].is_u64());
}

#[test]
fn rustc_version_line_error_carries_fake_rustc_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let rustc = write_fake_tool(temp.path(), "rustc", "", "MARKER_RUSTC_V_3381", 1);
    let mut forwarded = Vec::new();
    let err = rustc_version_line_with_sinks(
        &rustc,
        ToolSinks {
            stderr: &mut forwarded,
            log_path: None,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("MARKER_RUSTC_V_3381"), "{err}");
}

#[test]
fn rustc_version_line_success_returns_first_line() {
    let temp = tempfile::tempdir().unwrap();
    let rustc = write_fake_tool(temp.path(), "rustc", "rustc 9.9.9 (fake)", "", 0);
    let mut forwarded = Vec::new();
    let line = rustc_version_line_with_sinks(
        &rustc,
        ToolSinks {
            stderr: &mut forwarded,
            log_path: None,
        },
    )
    .unwrap();
    assert_eq!(line, "rustc 9.9.9 (fake)");
}

#[test]
fn spawn_failure_names_the_missing_binary_and_is_logged() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("small-tools.jsonl");
    let mut forwarded = Vec::new();
    let mut command = Command::new("/definitely/not/a/tool-3389");
    let err = run_small_tool_with_sinks(
        &mut command,
        "tool-3389 -V",
        None,
        ToolSinks {
            stderr: &mut forwarded,
            log_path: Some(log.clone()),
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("tool-3389"), "{err}");
    assert!(read_records(&log)[0]["error"].is_string());
}
