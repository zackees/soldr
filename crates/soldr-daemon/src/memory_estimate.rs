//! Shadow-mode per-unit peak-memory estimate for compiler admission (soldr#3152).
//!
//! soldr#3152 replaces the admission name lists (`SOLDR_HEAVY_TEST_LINKS`,
//! `SOLDR_RUST_EXCLUSIVE_NON_LINKING_UNITS`) with a measured predicate:
//! estimate a Rust unit's peak memory from its command line, then spend that
//! estimate against live headroom so exclusivity becomes the emergent case for
//! a unit that genuinely needs the machine.
//!
//! This module is step 3 of that plan, *shadow mode*. For every compiler child
//! zccache is about to admit, it computes the estimate and appends one row to
//! `admission-estimate.jsonl`. Admission itself does not read the estimate.
//! zccache 1.13.23 journals each compile's measured `child_peak_rss_bytes`,
//! and [`args_digest`] is the join key between the two files, so the
//! coefficients below can be fitted and under-prediction measured before any
//! semaphore spends them.
//!
//! The coefficients are **uncalibrated placeholders**. They are deliberately
//! monotone in every feature: fitting may change their magnitude, but more
//! link input, fat LTO, or a single codegen unit must never predict *less*.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::core::SoldrPaths;

/// Bumped on any field removal or meaning change so offline readers can refuse
/// rows they cannot parse.
const SCHEMA_VERSION: u32 = 1;

const MIB: u64 = 1024 * 1024;

/// Floor for any compiler child: rustc's own working set before it reads a
/// single input. Placeholder until fitted.
const BASE_BYTES: u64 = 256 * MIB;

/// Weight on the summed bytes of every `--extern` rlib. Placeholder.
const EXTERN_SUM_WEIGHT: u64 = 2;

/// Extra weight on the largest single rlib, which bounds LLVM's working set
/// more tightly than the total. Placeholder.
const EXTERN_MAX_WEIGHT: u64 = 4;

/// The unit's whole-program optimisation mode, read from `-C lto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LtoMode {
    Off,
    /// Chunked across the graph: flatter than fat, heavier than none.
    Thin,
    /// The whole graph in memory at once.
    Fat,
}

/// Everything the estimate reads, all derived from the command line plus
/// `stat` of the `--extern` rlibs (the classifier's request exposes no cwd).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnitFeatures {
    pub(crate) crate_name: Option<String>,
    pub(crate) is_test: bool,
    /// `--emit` asked only for metadata (and dep-info): no codegen, no link.
    pub(crate) emit_metadata_only: bool,
    /// Every `--extern` argument, measurable or not.
    pub(crate) extern_count: usize,
    pub(crate) extern_bytes: u64,
    pub(crate) max_extern_bytes: u64,
    pub(crate) lto: LtoMode,
    pub(crate) codegen_units: Option<u32>,
}

/// [`UnitFeatures`] for `args`, with the rlib size lookup injected so tests do
/// not need hundreds of megabytes of real rlibs. Production passes
/// [`crate::amalgamation::file_len`].
pub(crate) fn features_with(
    args: &[String],
    size_of: impl Fn(&Path) -> Option<u64>,
) -> UnitFeatures {
    let mut extern_count = 0;
    let mut extern_bytes: u64 = 0;
    let mut max_extern_bytes: u64 = 0;
    for value in crate::amalgamation::extern_values(args) {
        extern_count += 1;
        let Some(bytes) = value
            .split_once('=')
            .and_then(|(_, path)| size_of(Path::new(path)))
        else {
            continue;
        };
        extern_bytes = extern_bytes.saturating_add(bytes);
        max_extern_bytes = max_extern_bytes.max(bytes);
    }

    let mut lto = LtoMode::Off;
    let mut codegen_units = None;
    let mut emit_metadata_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = flag_value(arg, "-C", &mut iter) {
            if value == "lto" {
                lto = LtoMode::Fat;
            } else if let Some(mode) = value.strip_prefix("lto=") {
                lto = parse_lto(mode);
            } else if let Some(units) = value.strip_prefix("codegen-units=") {
                codegen_units = units.parse().ok();
            }
        } else if let Some(value) = long_flag_value(arg, "--emit", &mut iter) {
            emit_metadata_only = emits_metadata_only(value);
        }
    }

    UnitFeatures {
        crate_name: crate::amalgamation::rust_crate_name(args).map(str::to_string),
        is_test: args.iter().any(|arg| arg == "--test"),
        emit_metadata_only,
        extern_count,
        extern_bytes,
        max_extern_bytes,
        lto,
        codegen_units,
    }
}

/// `-C value` or `-Cvalue`.
fn flag_value<'a>(
    arg: &'a str,
    flag: &str,
    rest: &mut std::slice::Iter<'a, String>,
) -> Option<&'a str> {
    if arg == flag {
        rest.next().map(String::as_str)
    } else {
        arg.strip_prefix(flag).filter(|value| !value.is_empty())
    }
}

/// `--flag value` or `--flag=value`.
fn long_flag_value<'a>(
    arg: &'a str,
    flag: &str,
    rest: &mut std::slice::Iter<'a, String>,
) -> Option<&'a str> {
    if arg == flag {
        rest.next().map(String::as_str)
    } else {
        arg.strip_prefix(flag)
            .and_then(|value| value.strip_prefix('='))
    }
}

fn parse_lto(mode: &str) -> LtoMode {
    match mode {
        "thin" => LtoMode::Thin,
        "off" | "no" | "n" | "false" => LtoMode::Off,
        // `fat`, `yes`, `y`, `on`, `true`: rustc's spellings of whole-graph LTO.
        _ => LtoMode::Fat,
    }
}

fn emits_metadata_only(kinds: &str) -> bool {
    let mut metadata = false;
    for kind in kinds.split(',') {
        let kind = kind.split_once('=').map_or(kind, |(name, _)| name);
        match kind {
            "metadata" => metadata = true,
            "dep-info" => {}
            _ => return false,
        }
    }
    metadata
}

/// Predicted peak resident bytes for one compiler child. Placeholder model,
/// monotone in every feature (see the module docs).
pub(crate) fn estimate_peak_bytes(features: &UnitFeatures) -> u64 {
    let linear = BASE_BYTES
        .saturating_add(features.extern_bytes.saturating_mul(EXTERN_SUM_WEIGHT))
        .saturating_add(features.max_extern_bytes.saturating_mul(EXTERN_MAX_WEIGHT));
    let lto_percent = match features.lto {
        LtoMode::Off => 100,
        LtoMode::Thin => 150,
        LtoMode::Fat => 300,
    };
    let cgu_percent = if features.codegen_units == Some(1) {
        125
    } else {
        100
    };
    let estimate = percent_of(percent_of(linear, lto_percent), cgu_percent);
    if features.emit_metadata_only {
        // No codegen and no link: the dominant term collapses toward the floor.
        BASE_BYTES.max(estimate / 4)
    } else {
        estimate
    }
}

fn percent_of(value: u64, percent: u64) -> u64 {
    (value / 100).saturating_mul(percent) + (value % 100).saturating_mul(percent) / 100
}

/// Lower-case hex SHA-256 of `args` joined by NUL. The compile journal records
/// the same `args`, so an offline reader computes the same key for each row.
pub(crate) fn args_digest(args: &[String]) -> String {
    let mut hasher = Sha256::new();
    for arg in args {
        hasher.update(arg.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        out.push(char::from(b"0123456789abcdef"[usize::from(byte & 0x0F)]));
    }
    out
}

/// Append-only JSONL of shadow estimates, beside the daemon's other logs.
#[must_use]
pub(crate) fn estimate_log_path(paths: &SoldrPaths) -> PathBuf {
    crate::cache_lib::soldr_daemon_dir(paths)
        .join("logs")
        .join("admission-estimate.jsonl")
}

#[derive(Serialize)]
struct Row<'a> {
    schema_version: u32,
    ts_ms: i64,
    pid: u32,
    args_digest: String,
    crate_name: Option<&'a str>,
    is_test: bool,
    emit_metadata_only: bool,
    extern_count: usize,
    extern_bytes: u64,
    max_extern_bytes: u64,
    lto: LtoMode,
    codegen_units: Option<u32>,
    estimate_bytes: u64,
    /// What admission actually decided, so a row shows where the estimate
    /// and today's name-list classifier disagree.
    exclusive: bool,
}

/// Append one row. Best-effort: a diagnostic must never fail a compile, so
/// every error (unwritable directory, full disk, serialization) is dropped.
pub(crate) fn record(
    log: &Path,
    args: &[String],
    features: &UnitFeatures,
    estimate_bytes: u64,
    exclusive: bool,
) {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);
    let Ok(line) = serde_json::to_string(&Row {
        schema_version: SCHEMA_VERSION,
        ts_ms,
        pid: std::process::id(),
        args_digest: args_digest(args),
        crate_name: features.crate_name.as_deref(),
        is_test: features.is_test,
        emit_metadata_only: features.emit_metadata_only,
        extern_count: features.extern_count,
        extern_bytes: features.extern_bytes,
        max_extern_bytes: features.max_extern_bytes,
        lto: features.lto,
        codegen_units: features.codegen_units,
        estimate_bytes,
        exclusive,
    }) else {
        return;
    };
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(file, "{line}");
    }
}

/// Shadow-mode hook: estimate and log a Rust compiler child without changing
/// its admission. Non-Rust compilers are skipped; their shape is judged by
/// `amalgamation::detect` today.
pub(crate) fn shadow(log: &Path, is_rustc: bool, args: &[String], exclusive: bool) {
    if !is_rustc {
        return;
    }
    let features = features_with(args, crate::amalgamation::file_len);
    record(
        log,
        args,
        &features,
        estimate_peak_bytes(&features),
        exclusive,
    );
}

#[cfg(test)]
#[path = "memory_estimate_tests.rs"]
mod tests;
