//! `soldr install-zccache` — pinned zccache install workflow, including
//! `--remove` and `--status` modes.

use crate::core::{SoldrError, SoldrPaths};
use crate::JSON_SCHEMA_VERSION;
use serde::Serialize;

use super::print_json;

#[derive(Serialize)]
struct InstallZccacheOutput {
    schema_version: u32,
    command: &'static str,
    install_dir: String,
    source_kind: String,
    source_value: String,
    version: String,
    binaries: std::collections::BTreeMap<String, crate::fetch::PinnedBinaryRecord>,
    installed_at: String,
    soldr_version: String,
}

#[derive(Serialize)]
struct InstallZccacheRemoveOutput {
    schema_version: u32,
    command: &'static str,
    removed: bool,
    path: String,
}

/// `--status` payload when the pinned install exists.
#[derive(Serialize)]
struct InstallZccacheStatusOutput {
    schema_version: u32,
    command: &'static str,
    pinned: serde_json::Value,
    drift_from_managed: bool,
    managed_version: &'static str,
}

/// `--status` payload when no pinned install exists.
#[derive(Serialize)]
struct InstallZccacheStatusMissingOutput {
    schema_version: u32,
    command: &'static str,
    pinned: Option<()>,
    managed_version: &'static str,
}

pub(crate) async fn run_install_zccache(
    source: Option<String>,
    remove: bool,
    status: bool,
    json: bool,
) -> Result<(), SoldrError> {
    // Mutual-exclusion: clap already rejects multiple flags being set
    // simultaneously, but it lets all three be omitted; reject that case
    // explicitly so the user gets a clear, actionable error.
    let chosen = [source.is_some(), remove, status]
        .iter()
        .filter(|x| **x)
        .count();
    if chosen == 0 {
        return Err(SoldrError::Other(
            "install-zccache: provide a SOURCE (`system`, a path, or a URL), or pass --remove / --status".into(),
        ));
    }
    if remove {
        return run_install_zccache_remove(json);
    }
    if status {
        return run_install_zccache_status(json);
    }
    let raw = source.expect("source presence checked above");
    let parsed = crate::fetch::InstallSource::parse(&raw)?;
    let paths = SoldrPaths::new()?;
    let report = crate::fetch::install_zccache_from_source(&parsed, &paths).await?;
    emit_install_report(&report, json)
}

fn emit_install_report(report: &crate::fetch::InstallReport, json: bool) -> Result<(), SoldrError> {
    if json {
        let output = InstallZccacheOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "install-zccache",
            install_dir: report.install_dir.clone(),
            source_kind: report.source_kind.clone(),
            source_value: report.source_value.clone(),
            version: report.version.clone(),
            binaries: report.binaries.clone(),
            installed_at: report.installed_at.clone(),
            soldr_version: report.soldr_version.clone(),
        };
        print_json(&output)?;
    } else {
        println!("soldr install-zccache");
        println!("  install dir:  {}", report.install_dir);
        println!(
            "  source:       {} ({})",
            report.source_kind, report.source_value
        );
        println!("  version:      {}", report.version);
        println!("  installed at: {}", report.installed_at);
        println!("  binaries:");
        for (name, record) in &report.binaries {
            let short = sha256_short_str(&record.sha256);
            println!(
                "    {name:<14}  sha256={short}  size={} bytes",
                record.size_bytes
            );
        }
        if report.version != "unknown" && report.version != crate::fetch::MANAGED_ZCCACHE_VERSION {
            println!(
                "  note:         pinned version {} differs from managed default {}",
                report.version,
                crate::fetch::MANAGED_ZCCACHE_VERSION
            );
        }
    }
    Ok(())
}

fn run_install_zccache_remove(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let dir = crate::fetch::pinned_zccache_dir(&paths);
    let removed = crate::fetch::remove_pinned_zccache(&paths)?;
    if json {
        let output = InstallZccacheRemoveOutput {
            schema_version: JSON_SCHEMA_VERSION,
            command: "install-zccache --remove",
            removed,
            path: dir.display().to_string(),
        };
        print_json(&output)?;
    } else if removed {
        println!("soldr install-zccache --remove: removed {}", dir.display());
    } else {
        println!(
            "soldr install-zccache --remove: no pinned install at {} (nothing to do)",
            dir.display()
        );
    }
    Ok(())
}

fn run_install_zccache_status(json: bool) -> Result<(), SoldrError> {
    let paths = SoldrPaths::new()?;
    let sidecar = crate::fetch::read_pinned_sidecar(&paths)?;
    let pinned_runtime = crate::fetch::ZccacheResolver::new(&paths)?.inspect_pinned()?;
    match sidecar {
        Some(sidecar) => {
            let drift = crate::fetch::pinned_version_drift_from_managed(&sidecar);
            let install_dir = pinned_runtime
                .as_ref()
                .map(|runtime| runtime.runtime_dir.display().to_string())
                .unwrap_or_else(|| {
                    crate::fetch::pinned_zccache_dir(&paths)
                        .display()
                        .to_string()
                });
            if json {
                let pinned_value = serde_json::to_value(&sidecar).map_err(|e| {
                    SoldrError::Other(format!("install-zccache status: serialize sidecar: {e}"))
                })?;
                let output = InstallZccacheStatusOutput {
                    schema_version: JSON_SCHEMA_VERSION,
                    command: "install-zccache --status",
                    pinned: pinned_value,
                    drift_from_managed: drift,
                    managed_version: crate::fetch::MANAGED_ZCCACHE_VERSION,
                };
                print_json(&output)?;
            } else {
                println!("soldr install-zccache --status");
                println!("  install dir:  {install_dir}");
                println!(
                    "  source:       {} ({})",
                    sidecar.source_kind, sidecar.source_value
                );
                println!("  version:      {}", sidecar.version);
                println!(
                    "  installed at: {}{}",
                    sidecar.installed_at,
                    install_age_note(&sidecar.installed_at)
                );
                println!("  soldr version: {}", sidecar.soldr_version);
                println!("  binaries:");
                for (name, record) in &sidecar.binaries {
                    let short = sha256_short_str(&record.sha256);
                    println!(
                        "    {name:<14}  sha256={short}  size={} bytes",
                        record.size_bytes
                    );
                }
                if drift {
                    println!(
                        "  warning:      pinned version {} is different from soldr's managed default {} — consider `soldr install-zccache --remove` to use the managed version",
                        sidecar.version,
                        crate::fetch::MANAGED_ZCCACHE_VERSION
                    );
                }
            }
        }
        None => {
            if json {
                let output = InstallZccacheStatusMissingOutput {
                    schema_version: JSON_SCHEMA_VERSION,
                    command: "install-zccache --status",
                    pinned: None,
                    managed_version: crate::fetch::MANAGED_ZCCACHE_VERSION,
                };
                print_json(&output)?;
            } else {
                println!(
                    "no pinned zccache installed; using managed default {}",
                    crate::fetch::MANAGED_ZCCACHE_VERSION
                );
            }
        }
    }
    Ok(())
}

fn sha256_short_str(full: &str) -> String {
    full.chars().take(12).collect()
}

/// Best-effort "installed N days ago" hint from an RFC3339 UTC string.
/// Parses just enough of the timestamp to compute a delta in days;
/// returns an empty string on parse failure so the human output never
/// breaks.
fn install_age_note(installed_at: &str) -> String {
    let Some(parsed_secs) = parse_iso8601_utc_seconds(installed_at) else {
        return String::new();
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = now_secs - parsed_secs;
    if delta < 60 {
        " (just now)".to_string()
    } else if delta < 3600 {
        format!(
            " ({} minute{} ago)",
            delta / 60,
            if delta / 60 == 1 { "" } else { "s" }
        )
    } else if delta < 86_400 {
        format!(
            " ({} hour{} ago)",
            delta / 3600,
            if delta / 3600 == 1 { "" } else { "s" }
        )
    } else {
        format!(
            " ({} day{} ago)",
            delta / 86_400,
            if delta / 86_400 == 1 { "" } else { "s" }
        )
    }
}

fn parse_iso8601_utc_seconds(s: &str) -> Option<i64> {
    // Minimal "YYYY-MM-DDTHH:MM:SSZ" parser matching what
    // soldr-fetch::install_zccache::format_iso8601_utc emits. Returns
    // None on anything unexpected — the caller treats that as "no age
    // hint".
    let s = s.trim();
    if s.len() < 20 {
        return None;
    }
    let year: i32 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    Some(ymdhms_to_unix_seconds(
        year, month, day, hour, minute, second,
    ))
}

fn ymdhms_to_unix_seconds(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> i64 {
    // Inverse of unix_seconds_to_ymdhms in install_zccache; civil_to_days
    // by Howard Hinnant (public domain).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as i64;
    let m_i = m as i64;
    let d_i = d as i64;
    let doy = (153 * (if m_i > 2 { m_i - 3 } else { m_i + 9 }) + 2) / 5 + d_i - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as i64 * 146_097 + doe - 719_468;
    days * 86_400 + (h as i64) * 3600 + (min as i64) * 60 + (s as i64)
}

#[cfg(test)]
mod install_zccache_tests {
    use super::*;

    #[test]
    fn sha256_short_truncates_to_12() {
        let s = sha256_short_str("0123456789abcdef0123456789");
        assert_eq!(s, "0123456789ab");
        assert_eq!(s.len(), 12);
    }

    #[test]
    fn parse_iso8601_handles_known_timestamps() {
        // Sanity-check the parser against hand-computed values.
        assert_eq!(parse_iso8601_utc_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601_utc_seconds("2023-11-14T22:13:20Z"),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn parse_iso8601_rejects_short_strings() {
        assert_eq!(parse_iso8601_utc_seconds(""), None);
        assert_eq!(parse_iso8601_utc_seconds("not-a-date"), None);
        assert_eq!(parse_iso8601_utc_seconds("2026-05-2"), None);
    }
}
