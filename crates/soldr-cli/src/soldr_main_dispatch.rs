async fn run_cli(cli: Cli) -> Result<(), SoldrError> {
    // #1364: a truthy `ZCCACHE_DISABLE` acts like `--no-cache` so the
    // standard zccache kill-switch actually bypasses the wrapper/daemon.
    let cache_enabled = !cli.no_cache && !cargo_front_door::zccache_disable_requested();
    let trust_inherited_soldr_env = cli.trust_inherited_soldr_env;
    // soldr#1766 / soldr#1761: global flags with an env-var spelling are
    // published together by `Cli::export_global_env`, so the whole process
    // tree -- including the daemon, which auto-forwards `SOLDR_*` -- agrees
    // without threading booleans through every prepare path.
    cli.export_global_env();

    match cli.command {
        Commands::Build { args } => {
            run_blessed_build(args, cache_enabled, trust_inherited_soldr_env).await?;
        }
        Commands::Cc(args) => {
            guarded_exit(crate::cc_cmd::run(args, crate::cc_cmd::Language::C).await?)
        }
        Commands::Cxx(args) => {
            guarded_exit(crate::cc_cmd::run(args, crate::cc_cmd::Language::Cxx).await?)
        }
        Commands::Wheel(args) => {
            // soldr#2139 gap 1. Re-enter through `soldr maturin build ...` so
            // provisioning, toolchain pinning, the build lease, target prep,
            // and the PyO3 plan stay in exactly one place. See `wheel_cmd`.
            let argv = crate::wheel_cmd::maturin_invocation(
                &args,
                !cache_enabled,
                trust_inherited_soldr_env,
            )?;
            guarded_exit(Box::pin(run_with_args("soldr", &argv)).await?);
        }
        Commands::Cargo { args } => {
            let args = crate::target_lifecycle::prepare_cargo_invocation(args).await?;
            // soldr#1079: same MSVC host env injection that
            // `Commands::Build` does, so `soldr cargo build` /
            // `soldr cargo test` on a native Windows MSVC target also
            // succeed from a plain PowerShell without `$env:LIB`.
            ensure_msvc_host_env_for_native(&args).await;
            guarded_exit(
                cargo_front_door::run_cargo_front_door(
                    &args,
                    cache_enabled,
                    trust_inherited_soldr_env,
                )
                .await?,
            );
        }
        Commands::Dylint { args } => {
            guarded_exit(
                Box::pin(run_dylint_command(
                    args,
                    cache_enabled,
                    trust_inherited_soldr_env,
                ))
                .await?,
            );
        }
        Commands::Lint { args } => {
            guarded_exit(
                lint_cmd::run_lint(&args, cache_enabled, trust_inherited_soldr_env).await?,
            );
        }
        Commands::CiTest { args } => {
            guarded_exit(
                crate::ci_test::run(&args, cache_enabled, trust_inherited_soldr_env).await?,
            );
        }
        Commands::Cook { args } => {
            guarded_exit(cook::run_cook(&args, cache_enabled).await?);
        }
        Commands::Exec { args } => {
            guarded_exit(exec_cmd::run_exec(&args)?);
        }
        Commands::Rustc { args } => {
            guarded_exit(toolchain::run_rustc_like("rustc", &args, cache_enabled)?);
        }
        Commands::Rustfmt { args } => {
            guarded_exit(toolchain::run_rustfmt(&args, cache_enabled)?);
        }
        Commands::ClippyDriver { args } => {
            guarded_exit(toolchain::run_rustc_like(
                "clippy-driver",
                &args,
                cache_enabled,
            )?);
        }
        Commands::Rustdoc { args } => {
            guarded_exit(toolchain::run_rustdoc(&args)?);
        }
        Commands::RustGdb { args } => {
            guarded_exit(toolchain::run_toolchain_passthrough("rust-gdb", &args)?);
        }
        Commands::RustLldb { args } => {
            guarded_exit(toolchain::run_toolchain_passthrough("rust-lldb", &args)?);
        }
        Commands::RustAnalyzer { args } => {
            guarded_exit(toolchain::run_rust_analyzer(&args, cache_enabled)?);
        }
        Commands::Rustup { args } => {
            guarded_exit(toolchain::run_rustup_passthrough(&args)?);
        }
        Commands::Toolchain { subcommand } => match subcommand {
            ToolchainSubcommand::Install => {
                guarded_exit(toolchain::run_toolchain_install()?);
            }
            ToolchainSubcommand::Prepare => {
                guarded_exit(toolchain::run_toolchain_prepare()?);
            }
            ToolchainSubcommand::Ensure { json } => {
                guarded_exit(toolchain_ensure::run_toolchain_ensure(json).await?);
            }
            ToolchainSubcommand::Link {
                shim_dir,
                json,
                force,
            } => {
                guarded_exit(toolchain_link::run_toolchain_link(
                    toolchain_link::LinkArgs {
                        shim_dir,
                        json,
                        force,
                    },
                )?);
            }
            ToolchainSubcommand::Doctor { json } => {
                guarded_exit(toolchain_doctor::run_toolchain_doctor(json)?);
            }
            ToolchainSubcommand::Catalogue { json } => {
                guarded_exit(crate::fetch::manifest_lookup::run_toolchain_catalogue(json).await?);
            }
        },
        Commands::Bootstrap { json } => {
            guarded_exit(bootstrap::run_bootstrap(json).await?);
        }
        Commands::Doctor {
            json,
            refresh_defender_probe,
            remove_shadowing_shim: fix,
        } => guarded_exit(doctor::run_doctor(json, refresh_defender_probe, fix)?),
        Commands::Optimize(args) => {
            guarded_exit(optimize::run_optimize(args)?);
        }
        Commands::Shims { json } => {
            let paths = SoldrPaths::new()?;
            guarded_exit(install_shims::run_shims(&paths, json)?);
        }
        Commands::DefenderExclusions { subcommand } => {
            guarded_exit(optimize::run_defender_exclusions(subcommand)?);
        }
        Commands::Save(args) => {
            guarded_exit(save_load::run_save(args));
        }
        Commands::Hydrate(args) => {
            guarded_exit(save_load::run_load(args));
        }
        Commands::Archive {
            target,
            stage_dir,
            input,
            extract_dir,
            output,
        } => {
            archive_cmd::run(target, output, stage_dir, input, extract_dir)?;
        }
        Commands::Prepare {
            target,
            github_env,
            save,
            restore,
        } => {
            // `--target` accepts three shapes — see
            // `prepare_cmd::parse_target_arg` for the parser:
            //   - `all`         → every triple under
            //                     `[workspace.metadata.soldr].targets`
            //                     (needs a workspace context — #914).
            //   - `<a>,<b>,<c>` → an explicit comma-separated list.
            //                     Useful for docker-image bake steps
            //                     where no Cargo.toml is mounted yet.
            //   - `<triple>`    → a single triple (legacy default).
            let targets =
                crate::target_lifecycle::resolve_prepare_targets(&target, github_env.is_some())?;
            // soldr#940 — run per-target preparations concurrently with
            // a bounded worker pool. `--target all` previously serialized
            // 8 cold downloads on top of each other; now they overlap.
            // Per-target dispatch (zig + Apple SDK, LLVM + xwin, …) is
            // also internally parallelized — see `prepare_cmd::run`.
            //
            // Concurrency cap: min(num_cpus, num_targets, 4). 4 is the
            // GitHub-runner-friendly ceiling — beyond that the
            // contention on the NIC dominates the parallelism win.
            let cpu_cap = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2);
            let concurrency = cpu_cap.min(targets.len()).clamp(1, 4);
            if targets.len() > 1 {
                eprintln!(
                    "soldr prepare: parallelizing {} targets with {} workers (soldr#940)",
                    targets.len(),
                    concurrency,
                );
            }
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
            let mut handles = Vec::with_capacity(targets.len());
            for triple in &targets {
                let triple_owned = triple.clone();
                let github_env_clone = github_env.clone();
                let save_clone = save.clone();
                let restore_clone = restore.clone();
                let sem_clone = std::sync::Arc::clone(&sem);
                handles.push(tokio::spawn(async move {
                    let _permit = sem_clone
                        .acquire_owned()
                        .await
                        .expect("semaphore not closed");
                    eprintln!("soldr prepare: ===== target {triple_owned} =====");
                    let result = prepare_cmd::run(
                        triple_owned.clone(),
                        github_env_clone,
                        save_clone,
                        restore_clone,
                    )
                    .await;
                    (triple_owned, result)
                }));
            }
            let mut failures: Vec<(String, String)> = Vec::new();
            for handle in handles {
                let (triple, result) = handle
                    .await
                    .map_err(|e| SoldrError::Other(format!("prepare worker join: {e}")))?;
                if let Err(e) = result {
                    eprintln!("soldr prepare: target {triple} failed: {e}");
                    failures.push((triple, e.to_string()));
                }
            }
            if !failures.is_empty() {
                let summary = failures
                    .iter()
                    .map(|(t, e)| format!("  {t}: {e}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(SoldrError::Other(format!(
                    "soldr prepare: {} of {} target(s) failed:\n{summary}",
                    failures.len(),
                    targets.len()
                )));
            }
        }
        Commands::BuildFromSource {
            tool,
            target,
            version,
        } => {
            build_from_source_cmd::run(&tool, target, version)?;
        }
        Commands::Install(args) => crate::install::run(args).await?,
        Commands::Env {
            target,
            shell_export,
            json,
            plan_only,
        } => {
            guarded_exit(env_cmd::run_env_command(&target, shell_export, json, plan_only).await?);
        }
        Commands::Status { json } => {
            let output = cache::collect_status_output(cache_enabled)?;
            if json {
                cache::print_json(&output)?;
            } else {
                cache::print_status_output(&output);
            }
        }
        Commands::Clean => cache::clear_zccache_cache()?,
        Commands::Purge => cache::purge_soldr_cache()?,
        Commands::Config => {
            println!("(config not yet implemented)");
        }
        Commands::Logs { command } => {
            match command {
                // soldr#820: `list` / `show` / `paths` are implemented;
                // `view` / `prune` remain follow-up verbs.
                Some(LogsSubcommand::List { limit, json }) => {
                    guarded_exit(logs_cmd::run_logs_list(limit, json)?);
                }
                Some(LogsSubcommand::Show { launch_id, json }) => {
                    guarded_exit(logs_cmd::run_logs_show(&launch_id, json)?);
                }
                Some(LogsSubcommand::Paths { json }) => {
                    guarded_exit(logs_cmd::run_logs_paths(json)?);
                }
                None => {
                    // Bare `soldr logs` with no subcommand: print the help-shaped
                    // overview from the issue's design plus follow-up hints.
                    eprintln!("soldr logs — inspect soldr's runtime activity (issue #820)");
                    eprintln!();
                    eprintln!("Subcommands:");
                    eprintln!("  soldr logs list                List recent launches");
                    eprintln!("  soldr logs show <launch-id>    Session summary + log paths");
                    eprintln!("  soldr logs paths               Print every directory soldr writes logs into");
                    eprintln!();
                    eprintln!("Planned follow-up verbs (not implemented yet):");
                    eprintln!("  soldr logs view <launch-id>    Stream a launch's JSONL journal");
                    eprintln!("  soldr logs prune --keep N      Bounded retention sweep");
                    eprintln!();
                    eprintln!("Run `soldr logs list --json` for a machine-readable form.");
                    guarded_exit(0);
                }
            }
        }
        Commands::Cache { json, command } => match command {
            Some(CacheSubcommand::Report { json: report_json }) => {
                cache::run_cache_report_command(report_json || json)?;
            }
            Some(CacheSubcommand::Shutdown {
                archive_logs,
                no_depgraph_save,
                shutdown_timeout_seconds,
                no_wait,
                json: shutdown_json,
            }) => {
                cache::run_cache_shutdown_command(
                    archive_logs,
                    no_depgraph_save,
                    shutdown_timeout_seconds,
                    !no_wait,
                    shutdown_json || json,
                )
                .await?;
            }
            Some(CacheSubcommand::Flush { json: flush_json }) => {
                cache::run_cache_flush_command(flush_json || json).await?;
            }
            Some(CacheSubcommand::PruneTarget {
                path,
                dry_run,
                no_dry_run,
                force,
                keep_latest,
                json: prune_json,
            }) => {
                let effective_dry_run = !(force || no_dry_run);
                // Either flag pair maps onto the same boolean; `dry_run`
                // is the documented default so we accept it explicitly.
                let _ = dry_run;
                cache::run_cache_prune_target_command(
                    path,
                    effective_dry_run,
                    keep_latest,
                    prune_json || json,
                )?;
            }
            Some(CacheSubcommand::TrimTarget {
                path,
                profile,
                dry_run,
                no_dry_run,
                force,
                json: trim_json,
            }) => {
                let effective_dry_run = !(force || no_dry_run);
                let _ = dry_run;
                let trim_profile = match profile {
                    TrimProfileArg::Local => cache::TrimProfile::Local,
                    TrimProfileArg::Ci => cache::TrimProfile::Ci,
                };
                cache::run_cache_trim_target_command(
                    path,
                    trim_profile,
                    effective_dry_run,
                    trim_json || json,
                )?;
            }
            Some(CacheSubcommand::ReleaseWorktree {
                path,
                json: rw_json,
            }) => {
                cache::run_cache_release_worktree_command(path, rw_json || json)?;
            }
            Some(CacheSubcommand::SweepTrash { json: st_json }) => {
                cache::run_cache_sweep_trash_command(st_json || json)?;
            }
            None => {
                let output = cache::collect_cache_output()?;
                if json {
                    cache::print_json(&output)?;
                } else {
                    cache::print_cache_output(&output);
                }
            }
        },
        Commands::Version { json } => {
            let output = cache::version_output();
            if json {
                cache::print_json(&output)?;
            } else {
                println!("soldr {}", output.soldr_version);
            }
        }
        Commands::Gc {
            dry_run,
            all,
            older_than,
            larger_than,
            json,
            command,
        } => {
            if all {
                return Err(SoldrError::Other(
                    "`soldr gc --all` no longer deletes targets; use `soldr gc purge --all`".into(),
                ));
            }
            if dry_run && command.is_some() {
                return Err(SoldrError::Other(
                    "`soldr gc --dry-run` is a summary alias; use `soldr gc` or `soldr gc purge`"
                        .into(),
                ));
            }
            let invocation = match command {
                Some(GcSubcommand::Purge {
                    all,
                    older_than,
                    larger_than,
                    json,
                    kind,
                    registry_src,
                    git_checkouts,
                    target_incremental,
                    build_scripts,
                    doc,
                    subcommand_caches,
                }) => {
                    // #323 slice 2: --registry-src is a shorthand for
                    // --kind cargo_registry_src; clap already enforces
                    // mutual exclusion.
                    // #323 slice 3: --git-checkouts is a shorthand for
                    // --kind cargo_git_checkouts.
                    // #323 slice 4: in-target subtree shorthands map to
                    // their explicit taxonomy kinds.
                    let effective_kind = if registry_src {
                        Some(GcListKind::CargoRegistrySrc)
                    } else if git_checkouts {
                        Some(GcListKind::CargoGitCheckouts)
                    } else if target_incremental {
                        Some(GcListKind::CargoTargetIncremental)
                    } else if build_scripts {
                        Some(GcListKind::CargoTargetBuildScriptBinaries)
                    } else if doc {
                        Some(GcListKind::CargoTargetDoc)
                    } else if subcommand_caches {
                        Some(GcListKind::CargoTargetSubcommandCaches)
                    } else {
                        kind
                    };
                    match effective_kind {
                        Some(GcListKind::CargoRegistrySrc) => {
                            gc::run_gc_purge_registry_src_command(all, json)?;
                            return Ok(());
                        }
                        Some(GcListKind::CargoGitCheckouts) => {
                            gc::run_gc_purge_git_checkouts_command(all, json)?;
                            return Ok(());
                        }
                        Some(
                            GcListKind::CargoTargetIncremental
                            | GcListKind::CargoTargetBuildScriptBinaries
                            | GcListKind::CargoTargetDoc
                            | GcListKind::CargoTargetSubcommandCaches,
                        ) => {
                            gc::run_gc_purge_target_subtree_command(
                                effective_kind.expect("matched Some").into(),
                                all,
                                json,
                            )?;
                            return Ok(());
                        }
                        Some(
                            GcListKind::CargoRegistryCache
                            | GcListKind::CargoGitDb
                            | GcListKind::CargoInstalledBinaries
                            | GcListKind::RustupToolchain,
                        ) => {
                            let kind_name = match effective_kind.expect("matched Some") {
                                GcListKind::CargoRegistryCache => "cargo_registry_cache",
                                GcListKind::CargoGitDb => "cargo_git_db",
                                GcListKind::CargoInstalledBinaries => "cargo_installed_binaries",
                                GcListKind::RustupToolchain => "rustup_toolchain",
                                _ => "selected kind",
                            };
                            return Err(SoldrError::Other(format!(
                                "gc purge --kind {kind_name} is report-only; cargo/rustup own deletion for this primary cache"
                            )));
                        }
                        Some(GcListKind::CargoTarget) | None => gc::GcInvocation {
                            mode: gc::GcMode::Purge { all },
                            older_than,
                            larger_than,
                            json,
                        },
                    }
                }
                Some(GcSubcommand::List { json, kind }) => {
                    gc::run_gc_list_command(json, kind.map(Into::into))?;
                    return Ok(());
                }
                Some(GcSubcommand::Cargo(args)) => {
                    gc::run_gc_cargo_command(*args)?;
                    return Ok(());
                }
                Some(GcSubcommand::Locations { json }) => {
                    gc::run_gc_locations_command(json)?;
                    return Ok(());
                }
                Some(GcSubcommand::Sweep(args)) => {
                    gc::run_gc_sweep_command(*args)?;
                    return Ok(());
                }
                Some(GcSubcommand::Target(args)) => {
                    gc::run_gc_target_command(*args)?;
                    return Ok(());
                }
                Some(GcSubcommand::Maintain { root, json }) => {
                    let status = crate::daemon::maintenance::run_manual_root(root)
                        .await
                        .map_err(SoldrError::Other)?;
                    if json {
                        cache::print_json(&status)?;
                    } else {
                        cache::print_maintenance_status(Some(&status));
                    }
                    if status.successful_at_ms.is_none() {
                        return Err(SoldrError::Other(format!(
                            "cache maintenance did not complete: {}",
                            status
                                .deferred_reason
                                .as_deref()
                                .unwrap_or("component failure")
                        )));
                    }
                    return Ok(());
                }
                Some(GcSubcommand::AutoSweep) => {
                    gc::run_gc_auto_sweep_command()?;
                    return Ok(());
                }
                Some(GcSubcommand::HoldBuildLease) => {
                    run_build_lease_helper()?;
                    return Ok(());
                }
                None => gc::GcInvocation {
                    mode: gc::GcMode::Summary,
                    older_than,
                    larger_than,
                    json,
                },
            };
            gc::run_gc_command(invocation)?;
        }
        Commands::SessionStart {
            id,
            log,
            journal,
            json,
        } => {
            cache::run_session_start_command(id, log, journal, json).await?;
        }
        Commands::SessionEnd { id, clear, json } => {
            cache::run_session_end_command(id, clear, json)?;
        }
        Commands::Daemon { command } => run_daemon_command(command).await?,
        Commands::Broker { command } => crate::broker_cmd::run_broker_command(command)?,
        Commands::External(args) => {
            if args.is_empty() {
                eprintln!("usage: soldr <tool>[@version] [args...]");
                guarded_exit(1);
            }

            let (crate_name, version) = parse_tool_spec(&args[0]);
            let tool_args = &args[1..];

            // Issue #683 (parent #682, phase 1): bare cargo-subcommand
            // shorthand. When the typed verb (sans `@version`) is one
            // soldr already prebuilds as a cargo subcommand
            // (`KNOWN_TOOLS::lookup_by_cargo_subcommand`), route through
            // the cargo front door — `soldr nextest run` becomes
            // `soldr cargo nextest run`. This avoids the doomed
            // crates.io fetch for a literally-named `nextest` crate.
            // Version-pinned forms (`soldr nextest@0.9.x`) keep the
            // existing External path; cargo-subcommand pins are
            // managed in the soldr registry and the front door has no
            // per-invocation knob.
            if matches!(version, VersionSpec::Latest)
                && crate::fetch::lookup_by_cargo_subcommand(&crate_name).is_some()
            {
                let mut cargo_args = Vec::with_capacity(args.len());
                cargo_args.push(crate_name.clone());
                cargo_args.extend(tool_args.iter().cloned());
                let cargo_args =
                    crate::target_lifecycle::prepare_cargo_invocation(cargo_args).await?;
                guarded_exit(
                    cargo_front_door::run_cargo_front_door(
                        &cargo_args,
                        cache_enabled,
                        trust_inherited_soldr_env,
                    )
                    .await?,
                );
            }

            // Issue #685 (parent #682, phase 2): bare cargo built-in
            // shorthand. When the typed verb is one of cargo's own
            // first-party verbs (`build`, `test`, `check`, `clippy`,
            // `fmt`, ...), route through the cargo front door —
            // `soldr build --release` becomes `soldr cargo build
            // --release`. The collision verbs `clean` / `config` /
            // `version` are captured by clap before reaching this
            // arm; see `is_cargo_builtin_verb` for the explicit
            // exclusion list. Version-pinned forms keep the existing
            // External fetch path so `soldr build@1.0` parses
            // exactly like `soldr <unknown-tool>@1.0` does today.
            if matches!(version, VersionSpec::Latest) && is_cargo_builtin_verb(&crate_name) {
                let mut cargo_args = Vec::with_capacity(args.len());
                cargo_args.push(crate_name.clone());
                cargo_args.extend(tool_args.iter().cloned());
                let cargo_args =
                    crate::target_lifecycle::prepare_cargo_invocation(cargo_args).await?;
                // soldr#1105: bare-verb dispatch must also pre-inject
                // the host MSVC env so `soldr check` / `soldr build` /
                // `soldr test` on Windows behave the same as the
                // explicit `soldr cargo ...` forms with respect to
                // rust-lld's `LIB` requirement.
                ensure_msvc_host_env_for_native(&cargo_args).await;
                guarded_exit(
                    cargo_front_door::run_cargo_front_door(
                        &cargo_args,
                        cache_enabled,
                        trust_inherited_soldr_env,
                    )
                    .await?,
                );
            }

            // soldr#2898: `soldr zccache <args>` is a reserved embedded surface.
            // No standalone binary is resolved, downloaded, or invoked.
            // The compatibility forms route directly
            // into Soldr-owned compatibility handlers.
            if crate_name == "zccache" {
                guarded_exit(crate::zccache_compat::run(tool_args, version).await?);
            }

            // Issue #412: when the user typed a verb that LOOKS like
            // a typo or a renamed built-in (for example,
            // `build-from-sorce`), emit a "did you mean?" hint before
            // we fire the network fetch. The fetch still runs — the
            // suggestion is advisory.
            // Cargo's bare built-ins are an equally real top-level shorthand.
            // Prefer them before Soldr-native verbs: in particular `tset`
            // must lead a user back to `soldr test` (cargo test), not the
            // unrelated orchestration surface `soldr ci-test`.
            if let Some(suggestion) = fuzzy_match::suggest_close_match(&crate_name, CARGO_BUILTIN_VERBS)
                .or_else(|| fuzzy_match::suggest_close_match(&crate_name, SOLDR_BUILTIN_VERBS))
            {
                eprintln!("soldr: '{crate_name}' is not a known built-in soldr verb.");
                eprintln!("soldr: did you mean: {suggestion}?");
            }

            eprintln!("soldr: fetching {crate_name}...");
            // soldr#1264 follow-on: maturin gets a provisioning ladder
            // instead of the bare fetch — prebuilt binary from GitHub
            // Releases first, manual uv-provisioned isolated env as
            // the fallback (SOLDR_MATURIN_PROVISIONER=auto|binary|uv).
            // Everything else keeps the plain fetch_tool path.
            let result = if crate_name == "maturin" {
                fetch_maturin_with_provisioner(&version).await?
            } else {
                crate::fetch::fetch_tool(&crate_name, &version).await?
            };

            if result.cached {
                eprintln!("soldr: using cached {crate_name} v{}", result.version);
            } else {
                eprintln!("soldr: downloaded {crate_name} v{}", result.version);
            }

            let normalized_tool_args;
            let tool_args = if crate_name == "maturin" {
                normalized_tool_args =
                    crate::pyo3_detect::normalize_explicit_target_args(tool_args);
                normalized_tool_args.as_slice()
            } else {
                tool_args
            };
            let mut final_tool_args = tool_args.to_vec();
            let mut command = std::process::Command::new(&result.binary_path);
            let mut pep517_linker_state = None;
            let mut pep517_paths = None;
            // Held across the complete direct/PEP517 maturin child. This is
            // separate from the short-lived stats session request: the
            // OS-held lease is what prevents daemon GC from deleting a reused
            // PEP517 target or wheel namespace while maturin is using it.
            let mut _maturin_build_lease = None;

            // soldr#1264: `soldr maturin ...` is the engine behind the
            // PEP 517 build backend (src/soldr/__init__.py). maturin
            // spawns `cargo` itself, and on Windows the #493 `.cmd`
            // PATH shims below are invisible to Rust-spawned children
            // (CreateProcess resolves only `cargo.exe`, never `.cmd`),
            // so on a PATH-poisoned machine (e.g. a chocolatey GNU
            // cargo ahead of rustup's proxies) maturin silently builds
            // the wrong toolchain and cmake-based *-sys deps explode
            // in "MSYS Makefiles" flag mangling. Pin the child's
            // toolchain + build tools before exec:
            //   * `CARGO` → soldr's resolved rustup cargo (honors
            //     rust-toolchain.toml + MSVC-on-Windows). maturin
            //     reads `CARGO` before falling back to bare PATH
            //     lookup. A caller-provided CARGO always wins.
            //   * `RUSTC_WRAPPER` -> soldr's current binary when
            //     caching is enabled and the caller did not already
            //     choose a wrapper. Direct `soldr maturin build` then
            //     gets the same embedded-zccache route as the PEP 517
            //     backend. A caller-provided RUSTC_WRAPPER always
            //     wins over this auto-injection.
            //   * managed cmake/ninja env (`CMAKE`,
            //     `CMAKE_GENERATOR=Ninja`, PATH prepends) via the same
            //     `inject_cmake_tooling` the blessed `soldr build`
            //     surface uses (#1257). Same opt-outs apply.
            if crate_name == "maturin" {
                if std::env::var_os("CARGO").is_none() {
                    match resolve_toolchain_binary("cargo") {
                        Ok(cargo) => {
                            // A direct (non-rustup-proxy) toolchain cargo spawns `rustc` from
                            // PATH — on the poisoned-fixture machine that's the GNU standalone,
                            // which lacks the msvc std and dies with E0463. Pin RUSTC to the
                            // sibling rustc of the resolved cargo so cargo and rustc always come
                            // from the same toolchain; fall back to the resolver when there is
                            // no sibling.
                            if std::env::var_os("RUSTC").is_none() {
                                let sibling = cargo.parent().map(|dir| {
                                    dir.join(crate::platform::executable::name::native("rustc"))
                                });
                                match sibling.filter(|p| p.is_file()) {
                                    Some(rustc) => {
                                        command.env("RUSTC", rustc);
                                    }
                                    None => match resolve_toolchain_binary("rustc") {
                                        Ok(rustc) => {
                                            command.env("RUSTC", rustc);
                                        }
                                        Err(err) => eprintln!(
                                            "soldr warning: could not resolve \
                                             toolchain rustc for maturin: {err}"
                                        ),
                                    },
                                }
                            }
                            command.env("CARGO", &cargo);
                        }
                        Err(err) => eprintln!(
                            "soldr warning: could not resolve toolchain cargo for \
                             maturin; child falls back to PATH lookup: {err}"
                        ),
                    }
                }
                // CARGO alone is not enough: resolve_toolchain_binary's
                // last-resort probe is a PATH lookup, and on the
                // poisoned-fixture machine a GNU-host rustup resolves
                // the pinned channel to its GNU variant. Force the
                // TARGET too — same runtime MSVC-default policy the
                // cargo front door applies via CARGO_BUILD_TARGET
                // (Windows-only; explicit user env always wins). Both
                // cargo and maturin honor CARGO_BUILD_TARGET, so even
                // a wrong-host cargo emits the right-target wheel.
                let paths = SoldrPaths::new()?;
                // The Python PEP 517 backend must select the same effective
                // product root as this binary.  In particular, a development
                // soldr defaults to `.soldr-dev`; allowing the child to fall
                // back to its package-level `.soldr` default would mix
                // PEP517 target ownership across prod/dev daemons (#1763).
                command.env(crate::core::SOLDR_CACHE_DIR_ENV_VAR, &paths.root);
                let workspace_root =
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                let maturin_build = crate::pyo3_detect::maturin_args_are_build(tool_args);
                _maturin_build_lease = acquire_maturin_build_lease(&paths, tool_args)?;
                let maturin_target =
                    crate::pyo3_detect::resolve_build_target(tool_args, &workspace_root);
                if maturin_build {
                    let explicit_maturin =
                        std::env::var_os(MATURIN_USE_XWIN_ENV_VAR).map(|_| "set");
                    if let Some(policy) = maturin_xwin_policy(&maturin_target, explicit_maturin) {
                        command.env(MATURIN_USE_XWIN_ENV_VAR, policy);
                    }
                }
                let is_windows_host = crate::platform::host::facts::os()
                    == crate::platform::host::facts::HostOs::Windows;
                if maturin_build
                    && std::env::var_os("CARGO_BUILD_TARGET").is_none()
                    && (is_windows_host || maturin_target != crate::pyo3_detect::host_triple())
                {
                    command.env("CARGO_BUILD_TARGET", &maturin_target);
                }

                // Target OS/SDK preparation is orthogonal to Python ABI
                // policy. Direct maturin and PEP 517 builds receive the same
                // blessed target preparation as `soldr build` before the
                // PyO3 plan decides whether any Python variables are valid.
                if maturin_build && maturin_target != crate::pyo3_detect::host_triple() {
                    let target_prep =
                        crate::target_lifecycle::prepare_for_invocation(&paths, &maturin_target)
                            .await?;
                    crate::target_lifecycle::apply_to_process(&target_prep);
                    // Maturin forwards Cargo's unstable --config option.
                    // Preserve target-scoped build-script overrides just as
                    // the direct cargo lifecycle does.
                    crate::target_lifecycle::insert_args_before_separator(
                        &mut final_tool_args,
                        target_prep.cargo_args,
                    );
                }
                command.env(
                    crate::cache_lib::CACHE_ENABLED_ENV_VAR,
                    crate::cache_lib::cache_enabled_env_value(cache_enabled),
                );
                // soldr#2545 pre-spawn sweep: the wrapper policy below keys
                // on the inherited `RUSTC_WRAPPER`; if a Soldr-owned pair
                // drifted upstream, propagating it into a maturin/tool child
                // would bake the drift into that build. Fail first.
                crate::wrapper_identity::assert_inherited_wrapper_coherent("tool dispatch")?;
                if std::env::var_os("RUSTC_WRAPPER").is_none() {
                    if cache_enabled {
                        let wrapper_plan =
                            crate::zccache::prepare_rustc_wrapper_plan(&paths).await?;
                        wrapper_plan.apply_to_command(&mut command)?;
                    } else {
                        command.env_remove("RUSTC_WRAPPER");
                    }
                } else if cache_enabled {
                    crate::zccache::ZccacheChildEnv::from_current_process()?
                        .apply_to_command(&mut command);
                    // soldr#2451: the caller (e.g. the PEP 517 backend, which
                    // presets RUSTC_WRAPPER=soldr) owns the wrapper, so we must
                    // not override it — but the cargo children it spawns still
                    // re-enter soldr as that wrapper and resolve the broker
                    // daemon route by SOLDR_BROKER_SERVICE. The managed-plan
                    // branch above sets it; this caller-wrapper branch used to
                    // skip it, leaving a wheel-consumer build with no way to
                    // name the route (no sibling soldr-daemon beside the
                    // wrapper) — the "cannot resolve the broker daemon route
                    // (os error 2)" pep517-daemon-smoke failure. Register the
                    // daemon image and pass the service name down explicitly.
                    match crate::zccache::register_broker_daemon_service() {
                        Ok((_daemon, service_name)) => {
                            command.env(
                                crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR,
                                service_name,
                            );
                        }
                        Err(err) => eprintln!(
                            "soldr warning: could not register the broker daemon route for the \
                             caller-provided RUSTC_WRAPPER; cacheable compiles may fail: {err}"
                        ),
                    }
                }
                let mut prep = crate::blessed_build::BlessedPrep::default();
                crate::blessed_build::inject_cmake_tooling(&paths, &mut prep).await;
                // Mutate our own env (inherited by the child) so the
                // shim-dir PATH prepend below composes on top.
                for (k, v) in &prep.env {
                    std::env::set_var(k, v);
                }
                for dir in &prep.path_dirs {
                    prepend_to_path_env(dir);
                }

                if maturin_build {
                    pep517_paths = Some(paths.clone());
                    let state = crate::linker::apply_pep517_override(
                        &mut command,
                        &maturin_target,
                        &paths,
                    )?;
                    if state.cached_fallback {
                        eprintln!(
                            "soldr warning: fast linker `{}` was unavailable on the previous PEP 517 build; using the working standard linker",
                            state.candidate.as_deref().unwrap_or("unknown")
                        );
                    }
                    pep517_linker_state = Some(state);
                    let mut pyo3_plan = crate::pyo3_detect::resolve_for_invocation(
                        &workspace_root,
                        tool_args,
                        Some(&maturin_target),
                    );
                    pyo3_plan.materialize_compatibility(&paths).await?;
                    pyo3_plan.emit_diagnostic();
                    pyo3_plan.apply_to_command(&mut command);
                }
            }
            command.args(&final_tool_args);

            // Issue #493: when the user runs `soldr <external-tool>`,
            // install a transient PATH shim so any nested `cargo` /
            // `rustc` / `rustdoc` / `rustfmt` / `clippy-driver` spawned
            // by the tool routes back through soldr (and therefore
            // zccache and the managed toolchain home). The guard's
            // Drop removes the shim dir after the child exits.
            let _shim_guard = if shim_dir::should_install_shims() {
                match shim_dir::build_shim_dir() {
                    Ok(guard) => {
                        shim_dir::apply_to_command(&mut command, &guard.path);
                        Some(guard)
                    }
                    Err(err) => {
                        eprintln!(
                            "soldr warning: failed to build child shim dir; \
                             nested cargo/rustc calls will bypass soldr: {err}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            suppress_windows_console_window(&mut command);
            // soldr#2024: the child's output explains this exit, inherited
            // or teed back via `emit_child_output`.
            exit_guard::mark_spoke();
            let status = if let Some(state) = pep517_linker_state.as_ref() {
                if state.should_retry() || state.explicit_fast {
                    let first = command.output()?;
                    emit_child_output(&first);
                    if state.should_retry() && crate::linker::looks_like_linker_failure(&first) {
                        eprintln!(
                            "soldr warning: automatic fast linker `{}` failed; retrying once with the standard linker",
                            state.candidate.as_deref().unwrap_or("unknown")
                        );
                        state.clear_injected_env(&mut command);
                        let fallback = command.output()?;
                        emit_child_output(&fallback);
                        crate::linker::report_fallback_outcome(
                            &fallback,
                            pep517_paths.as_ref(),
                            state.cache_key.as_deref(),
                            state.candidate.as_deref().unwrap_or("unknown"),
                        );
                        fallback.status
                    } else {
                        if state.explicit_fast && crate::linker::looks_like_linker_failure(&first) {
                            eprintln!(
                                "soldr warning: explicitly requested SOLDR_LINKER=fast failed; no linker fallback was attempted"
                            );
                        }
                        first.status
                    }
                } else {
                    command.status()?
                }
            } else {
                command.status()?
            };

            let code = status.code().unwrap_or(1);
            if code != 0 {
                // soldr#1878: cargo surfaces a bare `Caused by:` with nothing
                // in it when the wrapped rustc dies without diagnostics. Say
                // which tool actually failed and where the full output went,
                // so the failure is never attributable to soldr by omission.
                eprintln!("soldr: {crate_name} exited {code}.");
            }
            // `process::exit` skips destructors, so anything still sitting in
            // a buffered stdout/stderr would be dropped here (soldr#1878).
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            guarded_exit(code);
        }
    }

    Ok(())
}
