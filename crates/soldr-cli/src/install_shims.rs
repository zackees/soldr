//! `soldr shims` — install the per-version shim dir + emit a stable
//! JSON describing where the shims live. See zackees/soldr#742 for the
//! full design including the file-lock + native-binary + recursion-
//! guard reasoning.
//!
//! Layout (per zackees/soldr#743 / PR #744):
//!
//! ```text
//! ~/.soldr/v<MANAGED_SHIM_VERSION>/shims/
//!     cargo{,.exe}            ← hardlinks/copies of soldr under each tool name
//!     rustc{,.exe}
//!     rustfmt{,.exe}
//!     clippy-driver{,.exe}
//!     rustdoc{,.exe}
//! ```
//!
//! `soldr shims --json` ensures the dir is populated and prints the
//! JSON consumed by [`clud`](https://github.com/zackees/clud/issues/343)
//! and other downstream callers.
//!
//! v1 scope: hardlink-first materialization with copy fallback, no daemon
//! RPC, no cross-process file lock. Concurrent first-runs are safe via
//! per-pid tmp suffix + atomic rename + content idempotency (every writer
//! produces identical content; last rename wins benignly). Daemon RPC +
//! in-memory mutex is tracked as the next iteration in #742.

use crate::core::{SoldrError, SoldrPaths};
use crate::fetch::MANAGED_SHIM_VERSION;
use serde::Serialize;
use std::path::Path;
use std::time::Instant;

const TOOLS: &[&str] = &["cargo", "rustc", "rustfmt", "clippy-driver", "rustdoc"];
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Serialize)]
pub struct ShimsOutput {
    pub schema_version: u32,
    pub shim_dir: String,
    pub shim_kind: &'static str,
    pub link_mode: &'static str,
    pub soldr_shim_source: String,
    pub soldr_version: &'static str,
    pub tools: Vec<ToolEntry>,
    pub path_entry: String,
    pub elapsed_ms: u128,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ToolEntry {
    pub name: String,
    pub shim_path: String,
    pub created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<&'static str>,
}

/// Skip codes for the JSON `skip_reason` field. Stable strings; consumers
/// (clud, setup-soldr) parse these.
pub const SKIP_EXISTING_MATCHES: &str = "existing-matches";

/// Top-level entry point invoked from `main.rs` dispatch for the
/// `Commands::Shims { json }` clap variant.
pub fn run_shims(paths: &SoldrPaths, json: bool) -> Result<i32, SoldrError> {
    let started = Instant::now();
    paths.ensure_dirs()?;

    let shim_source = crate::shim_materialize::soldr_binary_source()?;
    let shim_dir = paths.versioned_shims_dir();
    std::fs::create_dir_all(&shim_dir).map_err(SoldrError::Io)?;

    let mut tools_out = Vec::with_capacity(TOOLS.len());
    for tool in TOOLS {
        let target = shim_dir.join(tool_file_name(tool));
        let entry = install_one(&target, &shim_source, tool)?;
        tools_out.push(entry);
    }
    sweep_orphans(&shim_dir);

    let output = ShimsOutput {
        schema_version: SCHEMA_VERSION,
        shim_dir: shim_dir.display().to_string(),
        shim_kind: "multicall-soldr",
        link_mode: crate::shim_materialize::LINK_MODE_HARDLINK_OR_COPY,
        soldr_shim_source: shim_source.display().to_string(),
        soldr_version: MANAGED_SHIM_VERSION,
        tools: tools_out,
        path_entry: shim_dir.display().to_string(),
        elapsed_ms: started.elapsed().as_millis(),
    };

    if json {
        emit_json(&output)?;
    } else {
        emit_human(&output);
    }
    Ok(0)
}

/// Per-OS shim file name for `tool` (appends `.exe` on Windows).
pub(crate) fn tool_file_name(tool: &str) -> String {
    crate::platform::executable::name::native(tool)
}

/// Install (or re-install if stale) a single shim file. Atomic and
/// idempotent via [`crate::shim_materialize::materialize_executable`].
fn install_one(target: &Path, source: &Path, tool: &str) -> Result<ToolEntry, SoldrError> {
    // soldr#1856: same guard as the transient shim dir. Without it,
    // `soldr install-shims` from a pip-installed soldr writes a broken
    // `~/.soldr/bin/cargo` whose `@loader_path/../<pkg>.dylibs` reference no
    // longer resolves. Idempotency is preserved by comparing against the
    // trampoline text rather than the Mach-O bytes.
    if soldr_core::self_relocate::exe_depends_on_bundled_wheel_libs(source) {
        let body = crate::shim_dir::trampoline_shim_body(source);
        let created = match std::fs::read_to_string(target) {
            Ok(existing) if existing == body => false,
            _ => {
                crate::shim_dir::write_trampoline_shim(target, source)?;
                true
            }
        };
        return Ok(ToolEntry {
            name: tool.to_string(),
            shim_path: target.display().to_string(),
            created,
            skip_reason: if created {
                None
            } else {
                Some(SKIP_EXISTING_MATCHES)
            },
        });
    }
    let result = crate::shim_materialize::materialize_executable(source, target)?;

    Ok(ToolEntry {
        name: tool.to_string(),
        shim_path: target.display().to_string(),
        created: result.created,
        skip_reason: if result.created {
            None
        } else {
            Some(SKIP_EXISTING_MATCHES)
        },
    })
}

/// Best-effort sweep of leftover `*.tmp.*` files inside the shim dir.
/// Silently ignores any IO errors — orphans are cosmetic.
fn sweep_orphans(shim_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(shim_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if let Some(_idx) = s.find(".tmp.") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn emit_json(output: &ShimsOutput) -> Result<(), SoldrError> {
    let payload = serde_json::to_string_pretty(output)
        .map_err(|e| SoldrError::Other(format!("shims: failed to serialize JSON: {e}")))?;
    println!("{payload}");
    Ok(())
}

fn emit_human(output: &ShimsOutput) {
    println!(
        "soldr shims: shim dir {} (soldr v{})",
        output.shim_dir, output.soldr_version
    );
    let mut created = 0usize;
    let mut matched = 0usize;
    for entry in &output.tools {
        if entry.created {
            created += 1;
            println!("  wrote   {} -> {}", entry.name, entry.shim_path);
        } else {
            matched += 1;
            println!("  skip    {} (existing matches)", entry.name);
        }
    }
    println!(
        "soldr shims: {created} written, {matched} already up-to-date — \
         prepend `{}` to PATH to route Rust toolchain calls through soldr",
        output.path_entry
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn fake_shim_source(tmp: &TempDir) -> PathBuf {
        let bin = tmp.path().join("soldr-fake");
        std::fs::write(&bin, b"FAKE-SOLDR-BYTES-v1").unwrap();
        bin
    }

    crate::timed_test!(install_one_creates_file_when_missing, {
        let tmp = TempDir::new().unwrap();
        let source = fake_shim_source(&tmp);
        let target = tmp.path().join("cargo");
        let entry = install_one(&target, &source, "cargo").unwrap();
        assert!(entry.created, "should create on first install");
        assert!(entry.skip_reason.is_none());
        assert!(target.is_file(), "target file should exist after install");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            std::fs::read(&source).unwrap(),
            "target content must match soldr source byte-for-byte"
        );
    });

    crate::timed_test!(install_one_is_idempotent, {
        let tmp = TempDir::new().unwrap();
        let source = fake_shim_source(&tmp);
        let target = tmp.path().join("rustc");

        let first = install_one(&target, &source, "rustc").unwrap();
        assert!(first.created);

        let second = install_one(&target, &source, "rustc").unwrap();
        assert!(!second.created, "second run must not re-create");
        assert_eq!(second.skip_reason, Some(SKIP_EXISTING_MATCHES));
    });

    crate::timed_test!(install_one_replaces_when_source_changes, {
        let tmp = TempDir::new().unwrap();
        let source = fake_shim_source(&tmp);
        let target = tmp.path().join("rustfmt");
        install_one(&target, &source, "rustfmt").unwrap();

        // Mutate source — simulates a soldr binary upgrade.
        std::fs::remove_file(&source).unwrap();
        std::fs::write(&source, b"FAKE-SOLDR-BYTES-v2").unwrap();
        let replaced = install_one(&target, &source, "rustfmt").unwrap();
        assert!(replaced.created, "must replace when source bytes change");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"FAKE-SOLDR-BYTES-v2",
            "target should reflect new source bytes"
        );
    });

    crate::timed_test!(tool_file_name_appends_exe_on_windows_only, {
        let cargo = tool_file_name("cargo");
        if cfg!(windows) {
            assert_eq!(cargo, "cargo.exe");
        } else {
            assert_eq!(cargo, "cargo");
        }
    });

    crate::timed_test!(sweep_orphans_removes_only_tmp_files, {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("cargo"), b"keep").unwrap();
        std::fs::write(tmp.path().join("cargo.tmp.999-1"), b"sweep").unwrap();
        std::fs::write(tmp.path().join("rustc.tmp.999-2"), b"sweep").unwrap();
        sweep_orphans(tmp.path());
        assert!(
            tmp.path().join("cargo").is_file(),
            "non-tmp file must remain"
        );
        assert!(
            !tmp.path().join("cargo.tmp.999-1").exists(),
            "tmp file must be swept"
        );
        assert!(
            !tmp.path().join("rustc.tmp.999-2").exists(),
            "tmp file must be swept"
        );
    });

    crate::timed_test!(json_output_carries_versioned_path_entry_and_schema, {
        let entries = vec![ToolEntry {
            name: "cargo".to_string(),
            shim_path: "/.soldr/v0.7.55/shims/cargo".to_string(),
            created: true,
            skip_reason: None,
        }];
        let out = ShimsOutput {
            schema_version: SCHEMA_VERSION,
            shim_dir: "/.soldr/v0.7.55/shims".to_string(),
            shim_kind: "multicall-soldr",
            link_mode: crate::shim_materialize::LINK_MODE_HARDLINK_OR_COPY,
            soldr_shim_source: "/opt/soldr".to_string(),
            soldr_version: MANAGED_SHIM_VERSION,
            tools: entries,
            path_entry: "/.soldr/v0.7.55/shims".to_string(),
            elapsed_ms: 4,
        };
        let json = serde_json::to_string(&out).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["path_entry"], "/.soldr/v0.7.55/shims");
        assert_eq!(parsed["shim_kind"], "multicall-soldr");
        assert_eq!(parsed["link_mode"], "hardlink-or-copy");
        assert_eq!(parsed["tools"][0]["name"], "cargo");
        assert!(
            parsed["tools"][0].get("skip_reason").is_none(),
            "skip_reason must be omitted when None"
        );
    });
}
