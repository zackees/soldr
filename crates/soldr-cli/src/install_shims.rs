//! `soldr shims` — install the per-version shim dir + emit a stable
//! JSON describing where the shims live. See zackees/soldr#742 for the
//! full design including the file-lock + native-binary + recursion-
//! guard reasoning.
//!
//! Layout (per zackees/soldr#743 / PR #744):
//!
//! ```text
//! ~/.soldr/v<MANAGED_SHIM_VERSION>/shims/
//!     cargo{,.exe}            ← copies of soldr-shim under each tool name
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
//! v1 scope: copy-only materialization (no symlink / hardlink
//! optimization), no daemon RPC, no cross-process file lock. Concurrent
//! first-runs are safe via per-pid tmp suffix + atomic rename + content
//! idempotency (every writer produces identical content; last rename
//! wins benignly). Daemon RPC + in-memory mutex is tracked as the next
//! iteration in #742.

use crate::core::{SoldrError, SoldrPaths};
use crate::fetch::MANAGED_SHIM_VERSION;
use serde::Serialize;
use std::path::{Path, PathBuf};
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

    let shim_source = locate_soldr_shim_source()?;
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
        shim_kind: "native-binary",
        link_mode: "copy",
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
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

/// Resolve the path to the `soldr-shim` binary sibling of the running
/// soldr. Returns an error with a clear message when missing — the
/// shim is built as a `[[bin]]` in the same crate as soldr, so a normal
/// release install should always ship both.
fn locate_soldr_shim_source() -> Result<PathBuf, SoldrError> {
    let soldr_path = crate::current_soldr_binary()?;
    let parent = soldr_path.parent().ok_or_else(|| {
        SoldrError::Other(format!(
            "soldr binary path has no parent dir: {}",
            soldr_path.display()
        ))
    })?;
    let exe_name = if cfg!(windows) {
        "soldr-shim.exe"
    } else {
        "soldr-shim"
    };
    let candidate = parent.join(exe_name);
    if !candidate.is_file() {
        return Err(SoldrError::Other(format!(
            "soldr-shim binary not found at {}. \
             Rebuild soldr with the matching workspace so both \
             `soldr` and `soldr-shim` are produced.",
            candidate.display()
        )));
    }
    Ok(candidate)
}

/// Install (or re-install if stale) a single shim file. Atomic via
/// per-pid tmp + rename. Idempotent via blake3 content match.
fn install_one(target: &Path, source: &Path, tool: &str) -> Result<ToolEntry, SoldrError> {
    if is_current(target, source)? {
        return Ok(ToolEntry {
            name: tool.to_string(),
            shim_path: target.display().to_string(),
            created: false,
            skip_reason: Some(SKIP_EXISTING_MATCHES),
        });
    }

    let tmp = tmp_path_for(target);
    std::fs::copy(source, &tmp).map_err(SoldrError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)
            .map_err(SoldrError::Io)?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms).map_err(SoldrError::Io)?;
    }
    // Atomic rename — on POSIX same-fs, on Windows same-volume.
    std::fs::rename(&tmp, target).map_err(|err| {
        // Best-effort cleanup of the tmp on rename failure.
        let _ = std::fs::remove_file(&tmp);
        SoldrError::Io(err)
    })?;

    Ok(ToolEntry {
        name: tool.to_string(),
        shim_path: target.display().to_string(),
        created: true,
        skip_reason: None,
    })
}

/// `<target>.tmp.<pid>-<random>` — per-pid suffix prevents racing tmps
/// across concurrent invocations.
fn tmp_path_for(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let suffix = format!("tmp.{pid}-{nanos}");
    let mut s = target.as_os_str().to_os_string();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

/// `target` is "current" when it exists, has the same byte length as
/// `source`, AND has the same blake3 content hash.
fn is_current(target: &Path, source: &Path) -> Result<bool, SoldrError> {
    let Ok(target_meta) = std::fs::metadata(target) else {
        return Ok(false);
    };
    let Ok(source_meta) = std::fs::metadata(source) else {
        return Ok(false);
    };
    if target_meta.len() != source_meta.len() {
        return Ok(false);
    }
    let target_hash = blake3_file(target)?;
    let source_hash = blake3_file(source)?;
    Ok(target_hash == source_hash)
}

fn blake3_file(path: &Path) -> Result<[u8; 32], SoldrError> {
    zccache::hash::hash_file(path)
        .map(|hash| *hash.as_bytes())
        .map_err(SoldrError::Io)
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
    use tempfile::TempDir;

    fn fake_shim_source(tmp: &TempDir) -> PathBuf {
        let bin = tmp.path().join("soldr-shim-fake");
        std::fs::write(&bin, b"FAKE-SOLDR-SHIM-BYTES-v1").unwrap();
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
            "target content must match source byte-for-byte"
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

        // Mutate source — simulates a soldr-shim binary upgrade.
        std::fs::write(&source, b"FAKE-SOLDR-SHIM-BYTES-v2").unwrap();
        let replaced = install_one(&target, &source, "rustfmt").unwrap();
        assert!(replaced.created, "must replace when source bytes change");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"FAKE-SOLDR-SHIM-BYTES-v2",
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

    crate::timed_test!(tmp_path_includes_pid_and_random_suffix, {
        let target = Path::new("/tmp/cargo");
        let a = tmp_path_for(target);
        let b = tmp_path_for(target);
        // Different invocations should differ (subsec_nanos jitter).
        // Worst case identical nanos → both writers race on same tmp,
        // both produce identical content, so the race is still safe.
        let a_str = a.to_string_lossy();
        let b_str = b.to_string_lossy();
        assert!(
            a_str.contains(".tmp."),
            "tmp path must contain .tmp. delimiter: {a_str}"
        );
        assert!(
            b_str.contains(".tmp."),
            "tmp path must contain .tmp. delimiter: {b_str}"
        );
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

    crate::timed_test!(is_current_returns_false_when_target_missing, {
        let tmp = TempDir::new().unwrap();
        let source = fake_shim_source(&tmp);
        let target = tmp.path().join("does-not-exist");
        assert!(!is_current(&target, &source).unwrap());
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
            shim_kind: "native-binary",
            link_mode: "copy",
            soldr_shim_source: "/opt/soldr-shim".to_string(),
            soldr_version: MANAGED_SHIM_VERSION,
            tools: entries,
            path_entry: "/.soldr/v0.7.55/shims".to_string(),
            elapsed_ms: 4,
        };
        let json = serde_json::to_string(&out).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["path_entry"], "/.soldr/v0.7.55/shims");
        assert_eq!(parsed["shim_kind"], "native-binary");
        assert_eq!(parsed["link_mode"], "copy");
        assert_eq!(parsed["tools"][0]["name"], "cargo");
        assert!(
            parsed["tools"][0].get("skip_reason").is_none(),
            "skip_reason must be omitted when None"
        );
    });
}
