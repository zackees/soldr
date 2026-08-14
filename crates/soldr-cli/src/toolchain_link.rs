//! `soldr toolchain link --shim-dir <path>` — write PATH shim executables
//! that re-route `cargo` / `rustfmt` / `clippy-driver` / `rustc` /
//! `rustdoc` back through `soldr <tool>`. Port of setup-soldr's
//! `ensure-shims.ts` into the soldr binary (issue #407 Phase 3).
//!
//! The shims are native multicall hardlinks/copies of the running soldr
//! binary, matching `crate::shim_dir` and `soldr shims`. `toolchain link`
//! writes them to a caller-chosen directory and supports idempotent re-runs
//! plus `--force` overwrites. Setup-soldr#133 will delegate its TS shim
//! installer to this verb on the soldr-managed side.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::core::SoldrError;

const SCHEMA_VERSION: u32 = 1;

/// Tools we write shims for. Mirrors setup-soldr's `SHIMMED_TOOLS`
/// constant in `src/lib/shim-bypass-check.ts` and the
/// `crate::shim_dir::SHIMMED_TOOLS` list used by transient child-process
/// shims. Order is the stable JSON output order.
pub(crate) const LINK_SHIMMED_TOOLS: &[&str] =
    &["cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"];

/// CLI-level args for `toolchain link`, mirroring the
/// `cli_args::ToolchainSubcommand::Link` variant.
#[derive(Debug, Clone)]
pub(crate) struct LinkArgs {
    pub shim_dir: PathBuf,
    pub json: bool,
    pub force: bool,
}

#[derive(Serialize, Debug)]
pub(crate) struct ToolchainLinkOutput {
    pub schema_version: u32,
    pub shim_dir: String,
    pub tools: Vec<ToolEntry>,
    pub elapsed_ms: u128,
}

#[derive(Serialize, Debug)]
pub(crate) struct ToolEntry {
    pub name: String,
    pub shim_path: String,
    pub created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

/// `existing-matches`: the shim file already exists and its contents
/// equal the expected body, so we left it alone (idempotent re-run).
pub(crate) const SKIP_EXISTING_MATCHES: &str = "existing-matches";
/// `existing-differs`: the shim file already exists but its contents
/// differ from the expected body, AND `--force` was not passed, so we
/// left it alone to avoid clobbering caller-customized content.
pub(crate) const SKIP_EXISTING_DIFFERS: &str = "existing-differs";

/// Top-level entry point invoked from `main.rs` dispatch. Resolves the
/// running soldr binary path, writes each shim, and emits the human or
/// JSON result.
pub(crate) fn run_toolchain_link(args: LinkArgs) -> Result<i32, SoldrError> {
    let started = Instant::now();
    let soldr_bin = crate::shim_materialize::soldr_binary_source()?;
    let outcome = write_shims(&args.shim_dir, &soldr_bin, args.force)?;

    let output = ToolchainLinkOutput {
        schema_version: SCHEMA_VERSION,
        shim_dir: args.shim_dir.display().to_string(),
        tools: outcome,
        elapsed_ms: started.elapsed().as_millis(),
    };

    if args.json {
        emit_json(&output)?;
    } else {
        emit_human(&output);
    }
    Ok(0)
}

/// Resolve the on-disk shim path for a tool inside `shim_dir`. On Windows
/// this is a native `<tool>.exe` multicall shim so Rust-spawned children
/// using `CreateProcess` can discover it without shell mediation.
pub(crate) fn shim_path(shim_dir: &Path, tool: &str) -> PathBuf {
    shim_dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX))
}

/// Core file-writer. Creates `shim_dir` if missing, then per tool:
/// - if no existing file → hardlink/copy + created=true
/// - if existing file with matching executable bytes → skip (existing-matches)
/// - if existing file with different body AND !force → skip (existing-differs)
/// - if existing file with different body AND force → hardlink/copy + created=true
pub(crate) fn write_shims(
    shim_dir: &Path,
    soldr_bin: &Path,
    force: bool,
) -> Result<Vec<ToolEntry>, SoldrError> {
    std::fs::create_dir_all(shim_dir).map_err(SoldrError::Io)?;
    let mut out = Vec::with_capacity(LINK_SHIMMED_TOOLS.len());
    for tool in LINK_SHIMMED_TOOLS {
        let path = shim_path(shim_dir, tool);
        let entry = if crate::shim_materialize::executable_matches(&path, soldr_bin)? {
            ToolEntry {
                name: (*tool).to_string(),
                shim_path: path.display().to_string(),
                created: false,
                skip_reason: Some(SKIP_EXISTING_MATCHES.to_string()),
            }
        } else if path.exists() && !force {
            ToolEntry {
                name: (*tool).to_string(),
                shim_path: path.display().to_string(),
                created: false,
                skip_reason: Some(SKIP_EXISTING_DIFFERS.to_string()),
            }
        } else {
            let result = crate::shim_materialize::materialize_executable(soldr_bin, &path)?;
            ToolEntry {
                name: (*tool).to_string(),
                shim_path: path.display().to_string(),
                created: result.created,
                skip_reason: (!result.created).then(|| SKIP_EXISTING_MATCHES.to_string()),
            }
        };
        out.push(entry);
    }
    Ok(out)
}

fn emit_json(output: &ToolchainLinkOutput) -> Result<(), SoldrError> {
    let payload = serde_json::to_string_pretty(output)
        .map_err(|e| SoldrError::Other(format!("link: failed to serialize JSON: {e}")))?;
    println!("{payload}");
    Ok(())
}

fn emit_human(output: &ToolchainLinkOutput) {
    println!("soldr toolchain link: shim dir {}", output.shim_dir);
    let mut created = 0usize;
    let mut skipped_matches = 0usize;
    let mut skipped_differs = 0usize;
    for entry in &output.tools {
        if entry.created {
            created += 1;
            println!("  wrote   {} -> {}", entry.name, entry.shim_path);
        } else {
            match entry.skip_reason.as_deref() {
                Some(SKIP_EXISTING_MATCHES) => {
                    skipped_matches += 1;
                    println!(
                        "  skip    {} (existing matches) {}",
                        entry.name, entry.shim_path
                    );
                }
                Some(SKIP_EXISTING_DIFFERS) => {
                    skipped_differs += 1;
                    println!(
                        "  skip    {} (existing differs; pass --force to overwrite) {}",
                        entry.name, entry.shim_path
                    );
                }
                Some(other) => {
                    println!("  skip    {} ({}) {}", entry.name, other, entry.shim_path);
                }
                None => {
                    // Defensive: a non-created entry should always carry
                    // a skip_reason. Print verbatim so callers can debug.
                    println!("  skip    {} {}", entry.name, entry.shim_path);
                }
            }
        }
    }
    println!(
        "soldr toolchain link: {created} written, {skipped_matches} already up-to-date, \
         {skipped_differs} skipped (use --force to overwrite)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn system_cmd_exe() -> PathBuf {
        let system_root = std::env::var_os("SystemRoot")
            .unwrap_or_else(|| std::ffi::OsString::from("C:\\Windows"));
        PathBuf::from(system_root).join("System32").join("cmd.exe")
    }

    fn tempdir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("soldr-link-test-{label}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    fn fake_soldr_binary(dir: &Path, label: &str) -> PathBuf {
        let suffix = std::env::consts::EXE_SUFFIX;
        let path = dir.join(format!("soldr-{label}{suffix}"));
        std::fs::write(&path, format!("fake-soldr-{label}")).expect("write fake soldr");
        path
    }

    #[test]
    fn shimmed_tools_match_setup_soldr_routed_tools() {
        // The setup-soldr `ROUTED_TOOLS` constant must stay in sync with
        // this list. If a tool is added on one side without the other,
        // setup-soldr#133 delegation breaks.
        assert_eq!(
            LINK_SHIMMED_TOOLS,
            &["cargo", "rustfmt", "clippy-driver", "rustc", "rustdoc"]
        );
    }

    #[test]
    fn shim_path_appends_exe_on_windows_only() {
        let dir = PathBuf::from("/tmp/shims");
        let p = shim_path(&dir, "cargo");
        let expected = crate::platform::executable::name::native("cargo");
        assert!(
            p.ends_with(&expected),
            "shim path should use the host-native name {expected}: {}",
            p.display()
        );
    }

    #[test]
    fn write_shims_creates_every_routed_tool_on_fresh_dir() {
        let dir = tempdir("fresh");
        let soldr = fake_soldr_binary(&dir, "fresh");
        let result = write_shims(&dir, &soldr, false).expect("write_shims");
        assert_eq!(result.len(), LINK_SHIMMED_TOOLS.len());
        for (entry, expected) in result.iter().zip(LINK_SHIMMED_TOOLS.iter()) {
            assert_eq!(&entry.name, expected, "tool order must match");
            assert!(
                entry.created,
                "every shim should be created on fresh run: {entry:?}"
            );
            assert!(entry.skip_reason.is_none());
            let p = shim_path(&dir, expected);
            assert!(p.is_file(), "missing shim at {}", p.display());
        }
    }

    #[test]
    fn windows_link_shims_are_visible_to_rust_command_lookup() {
        if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
            return;
        }
        let dir = tempdir("windows-command-lookup");
        let cmd = system_cmd_exe();
        assert!(cmd.is_file(), "missing {}", cmd.display());
        write_shims(&dir, &cmd, false).expect("write shims");

        for tool in LINK_SHIMMED_TOOLS {
            let output = std::process::Command::new(tool)
                .args(["/D", "/C", "exit 0"])
                .env("PATH", &dir)
                .output()
                .unwrap_or_else(|err| {
                    panic!("Rust Command::new({tool:?}) did not find executable shim: {err}")
                });
            assert!(
                output.status.success(),
                "link shim {tool} resolved but failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn write_shims_skips_existing_matching_files() {
        let dir = tempdir("idempotent");
        let soldr = fake_soldr_binary(&dir, "idempotent");
        let first = write_shims(&dir, &soldr, false).expect("first run");
        assert!(first.iter().all(|e| e.created));

        let second = write_shims(&dir, &soldr, false).expect("second run");
        for entry in &second {
            assert!(!entry.created, "no file should be re-created: {entry:?}");
            assert_eq!(
                entry.skip_reason.as_deref(),
                Some(SKIP_EXISTING_MATCHES),
                "second run should report existing-matches: {entry:?}"
            );
        }
    }

    #[test]
    fn write_shims_skips_existing_differing_files_without_force() {
        let dir = tempdir("differs-noforce");
        let soldr = fake_soldr_binary(&dir, "differs-noforce");
        // Seed every shim with foreign content.
        std::fs::create_dir_all(&dir).expect("mkdir");
        for tool in LINK_SHIMMED_TOOLS {
            let p = shim_path(&dir, tool);
            std::fs::write(&p, "FOREIGN CONTENT").expect("seed");
        }

        let result = write_shims(&dir, &soldr, false).expect("write");
        for entry in &result {
            assert!(
                !entry.created,
                "differs+!force must NOT overwrite: {entry:?}"
            );
            assert_eq!(
                entry.skip_reason.as_deref(),
                Some(SKIP_EXISTING_DIFFERS),
                "differs+!force should report existing-differs: {entry:?}"
            );
            let on_disk = std::fs::read_to_string(&entry.shim_path).expect("read");
            assert_eq!(on_disk, "FOREIGN CONTENT", "must not overwrite");
        }
    }

    #[test]
    fn write_shims_overwrites_existing_differing_files_with_force() {
        let dir = tempdir("differs-force");
        let soldr = fake_soldr_binary(&dir, "differs-force");
        std::fs::create_dir_all(&dir).expect("mkdir");
        for tool in LINK_SHIMMED_TOOLS {
            let p = shim_path(&dir, tool);
            std::fs::write(&p, "FOREIGN CONTENT").expect("seed");
        }

        let result = write_shims(&dir, &soldr, true).expect("write");
        for entry in &result {
            assert!(entry.created, "differs+force MUST overwrite: {entry:?}");
            assert!(entry.skip_reason.is_none());
            let on_disk = std::fs::read_to_string(&entry.shim_path).expect("read");
            assert_eq!(
                on_disk,
                std::fs::read_to_string(&soldr).expect("read source"),
                "forced overwrite content mismatch"
            );
        }
    }

    #[test]
    fn write_shims_overwrite_with_force_is_idempotent() {
        let dir = tempdir("force-idempotent");
        let soldr = fake_soldr_binary(&dir, "force-idempotent");
        let first = write_shims(&dir, &soldr, true).expect("first");
        assert!(first.iter().all(|e| e.created));

        // Second --force run: existing-matches is still detected, so we
        // do NOT churn the file. (--force only matters when content
        // differs.)
        let second = write_shims(&dir, &soldr, true).expect("second");
        for entry in &second {
            assert!(!entry.created, "no churn when existing matches: {entry:?}");
            assert_eq!(entry.skip_reason.as_deref(), Some(SKIP_EXISTING_MATCHES));
        }
    }

    #[test]
    fn json_payload_serialises_schema_v1_and_tools_array() {
        let entries = vec![
            ToolEntry {
                name: "cargo".to_string(),
                shim_path: "/tmp/shims/cargo".to_string(),
                created: true,
                skip_reason: None,
            },
            ToolEntry {
                name: "rustfmt".to_string(),
                shim_path: "/tmp/shims/rustfmt".to_string(),
                created: false,
                skip_reason: Some(SKIP_EXISTING_MATCHES.to_string()),
            },
        ];
        let output = ToolchainLinkOutput {
            schema_version: SCHEMA_VERSION,
            shim_dir: "/tmp/shims".to_string(),
            tools: entries,
            elapsed_ms: 7,
        };
        let json = serde_json::to_string(&output).expect("serialise");
        let parsed: Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], Value::from(1));
        assert_eq!(parsed["shim_dir"], Value::from("/tmp/shims"));
        let tools = parsed["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], Value::from("cargo"));
        assert_eq!(tools[0]["created"], Value::from(true));
        assert!(
            tools[0].get("skip_reason").is_none(),
            "skip_reason must be omitted when None: {}",
            tools[0]
        );
        assert_eq!(tools[1]["created"], Value::from(false));
        assert_eq!(tools[1]["skip_reason"], Value::from(SKIP_EXISTING_MATCHES));
        assert!(parsed["elapsed_ms"].is_number());
    }
}
