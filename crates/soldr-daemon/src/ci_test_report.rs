//! Best-effort compiler-unit observations for `soldr ci-test` (#2869).
//!
//! The compiler request arrives here over the existing protobuf SESSION wire.
//! This module only writes a local, explicitly requested JSONL artifact; it
//! never affects Cargo's freshness decision or the compiler result.

use crate::daemon::protocol::CompileRequest;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;

const REPORT_PATH_ENV: &str = "SOLDR_CI_TEST_REPORT_PATH";
const STAGE_ENV: &str = "SOLDR_CI_TEST_STAGE";

#[derive(Serialize)]
struct Event {
    schema_version: u32,
    stage: String,
    cache_outcome: i32,
    identity: Identity,
}

#[derive(Serialize)]
struct Identity {
    digest: String,
    compiler: String,
    crate_name: Option<String>,
    crate_types: Vec<String>,
    target: Option<String>,
    source: Option<String>,
    cfg: Vec<String>,
    codegen: Vec<String>,
    cargo_features: Vec<String>,
    rustflags: Option<String>,
}

/// The request details needed after zccache has consumed the request body.
pub(crate) struct PreparedReport {
    path: PathBuf,
    stage: String,
    identity: Identity,
}

/// Capture a report's semantic identity before the compiler service consumes
/// the request's owned stdin/environment payload.
pub(crate) fn prepare(request: &CompileRequest) -> Option<PreparedReport> {
    let env = |name: &str| {
        request
            .env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    };
    let path = env(REPORT_PATH_ENV)?;
    Some(PreparedReport {
        path: PathBuf::from(path),
        stage: env(STAGE_ENV).cloned().unwrap_or_else(|| "unknown".into()),
        identity: normalized_identity(request),
    })
}

/// Append one observation. Diagnostics are deliberately suppressed: a report
/// must not turn a passing compiler invocation into a failed build.
pub(crate) fn record(report: PreparedReport, cache_outcome: i32) {
    let event = Event {
        schema_version: 1,
        stage: report.stage,
        cache_outcome,
        identity: report.identity,
    };
    let Ok(line) = serde_json::to_vec(&event) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(report.path)
    else {
        return;
    };
    let _ = file.write_all(&line);
    let _ = file.write_all(b"\n");
}

fn normalized_identity(request: &CompileRequest) -> Identity {
    let args = request.args.get(1..).unwrap_or_default();
    let values = |flag: &str| flag_values(args, flag);
    let compiler = request.args.first().cloned().unwrap_or_default();
    let crate_name = values("--crate-name").into_iter().next();
    let crate_types = values("--crate-type");
    let target = values("--target").into_iter().next();
    let source = args.iter().find(|arg| arg.ends_with(".rs")).cloned();
    let cfg = values("--cfg");
    let codegen = values("-C");
    let mut cargo_features: Vec<String> = request
        .env
        .iter()
        .filter(|(key, _)| key.starts_with("CARGO_FEATURE_"))
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    cargo_features.sort();
    let rustflags = request
        .env
        .iter()
        .find(|(key, _)| key == "CARGO_ENCODED_RUSTFLAGS" || key == "RUSTFLAGS")
        .map(|(_, value)| value.clone());
    let mut canonical = vec![
        compiler.clone(),
        crate_name.clone().unwrap_or_default(),
        crate_types.join(","),
        target.clone().unwrap_or_default(),
        source.clone().unwrap_or_default(),
        cfg.join(","),
        codegen.join(","),
        cargo_features.join(","),
        rustflags.clone().unwrap_or_default(),
    ];
    canonical.push(request.cwd.clone());
    let mut hasher = Sha256::new();
    for field in canonical {
        hasher.update(field.as_bytes());
        hasher.update([0]);
    }
    Identity {
        digest: hex::encode(hasher.finalize()),
        compiler,
        crate_name,
        crate_types,
        target,
        source,
        cfg,
        codegen,
        cargo_features,
        rustflags,
    }
}

fn flag_values(args: &[String], flag: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == flag {
            if let Some(value) = args.get(index + 1) {
                values.push(value.clone());
            }
            index += 2;
        } else if let Some(value) = args[index].strip_prefix(&format!("{flag}=")) {
            values.push(value.to_string());
            index += 1;
        } else {
            index += 1;
        }
    }
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_excludes_output_location_but_keeps_semantic_flags() {
        let request = CompileRequest {
            args: vec![
                "rustc".into(),
                "--crate-name".into(),
                "one".into(),
                "src/lib.rs".into(),
                "--out-dir".into(),
                "target/a".into(),
                "-C".into(),
                "opt-level=2".into(),
            ],
            cwd: "/repo".into(),
            env: vec![],
            stdin: vec![],
            lifecycle: None,
            ipc_busy_retries: 0,
        };
        let mut relocated = request.clone();
        relocated.args[5] = "target/b".into();
        assert_eq!(
            normalized_identity(&request).digest,
            normalized_identity(&relocated).digest
        );
        relocated.args[7] = "opt-level=3".into();
        assert_ne!(
            normalized_identity(&request).digest,
            normalized_identity(&relocated).digest
        );
    }
}
