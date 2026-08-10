//! Output formatting for `rpprobe` (S14 / #643).
//!
//! Two shapes for every command: an aligned table for a person, and JSON for
//! a script. The JSON is emitted from the same values the table is built from,
//! so `--json` cannot quietly disagree with what the operator saw.
//!
//! Nothing here decides what may be shown. The daemon already removed
//! everything a caller is not entitled to; a formatter that filtered would be
//! a second, weaker copy of that rule.

use running_process_probe::probe_diag::v1 as wire;
use serde_json::json;

use crate::cli::commands::CaptureStatus;

/// Render an aligned table.
///
/// Column widths come from the content, because a fixed width either truncates
/// a real path or wastes half the terminal — and the paths this tool prints are
/// exactly the values an operator wants to copy intact.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(cell.len());
            }
        }
    }

    let mut out = String::new();
    for (index, header) in headers.iter().enumerate() {
        push_cell(&mut out, header, widths[index], index + 1 == headers.len());
    }
    out.push('\n');
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            push_cell(&mut out, cell, widths[index], index + 1 == row.len());
        }
        out.push('\n');
    }
    out
}

fn push_cell(out: &mut String, cell: &str, width: usize, last: bool) {
    out.push_str(cell);
    if !last {
        // Trailing whitespace on the final column would be invisible padding
        // that a copy-paste picks up.
        for _ in cell.len()..width + 2 {
            out.push(' ');
        }
    }
}

/// Render the process list.
pub fn processes(processes: &[wire::ProcessInfo], as_json: bool) -> String {
    if as_json {
        let rows: Vec<_> = processes
            .iter()
            .map(|p| {
                json!({
                    "pid": p.key.as_ref().map(|k| k.pid).unwrap_or(0),
                    "name": p.name,
                    "app_class": p.app_class,
                    "cwd": p.cwd,
                    "exe": p.exe_path,
                    "registered": p.registered,
                    "env": p.env,
                    "env_names": p.env_names,
                })
            })
            .collect();
        return format!("{}\n", json!(rows));
    }

    if processes.is_empty() {
        return "no processes match\n".to_string();
    }

    let rows: Vec<Vec<String>> = processes
        .iter()
        .map(|p| {
            let env = if p.env.is_empty() {
                p.env_names.join(",")
            } else {
                let mut pairs: Vec<String> =
                    p.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
                pairs.sort();
                pairs.join(" ")
            };
            vec![
                p.key.as_ref().map(|k| k.pid).unwrap_or(0).to_string(),
                p.name.clone(),
                p.app_class.clone(),
                if p.registered {
                    "yes".into()
                } else {
                    "no".into()
                },
                p.cwd.clone(),
                env,
            ]
        })
        .collect();
    table(&["PID", "NAME", "CLASS", "REG", "CWD", "ENV"], &rows)
}

/// Render crash records.
pub fn crashes(records: &[wire::CrashRecord], as_json: bool) -> String {
    if as_json {
        let rows: Vec<_> = records
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "app_class": r.app_class,
                    "instance": r.instance_name,
                    "pid": r.key.as_ref().map(|k| k.pid).unwrap_or(0),
                    "signature": r.signature,
                    "fault_kind": r.fault_kind,
                    "crashed_at_ms": r.crash_unix_ms,
                    "artifact_bytes": r.artifact_bytes,
                })
            })
            .collect();
        return format!("{}\n", json!(rows));
    }

    if records.is_empty() {
        return "no crashes match\n".to_string();
    }

    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|r| {
            vec![
                r.id.to_string(),
                millis(r.crash_unix_ms),
                r.app_class.clone(),
                r.signature.clone(),
                r.fault_kind.clone(),
                bytes(r.artifact_bytes),
            ]
        })
        .collect();
    table(
        &["ID", "WHEN", "CLASS", "SIGNATURE", "FAULT", "SIZE"],
        &rows,
    )
}

/// Render the crash rollup.
pub fn crash_stats(stats: &wire::CrashStatsReply, as_json: bool) -> String {
    if as_json {
        let signatures: Vec<_> = stats
            .signatures
            .iter()
            .map(|s| {
                json!({
                    "signature": s.signature,
                    "count": s.count,
                    "first_unix_ms": s.first_unix_ms,
                    "last_unix_ms": s.last_unix_ms,
                    "app_classes": s.app_classes,
                })
            })
            .collect();
        return format!(
            "{}\n",
            json!({
                "total": stats.total,
                "distinct_classes": stats.distinct_classes,
                "first_unix_ms": stats.first_unix_ms,
                "last_unix_ms": stats.last_unix_ms,
                "signatures": signatures,
            })
        );
    }

    let mut out = format!(
        "{} crash(es) across {} class(es)",
        stats.total, stats.distinct_classes
    );
    if stats.total > 0 {
        out.push_str(&format!(
            ", {} → {}",
            millis(stats.first_unix_ms),
            millis(stats.last_unix_ms)
        ));
    }
    out.push_str("\n\n");

    if stats.signatures.is_empty() {
        return out;
    }
    let rows: Vec<Vec<String>> = stats
        .signatures
        .iter()
        .map(|s| {
            vec![
                s.count.to_string(),
                s.signature.clone(),
                millis(s.first_unix_ms),
                millis(s.last_unix_ms),
                s.app_classes.join(","),
            ]
        })
        .collect();
    out.push_str(&table(
        &["COUNT", "SIGNATURE", "FIRST", "LAST", "CLASSES"],
        &rows,
    ));
    out
}

/// Render capture outcomes.
pub fn capture(statuses: &[CaptureStatus], as_json: bool) -> String {
    if as_json {
        let rows: Vec<_> = statuses
            .iter()
            .map(|s| json!({"pid": s.pid, "job_id": s.job_id, "detail": s.detail}))
            .collect();
        return format!("{}\n", json!(rows));
    }

    let rows: Vec<Vec<String>> = statuses
        .iter()
        .map(|s| {
            vec![
                s.pid.to_string(),
                if s.job_id.is_empty() {
                    "(inline)".to_string()
                } else {
                    s.job_id.clone()
                },
                s.detail.clone(),
            ]
        })
        .collect();
    table(&["PID", "JOB", "DETAIL"], &rows)
}

/// Render the doctor report.
pub fn doctor(checks: &[(String, bool, String)], as_json: bool) -> String {
    if as_json {
        let rows: Vec<_> = checks
            .iter()
            .map(|(name, ok, detail)| json!({"check": name, "ok": ok, "detail": detail}))
            .collect();
        return format!("{}\n", json!(rows));
    }

    let rows: Vec<Vec<String>> = checks
        .iter()
        .map(|(name, ok, detail)| {
            vec![
                if *ok { "ok".into() } else { "FAIL".into() },
                name.clone(),
                detail.clone(),
            ]
        })
        .collect();
    table(&["", "CHECK", "DETAIL"], &rows)
}

/// Format unix milliseconds as an ISO-8601-ish UTC timestamp.
///
/// Hand-rolled from the civil-calendar algorithm rather than pulling in a date
/// crate: the CLI needs exactly one format, and a dependency whose changelog
/// has to be watched is a poor trade for twenty lines that cannot drift.
pub fn millis(unix_ms: u64) -> String {
    if unix_ms == 0 {
        return "-".to_string();
    }
    let secs = (unix_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);

    // Howard Hinnant's `civil_from_days`, shifted to a March-based year so the
    // leap day lands at the end and needs no special case.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60
    )
}

/// Format a byte count with a binary unit.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
