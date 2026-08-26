//! `soldr toolchain ensure` — auto-bootstrap rustup if missing, run the
//! `prepare` pipeline, then smoke-verify the resolved toolchain by
//! spawning `cargo --version` and `rustc --version`. With `--json`,
//! emits a stable machine-facing payload (`schema_version: 1`) that
//! `setup-soldr#133` consumes to delegate its TS toolchain logic.
//!
//! Phase 2 of #407. The JSON schema is frozen at version 1 — bumping it
//! requires a coordinated version bump in the consumer.

use serde::Serialize;
use std::time::Instant;

use crate::core::{
    command_output_with_timeout, suppress_windows_console_window, SoldrError, SoldrPaths,
};
use crate::{
    resolve_toolchain_binary,
    toolchain::{run_prepare_inner, PrepareSummary},
};

const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize, Debug)]
pub(crate) struct ToolchainEnsureOutput {
    pub schema_version: u32,
    pub channel: Option<String>,
    pub rustup_bootstrapped: bool,
    pub components_added: Vec<String>,
    pub targets_added: Vec<String>,
    pub plugins_installed: Vec<String>,
    pub smoke_verify: SmokeVerify,
    pub elapsed_ms: u128,
}

#[derive(Serialize, Debug, Default)]
pub(crate) struct SmokeVerify {
    pub cargo_version: Option<String>,
    pub rustc_version: Option<String>,
    pub ok: bool,
}

/// Implementation of `soldr toolchain ensure`. Returns the soldr exit
/// code: 0 on success, non-zero when smoke verify fails or the
/// underlying `prepare` pipeline reported a non-zero rustup / cargo
/// exit. In `--json` mode the JSON payload is emitted unconditionally
/// (even on smoke-verify failure) so the consumer can introspect.
pub(crate) async fn run_toolchain_ensure(json: bool) -> Result<i32, SoldrError> {
    let started = Instant::now();

    // soldr#2892: in `--json` this process's stdout IS the payload, and the
    // installer children below inherit it. They did, and rustup's stdout
    // landed in front of the JSON -- `json.load` rejects the result with
    // `Extra data: line 2 column 7`, which is how the target-run lane found
    // this.
    //
    // Not the fully-quiet marker `env --json` uses: that also silences the
    // stall heartbeat, on the grounds that its callers merge stdout and
    // stderr. This verb's caller reads stderr as a log and stdout as a file,
    // and rustup's progress is exactly what stops a human killing a
    // multi-minute first-time install. So the child's stdout moves to stderr
    // rather than being discarded.
    if json {
        std::env::set_var(crate::core::quiet::PAYLOAD_STDOUT_ENV_VAR, "1");
    }

    // 1. Bootstrap rustup if missing. Auto-bootstrap respects
    //    SOLDR_NO_BOOTSTRAP=1 by silently leaving the host unchanged —
    //    `prepare` will then surface the usual "rustup not found"
    //    diagnostic via the spawn-failure path. We deliberately don't
    //    re-implement the no-bootstrap diagnostic here.
    let rustup_bootstrapped = bootstrap_rustup_if_missing().await?;

    // 2. Read the manifest. Missing manifest is not an error — emit the
    //    schema-v1 empty payload so consumers can still parse it.
    let workspace_root = std::env::current_dir().map_err(SoldrError::from)?;
    let manifest = crate::core::read_rust_toolchain_manifest(&workspace_root)?;

    // 3. Run the prepare pipeline if a channel is declared. Otherwise
    //    skip it entirely (matching `prepare`'s no-channel behavior).
    let (prepare_code, prepare_summary) = if let Some(channel) = manifest.channel.as_deref() {
        run_prepare_inner(channel, &manifest)?
    } else {
        (0, PrepareSummary::default())
    };

    // 4. If prepare reported a non-zero exit, surface it without running
    //    smoke verify (the toolchain is already broken).
    if prepare_code != 0 {
        if json {
            emit_json(ToolchainEnsureOutput {
                schema_version: SCHEMA_VERSION,
                channel: manifest.channel.clone(),
                rustup_bootstrapped,
                components_added: prepare_summary.components_added,
                targets_added: prepare_summary.targets_added,
                plugins_installed: prepare_summary.plugins_installed,
                smoke_verify: SmokeVerify::default(),
                elapsed_ms: started.elapsed().as_millis(),
            })?;
        } else {
            eprintln!("soldr toolchain ensure: prepare exited with status {prepare_code}");
        }
        return Ok(prepare_code);
    }

    // 5. Smoke verify only when a channel exists. Without a manifest
    //    there's no toolchain to validate against.
    let smoke = if manifest.channel.is_some() {
        run_smoke_verify()?
    } else {
        SmokeVerify {
            cargo_version: None,
            rustc_version: None,
            // No channel means nothing to verify; treat as ok so the
            // exit code stays 0 (matches prepare's no-channel path).
            ok: true,
        }
    };

    let smoke_ok = smoke.ok;
    let output = ToolchainEnsureOutput {
        schema_version: SCHEMA_VERSION,
        channel: manifest.channel.clone(),
        rustup_bootstrapped,
        components_added: prepare_summary.components_added,
        targets_added: prepare_summary.targets_added,
        plugins_installed: prepare_summary.plugins_installed,
        smoke_verify: smoke,
        elapsed_ms: started.elapsed().as_millis(),
    };

    if json {
        emit_json(output)?;
    } else {
        emit_human(&output);
    }

    // soldr#1059 — flag a shadowing standalone `cargo` on PATH. JSON
    // callers (setup-soldr#133) already pick this up through the
    // doctor probe; the warning here is for the human surface only,
    // emitted to stderr so JSON consumers ignore it.
    if !json {
        if let Some(finding) = crate::cargo_path_check::detect_cargo_on_path() {
            if let Some(msg) = crate::cargo_path_check::warning_for(&finding) {
                eprintln!("{msg}");
            }
        }
    }

    if smoke_ok {
        Ok(0)
    } else {
        // Non-zero exit so shell-pipeline consumers (setup-soldr#133)
        // can detect the failure without parsing JSON.
        Ok(1)
    }
}

async fn bootstrap_rustup_if_missing() -> Result<bool, SoldrError> {
    // Honor the test-only `SOLDR_TEST_RUSTUP_BIN` override: when set, we
    // are inside an integration test that has already pre-provisioned a
    // fake rustup. Skip the bootstrap branch entirely.
    if std::env::var_os(crate::TEST_RUSTUP_BIN_ENV_VAR).is_some_and(|v| !v.is_empty()) {
        return Ok(false);
    }

    let paths = SoldrPaths::new()?;
    match crate::fetch::auto_bootstrap_if_missing(&paths).await? {
        crate::fetch::AutoBootstrapOutcome::AlreadyInstalled(_) => Ok(false),
        crate::fetch::AutoBootstrapOutcome::Installed(_) => Ok(true),
        // Opt-out leaves the host unchanged. The subsequent prepare run
        // will surface the standard "rustup not found" message.
        crate::fetch::AutoBootstrapOutcome::OptedOut => Ok(false),
    }
}

fn run_smoke_verify() -> Result<SmokeVerify, SoldrError> {
    let cargo_version = probe_version("cargo");
    let rustc_version = probe_version("rustc");
    let ok = cargo_version.is_some() && rustc_version.is_some();
    Ok(SmokeVerify {
        cargo_version,
        rustc_version,
        ok,
    })
}

/// Spawn `<tool> --version` and capture stdout. Returns `None` when
/// either the binary couldn't be resolved, the spawn failed, the exit
/// status was non-zero, or stdout was empty. The `None` case is what
/// drives `smoke_verify.ok = false`.
fn probe_version(tool: &str) -> Option<String> {
    let binary = resolve_toolchain_binary(tool).ok()?;
    let mut command = std::process::Command::new(&binary);
    command.arg("--version");
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &binary);
    suppress_windows_console_window(&mut command);
    let output = command_output_with_timeout(&mut command, &format!("{tool} --version")).ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .map(str::trim)
        .map(str::to_string)?;
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

fn emit_json(output: ToolchainEnsureOutput) -> Result<(), SoldrError> {
    let payload = serde_json::to_string_pretty(&output)
        .map_err(|e| SoldrError::Other(format!("ensure: failed to serialize JSON: {e}")))?;
    println!("{payload}");
    Ok(())
}

fn emit_human(output: &ToolchainEnsureOutput) {
    match output.channel.as_deref() {
        Some(channel) => println!("soldr toolchain ensure: channel {channel}"),
        None => println!(
            "soldr toolchain ensure: no rust-toolchain.toml channel found; \
             nothing to install or verify."
        ),
    }
    if output.rustup_bootstrapped {
        println!("soldr toolchain ensure: bootstrapped rustup into soldr-managed bin dir");
    }
    if !output.components_added.is_empty() {
        println!(
            "soldr toolchain ensure: components installed: {}",
            output.components_added.join(", ")
        );
    }
    if !output.targets_added.is_empty() {
        println!(
            "soldr toolchain ensure: targets installed: {}",
            output.targets_added.join(", ")
        );
    }
    if !output.plugins_installed.is_empty() {
        println!(
            "soldr toolchain ensure: plugins installed: {}",
            output.plugins_installed.join(", ")
        );
    }
    if let Some(cargo) = output.smoke_verify.cargo_version.as_deref() {
        println!("soldr toolchain ensure: smoke verify cargo: {cargo}");
    }
    if let Some(rustc) = output.smoke_verify.rustc_version.as_deref() {
        println!("soldr toolchain ensure: smoke verify rustc: {rustc}");
    }
    if output.smoke_verify.ok {
        println!("soldr toolchain ensure: smoke verify ok");
    } else if output.channel.is_some() {
        println!("soldr toolchain ensure: smoke verify FAILED");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn empty_smoke_verify_defaults_to_not_ok() {
        let s = SmokeVerify::default();
        assert!(!s.ok);
        assert!(s.cargo_version.is_none());
        assert!(s.rustc_version.is_none());
    }

    #[test]
    fn json_serialises_schema_version_first_and_arrays_present() {
        let output = ToolchainEnsureOutput {
            schema_version: SCHEMA_VERSION,
            channel: Some("1.94.1".to_string()),
            rustup_bootstrapped: false,
            components_added: vec!["clippy".to_string()],
            targets_added: vec![],
            plugins_installed: vec!["cargo-nextest@0.9".to_string()],
            smoke_verify: SmokeVerify {
                cargo_version: Some("cargo 1.94.1".to_string()),
                rustc_version: Some("rustc 1.94.1".to_string()),
                ok: true,
            },
            elapsed_ms: 42,
        };
        let json = serde_json::to_string(&output).expect("serialise");
        let parsed: Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], Value::from(1));
        assert_eq!(parsed["channel"], Value::from("1.94.1"));
        assert!(parsed["targets_added"].is_array());
        assert_eq!(parsed["smoke_verify"]["ok"], Value::from(true));
    }
}
