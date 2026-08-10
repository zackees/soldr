//! Standalone cleanup CLI for v1 CacheManifest registries.
//!
//! Phase 2 of #228 (#231). This binary does not require the broker or
//! originating daemons to be running.

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use running_process::broker::manifest;
use running_process::cleanup::{
    actions_json, instances, list, parse_duration_secs, prune, uninstall, verify_artifacts,
    verify_basic,
};

#[derive(Parser)]
#[command(
    name = "running-process-cleanup",
    about = "Inspect and clean running-process v1 CacheManifest registries"
)]
struct Cli {
    /// Override the central manifest registry directory.
    #[arg(long, global = true)]
    registry_dir: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List manifests in the central registry.
    List {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Prune dormant or explicitly-selected cache roots.
    Prune {
        /// Select manifests dormant for this duration, e.g. 30d, 12h.
        #[arg(long)]
        dormant_after: Option<String>,
        /// Keep current manifests that have a daemon process recorded.
        #[arg(long)]
        keep_current: bool,
        /// Keep the N most recently-active versions per service.
        #[arg(long)]
        keep_last: Option<usize>,
        /// Restrict pruning to a single service.
        #[arg(long)]
        service: Option<String>,
        /// Restrict pruning to a single service version.
        #[arg(long)]
        version: Option<String>,
        /// Actually delete selected roots. Omit for dry-run.
        #[arg(long)]
        confirm: bool,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Uninstall one service's manifest-declared cache roots.
    Uninstall {
        /// Service name to uninstall.
        service: String,
        /// Preserve CACHE_CONFIG roots.
        #[arg(long)]
        keep_config: bool,
        /// Actually delete selected roots. Omit for dry-run.
        #[arg(long)]
        confirm: bool,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Registry consistency verification plus exhaustive daemon-artifact
    /// reconciliation (#391): socket, pid file, .servicedef files, SQLite
    /// registry (incl. WAL/SHM), log files, emergency reserve, shadow dir.
    /// Read-only: nothing is deleted.
    Verify {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
        /// Reconcile the artifacts of this scope hash instead of the
        /// global daemon scope.
        #[arg(long)]
        scope_hash: Option<String>,
    },
    /// Enumerate visible broker instances.
    Instances {
        /// Placeholder for Phase 4 broker status aggregation.
        #[arg(long)]
        status: bool,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let registry_dir = cli
        .registry_dir
        .unwrap_or_else(manifest::central_registry_dir);

    match cli.command {
        Commands::List { json } => {
            let manifests = list::list(&registry_dir);
            if json {
                println!("{}", list::render_json(&manifests));
            } else if manifests.is_empty() {
                println!("no manifests found in {}", registry_dir.display());
            } else {
                for manifest in manifests {
                    println!(
                        "{} {} roots={} last_active_unix_ms={}",
                        manifest.service_name,
                        manifest.service_version,
                        manifest.roots.len(),
                        manifest.last_active_unix_ms
                    );
                }
            }
        }
        Commands::Prune {
            dormant_after,
            keep_current,
            keep_last,
            service,
            version,
            confirm,
            json,
        } => {
            let dormant_after_secs = dormant_after
                .as_deref()
                .map(parse_duration_secs)
                .transpose()
                .context("invalid --dormant-after")?;
            let options = prune::PruneOptions {
                dormant_after_secs,
                keep_current,
                keep_last,
                service,
                version,
                confirm,
            };
            let actions = prune::run(&registry_dir, &options)?;
            if json {
                println!("{}", actions_json(1, &actions));
            } else {
                print_actions(&actions, confirm);
            }
        }
        Commands::Uninstall {
            service,
            keep_config,
            confirm,
            json,
        } => {
            let actions = uninstall::run(&registry_dir, &service, keep_config, confirm)?;
            if json {
                println!("{}", actions_json(1, &actions));
            } else {
                print_actions(&actions, confirm);
            }
        }
        Commands::Verify { json, scope_hash } => {
            let report = verify_basic::run(&registry_dir);
            let artifact_paths =
                verify_artifacts::ArtifactPaths::from_environment(scope_hash.as_deref());
            let artifacts = verify_artifacts::run(&artifact_paths);
            if json {
                // Additive extension of the frozen verify JSON shape: the
                // registry document gains an `artifacts` object (#391).
                let mut document: serde_json::Value =
                    serde_json::from_str(&verify_basic::render_json(&report))
                        .context("internal: verify JSON did not round-trip")?;
                document["artifacts"] = artifacts.to_json_value();
                println!("{document}");
            } else {
                if report.findings.is_empty() {
                    println!("verified {} manifest(s); no findings", report.scanned);
                } else {
                    for finding in &report.findings {
                        println!(
                            "{}: {}: {}",
                            finding.severity,
                            finding.path.display(),
                            finding.message
                        );
                    }
                }
                print!("{}", artifacts.render_text());
            }
            if artifacts.exit_code() != 0 {
                anyhow::bail!("artifact verification could not inspect every location");
            }
        }
        Commands::Instances { status, json } => {
            let found = instances::list();
            if json {
                println!("{}", instances::render_json(&found));
            } else if found.is_empty() {
                if status {
                    println!(
                        "no broker instances found; status aggregation requires Phase 4 broker"
                    );
                } else {
                    println!("no broker instances found");
                }
            } else {
                for instance in found {
                    println!("{}", instance.path);
                }
            }
        }
    }

    Ok(())
}

fn print_actions(actions: &[running_process::cleanup::CleanupAction], confirm: bool) {
    if actions.is_empty() {
        println!("no matching cache roots");
        return;
    }
    for action in actions {
        let verb = if action.skipped {
            "skip"
        } else if confirm {
            "deleted"
        } else {
            "would delete"
        };
        if let Some(reason) = &action.skip_reason {
            println!("{verb}: {} ({reason})", action.path.display());
        } else {
            println!("{verb}: {}", action.path.display());
        }
    }
}
