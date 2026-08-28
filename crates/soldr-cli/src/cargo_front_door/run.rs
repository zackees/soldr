pub(crate) async fn run_cargo_front_door(
    args: &[String],
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<i32, SoldrError> {
    // Time the front door (#1843); a warm no-op does not invoke a rustc
    // wrapper. Zero-cost unless SOLDR_PROFILE_STARTUP is set.
    let mut profile = crate::startup_profile::WrapperProfile::new();

    if cargo_args_use_reserved_no_cache(args) {
        return Err(SoldrError::Other(
            "`--no-cache` must appear before `cargo`, as in `soldr --no-cache cargo build`".into(),
        ));
    }

    // Parse the opt-in watchdog before starting daemons, spawning Cargo, or
    // mutating build-session state. Malformed configuration is a user-facing
    // error, not a reason to launch a child that would need cleanup.
    let cargo_wait_timeout = cargo_wait_timeout()?;

    // soldr#2545 pre-spawn sweep: a front door nested inside a Soldr-owned
    // lineage (build scripts, tools re-invoking `soldr cargo`) must fail
    // here — before daemons start or cargo spawns — if the inherited
    // wrapper pair drifted, because cargo would fingerprint the changed
    // wrapper and silently recompile the world.
    crate::wrapper_identity::assert_inherited_wrapper_coherent("cargo front door")?;

    let trust_inherited_soldr_env =
        trust_inherited_soldr_env || env_flag_truthy(crate::TRUST_INHERITED_SOLDR_ENV_VAR);
    // The stable-rustc fallback re-enters this front door. Snapshot the
    // caller-facing contract before toolchain directives and Soldr-private
    // Cargo flags are normalized so the retry performs the same processing
    // exactly once.
    let zthreads_retry_context =
        ZthreadsRetryContext::new(args, cache_enabled, trust_inherited_soldr_env);
    let _fresh_workspace_env =
        FreshSoldrWorkspaceEnvGuard::apply_unless_trusted(trust_inherited_soldr_env);

    let cache_lifecycle = cache_lifecycle_from_env()?;
    let command_lifetime_shutdown_timeout = if cache_lifecycle == CacheLifecycle::Command {
        Some(command_lifetime_shutdown_timeout()?)
    } else {
        None
    };

    // Retain the old target-GC flags as stripped compatibility no-ops.
    let (args_without_dylint_cook_flag, dylint_dependency_cook) =
        strip_dylint_dependency_cook_flag(args);
    let args_owned = strip_no_gc_target_flags(&args_without_dylint_cook_flag);
    let (args_owned, explicit_toolchain) = subcommand::strip_cargo_toolchain_directive(&args_owned);
    let explicit_toolchain = explicit_toolchain.as_deref();
    let args: &[String] = &args_owned;

    // `cargo run` trampoline (issue #344). When the binary is already
    // up-to-date with the recorded sources, this exec's the binary
    // directly and never spawns cargo. Otherwise we get back a plan that
    // strips the soldr-private `--no-trampoline` flag from the arg list
    // and lets us refresh the sidecar after cargo succeeds.
    let trampoline_plan = if subcommand::is_cargo_run_invocation(args) {
        match try_run_trampoline(args)? {
            TrampolineDecision::Executed(code) => return Ok(code),
            TrampolineDecision::FellThrough(plan) => Some(plan),
        }
    } else {
        None
    };

    // Workspace build/check/clippy freshness belongs to Cargo. The retired
    // sidecar path did not model Cargo's complete semantic identity and could
    // return false Fresh results (#1528). Keep accepting the historical
    // soldr-only opt-out flag as argument-cleanup compatibility, but always
    // invoke Cargo for these verbs.
    let workspace_args = matches!(
        first_cargo_subcommand(args),
        Some("build" | "b" | "check" | "c" | "clippy")
    )
    .then(|| strip_no_trampoline_flag(args).0);

    // Use the cleaned arg vector from here on so `--no-trampoline` is
    // not forwarded to cargo.
    let owned_cleaned_args;
    let args: &[String] = match (trampoline_plan.as_ref(), workspace_args.as_ref()) {
        (Some(plan), _) => {
            owned_cleaned_args = plan.cleaned_args.clone();
            &owned_cleaned_args
        }
        (None, Some(cleaned)) => {
            owned_cleaned_args = cleaned.clone();
            &owned_cleaned_args
        }
        (None, None) => args,
    };

    let build_like_cargo = cargo_args_are_cacheable(args);
    if build_like_cargo {
        let repo_root = profile_debug::cargo_invocation_repo_path(args);
        line_endings::maybe_emit_crlf_warning(&repo_root);
    }
    profile.mark("crlf_warning");

    // soldr#2334: the bare `soldr cargo build --target <foreign>`
    // passthrough is contractually verbatim (CLAUDE.md two-build-paths),
    // so it does NOT route C dependencies through the managed target
    // toolchain — cc-built deps compile as host objects and the final
    // link fails with a wall of undefined references. When that shape is
    // detected with no routed/caller toolchain in scope, say so once and
    // name the blessed route instead of letting the link failure explain
    // itself badly.
    maybe_hint_foreign_target_passthrough(args);

    crate::toolchain::ensure_cargo_toolchain(explicit_toolchain)?;
    profile.mark("ensure_cargo_toolchain");
    let paths = SoldrPaths::new()?;
    paths.ensure_dirs()?;
    let dylint_requested = first_cargo_subcommand(args) == Some("dylint");
    let dylint_scope_already_active =
        std::env::var_os(crate::dylint_toolchain::TOOLCHAIN_ENV_VAR).is_some();
    // Only the process that introduces the Dylint scope owns the setup-soldr
    // success signal. Recursive cargo-dylint invocations inherit the scope but
    // must never publish completion for their parent.
    let dylint_entrypoint = dylint_requested && !dylint_scope_already_active;
    let dylint_scoped = dylint_requested || dylint_scope_already_active;
    if dylint_entrypoint {
        crate::dylint_toolchain::clear_success_marker()?;
    }
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // A missing cargo-dylint or dylint-link asset is a release-packaging
    // failure, not a reason to prepare a nightly toolchain. Resolve both
    // binaries first so unsupported hosts fail in seconds without downloads,
    // component probes, compiler work, or source-build fallback.
    let early_dylint = if dylint_entrypoint {
        let bootstrap = ensure_known_subcommand_tool(args, &paths).await?;
        let plan =
            crate::dylint_toolchain::resolve_plan(explicit_toolchain, &workspace_root).await?;
        crate::dylint_driver::ensure_prebuilt_driver(&plan, &paths).await?;
        Some((bootstrap, plan))
    } else {
        None
    };
    let dylint_plan = if let Some((_, plan)) = early_dylint.as_ref() {
        Some(crate::dylint_toolchain::prepare_resolved(plan.clone())?)
    } else if dylint_scoped {
        Some(crate::dylint_toolchain::prepare(explicit_toolchain, &workspace_root).await?)
    } else {
        None
    };
    let effective_toolchain = dylint_plan
        .as_ref()
        .map(|plan| plan.channel.as_str())
        .or(explicit_toolchain);
    let cargo = resolve_toolchain_binary_for_channel("cargo", effective_toolchain)?;
    let rustc = resolve_toolchain_binary_for_channel("rustc", effective_toolchain)?;
    // Deliberately uncached for the ambient default (`binaries.rs`), so this
    // is up to two `rustup which` subprocesses on every invocation.
    profile.mark("resolve_toolchain_binaries");
    let cargo_bin_dir = cargo
        .parent()
        .ok_or_else(|| SoldrError::Other("failed to resolve cargo bin directory".into()))?
        .to_path_buf();
    let existing_path = std::env::var_os("PATH");
    // Build the embedded-cache session plan on a background Tokio task while
    // the rest of the front-door pipeline performs known-subcommand fetch,
    // environment scrubbing, session-id stamping, target-registry
    // memoization, pre-GC, low-disk probing, profile_debug detection, and
    // linker injection. Since soldr#1368 this no longer downloads or extracts
    // a zccache binary; it prepares cache-root, rust-plan, and session state
    // for the service embedded in soldr-daemon. On warm builds the background
    // future resolves effectively immediately so the join at
    // `CargoCachePlan::finalize` is free.
    //
    // We intentionally spawn after the run-trampoline branch above because
    // that path exits without spawning cargo, and we don't
    // want to start a fetch we'll just drop. `cache_enabled` here is
    // the same flag the original synchronous `CargoCachePlan::prepare`
    // gated on; passing `false` produces a no-op `Disabled` prefetch.
    let cache_plan_prefetch = cache_plan::CargoCachePlanPrefetch::start(cache_enabled, &paths);

    // If the user invoked a known ecosystem subcommand (e.g. `cargo nextest`),
    // fetch the corresponding `cargo-<sub>` binary and prepend its directory to
    // PATH so cargo's subcommand dispatch finds it. Also collect transitive
    // bootstrap env (e.g. SDKROOT for explicit legacy
    // `cargo zigbuild --target *-apple-darwin`).
    let mut subcommand_tool_bootstrap = match early_dylint {
        Some((bootstrap, _)) => bootstrap,
        None => ensure_known_subcommand_tool(args, &paths).await?,
    };
    host_tooling::inject(args, &paths, &mut subcommand_tool_bootstrap).await;
    let owned_bootstrap_args;
    let args: &[String] = if subcommand_tool_bootstrap.cargo_args.is_empty() {
        args
    } else {
        owned_bootstrap_args =
            insert_cargo_global_args(args, &subcommand_tool_bootstrap.cargo_args);
        &owned_bootstrap_args
    };
    let extra_bin_dirs = subcommand_tool_bootstrap.bin_dirs;
    let transitive_env_overrides = subcommand_tool_bootstrap.env;
    // Compute env-var overrides keyed off the subcommand + its
    // --target argument. Today this fixes ring's build.rs on
    // `cargo xwin build --target *-pc-windows-msvc` by routing cc-rs
    // to `clang-cl` instead of the GNU-flavoured `clang`. See
    // `compute_subcommand_env_overrides` for the full rule set.
    let subcommand_env_overrides = compute_subcommand_env_overrides(args);

    let mut command = std::process::Command::new(&cargo);
    command.args(crate::target_alias::args_without_glibc_floor(args).iter());
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &cargo);
    suppress_windows_console_window(&mut command);
    // These Soldr control variables are consumed by this front-door
    // process. Letting Cargo inherit them leaks daemon lifecycle or retry
    // policy into build scripts and test binaries that may spawn nested Soldr.
    scrub_soldr_cache_lifecycle_env_for_child_cargo(&mut command);
    command.env_remove(zthreads_fallback::ATTEMPTED_ENV);
    if !trust_inherited_soldr_env {
        scrub_inherited_soldr_workspace_env_for_child_cargo(&mut command);
    }
    // soldr cargo is the top of the invocation tree, so any inherited
    // MAKEFLAGS/CARGO_MAKEFLAGS points at jobserver fds that aren't open in
    // our process. Stripping them lets cargo start a fresh jobserver instead
    // of printing the "failed to connect to jobserver" warning (see #283).
    command.env_remove("MAKEFLAGS");
    command.env_remove("CARGO_MAKEFLAGS");
    command.env("RUSTC", &rustc);

    // Issue #836 (sub of #835): pin the rust toolchain explicitly via
    // RUSTUP_TOOLCHAIN so rustup does NOT consult `rust-toolchain.toml`
    // on the cargo side and try to install the manifest's declared
    // `components = [...]` automatically.
    //
    // Why this matters in CI: many runner images (the GitHub-hosted
    // ubuntu-* lineage especially) ship a pre-existing `bin/cargo-fmt`
    // that conflicts with rustup's `rustfmt-preview` component install,
    // producing the well-known
    //
    //     error: failed to install component:
    //       'rustfmt-preview-x86_64-unknown-linux-gnu',
    //       detected conflict: 'bin/cargo-fmt'
    //
    // which kills the build before cargo even starts compiling. The
    // soldr bootstrap is supposed to short-circuit this — soldr itself
    // already knows the manifest channel (via
    // `read_rust_toolchain_manifest`), so passing it explicitly to
    // rustup with `RUSTUP_TOOLCHAIN` skips the manifest read on the
    // child cargo, and with it the entire auto-component-install path.
    //
    // Honor an explicit caller-set RUSTUP_TOOLCHAIN (don't clobber).
    // For users who genuinely need rustfmt / clippy at build time,
    // `soldr cargo fmt` / `clippy` still self-install via
    // `component_install::maybe_install_component_for_subcommand`.
    if let Some(toolchain) = explicit_toolchain {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    } else if std::env::var_os("RUSTUP_TOOLCHAIN").is_none() {
        let manifest_dir =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Ok(manifest) = crate::core::read_rust_toolchain_manifest(&manifest_dir) {
            if let Some(channel) = manifest.channel {
                let channel = channel.trim();
                if !channel.is_empty() {
                    command.env("RUSTUP_TOOLCHAIN", channel);
                }
            }
        }
    }
    if let Some(plan) = &dylint_plan {
        plan.apply_to_command(&mut command);
    }
    if dylint_dependency_cook {
        command.env_remove("RUSTC_WORKSPACE_WRAPPER");
        for (name, _) in std::env::vars_os() {
            let text = name.to_string_lossy();
            if text.starts_with("DYLINT_") || text == "ZCCACHE_DYLINT_CACHE_INPUT_HASH" {
                command.env_remove(name);
            }
        }
    }

    // Apply subcommand-derived env overrides (e.g. CC_<triple>=clang-cl
    // for `cargo xwin build --target *-pc-windows-msvc`). Honor a
    // caller-set value — don't clobber if the user already exported
    // their own CC / CXX / AR.
    for (key, value) in &subcommand_env_overrides {
        if std::env::var_os(key).is_none() {
            command.env(key, value);
        }
    }
    // Apply transitive-bootstrap env overrides (e.g. SDKROOT for explicit
    // legacy `cargo zigbuild --target *-apple-darwin`). These come from
    // `ensure_known_subcommand_tool` which calls into ensure_apple_sdk
    // / ensure_zig / etc. The functions themselves already gate on
    // `var_os` being unset before pushing, so just apply them.
    for (key, value) in &transitive_env_overrides {
        command.env(key, value);
    }

    emit_zig_cross_linker_preflight(&command, args)?;

    // Issue #824 follow-up: always engage RUSTC_WRAPPER + the zccache
    // session when caching is enabled, regardless of whether the cargo
    // subcommand is in our known-compiling set. The previous policy
    // (`cache_enabled && build_like_cargo`) silently dropped rustc
    // observations whenever soldr's classifier said "this subcommand
    // doesn't compile" — but build scripts, third-party cargo subcommand
    // plugins not yet in `known_tools`, and even some normally-non-
    // compiling verbs *can* re-shell to rustc through paths we don't
    // model. We always want zccache to see the call, then have zccache
    // itself decide whether to cache or pass through (its "non-cacheable"
    // classifier already handles read-only / non-hashable inputs).
    //
    // The trade-off is a small session-start/stop overhead (~hundreds of
    // ms) for cargo subcommands that don't end up spawning rustc — but
    // the observability win is "no rustc call goes unrecorded". The other
    // hooks (cook hydrate, disk watchdog, target-registry memo) still
    // gate on `build_like_cargo` because those have nothing to do with
    // rustc wrapping — they care about whether `target/` will be touched.
    let cache_enabled_for_cargo = cache_enabled;

    // Issue #597: auto-install rustup components for `soldr cargo {fmt,
    // clippy,miri}` when they're missing. Best-effort and silent on
    // failure — cargo's own error surfaces if the auto-install fails.
    // Honors SOLDR_NO_AUTO_COMPONENT=1.
    component_install::maybe_install_component_for_subcommand(args, &paths);

    // PR 3 (#578, meta #579): cross-repo cook-index pre-flight hydrate.
    // Best-effort — every failure path is silent so a missing daemon,
    // missing Cargo.lock, mismatched sha, or extract error never
    // breaks the cargo build. Only fires for build-like cargo
    // commands; `cargo metadata` / `cargo search` / etc. don't need
    // target/ to be populated.
    if build_like_cargo {
        cook_hydrate::maybe_hydrate(args, &paths, &rustc);
    }
    // Nothing here is memoized: a recursive walk hashing every Cargo.toml,
    // plus `rustc -V`, `git config --get remote.origin.url` and
    // `git branch --show-current` subprocesses, plus a daemon CookLookup.
    profile.mark("cook_hydrate");

    let cargo_subcommand = first_cargo_subcommand(args);
    let pyo3_build = matches!(
        cargo_subcommand,
        Some(
            "b" | "build"
                | "c"
                | "check"
                | "t"
                | "test"
                | "bench"
                | "d"
                | "doc"
                | "r"
                | "run"
                | "clippy"
                | "fix"
        )
    ) || cargo_subcommand == Some(concat!("rust", "c"));
    if build_like_cargo {
        // Cargo front door only: keep startup/low-disk warnings off unrelated
        // commands and out of the rustc-wrapper hot path.
        gc::emit_startup_target_warning_if_due();
    }
    profile.mark("gc_startup_warning");
    let dylint_shim_guard = if dylint_plan.is_some() && crate::shim_dir::should_install_shims() {
        Some(crate::shim_dir::build_dylint_shim_dir()?)
    } else {
        None
    };
    let mut path_dirs: Vec<std::path::PathBuf> = Vec::with_capacity(2 + extra_bin_dirs.len());
    if let Some(guard) = &dylint_shim_guard {
        path_dirs.push(guard.path.clone());
        command.env(crate::shim_dir::SOLDR_CHILD_SHIMS_ACTIVE_ENV_VAR, "1");
    }
    path_dirs.push(cargo_bin_dir);
    path_dirs.extend(extra_bin_dirs);
    command.env(
        "PATH",
        disk::prepend_paths(&path_dirs, existing_path.as_deref())?,
    );
    let _rustfmt_shim_guard =
        maybe_apply_rustfmt_zccache_shim(&mut command, args, cache_enabled_for_cargo);
    let explicit_target = target::default_cargo_build_target(args, dylint_requested)?;
    if let Some(target) = explicit_target.as_deref() {
        command.env("CARGO_BUILD_TARGET", target);
    }
    let known_cargo_target = target::known_cargo_build_target(args, explicit_target.as_deref());
    let cargo_profile_debug_default = if build_like_cargo {
        profile_debug::maybe_apply_cargo_profile_debug_default(
            &mut command,
            args,
            &paths,
            known_cargo_target.as_deref(),
        )?
    } else {
        None
    };
    // soldr#1610/#1614: every cargo-backed build surface consumes the
    // same target-aware PyO3 plan. The resolver is conservative: it only
    // injects PYO3_NO_PYTHON for a proven cross ABI3 extension, never for
    // embedding/non-ABI3 builds, and never downloads Python assets merely
    // because PyO3 appears in metadata.
    if pyo3_build {
        let workspace_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let mut pyo3_plan = crate::pyo3_detect::resolve_for_cargo_invocation(
            &workspace_root,
            args,
            known_cargo_target.as_deref(),
        );
        pyo3_plan.materialize_compatibility(&paths).await?;
        pyo3_plan.emit_diagnostic();
        pyo3_plan.apply_to_command(&mut command);
    }
    let native_cache_target = known_cargo_target.filter(|target| target.ends_with("-apple-darwin"));

    target::apply_linker_override(&mut command, args, explicit_target.as_deref(), &paths)?;

    // L3 (soldr#980): await the background zccache prefetch we kicked
    // off near the top of this function. Up until this point the cargo
    // command has been built without any wrapper env, so the prefetch
    // has been overlapping the entire setup pipeline. On a cold build
    // (binary not yet on disk) this is where the ~1-2 s saving falls
    // out — on a warm build the await is a near-no-op.
    //
    // Note: `cache_enabled_for_cargo` is currently `cache_enabled` (see
    // the comment above its assignment for the #824 follow-up
    // rationale). We thread it through `finalize` for symmetry with the
    // old synchronous API so that future divergence between the two
    // flags doesn't silently rewire the prefetch decision.
    let mut cache_plan =
        CargoCachePlan::finalize(cache_enabled_for_cargo, cache_plan_prefetch).await?;
    profile.mark("cache_plan_finalize");
    cache_plan.apply_to_command(&mut command, native_cache_target.as_deref())?;
    if dylint_plan.is_some() && cache_plan.uses_managed_zccache() {
        // Re-point the pair, not just RUSTC_WRAPPER: `apply_to_command`
        // above already stamped the rustc-shim identity into the
        // effective-wrapper mirror, and cargo-dylint re-enters the front
        // door (its nested `cargo metadata`), where a mismatched pair
        // fails the soldr#2545 drift guard. That failure is silent at
        // this level — dylint reports "No libraries were found" and
        // exits 0 having linted nothing (soldr#2634).
        crate::wrapper_identity::set_owned_rustc_wrapper(
            &mut command,
            crate::binaries::dylint_wrapper_shim_binary(&paths)?.as_os_str(),
            crate::wrapper_identity::WrapperOrigin::SoldrManaged,
        );
    }

    cache_plan.prepare_rust_artifact_plan(
        &cargo,
        &rustc,
        args,
        cargo_profile_debug_default.as_ref(),
        dylint_plan.as_ref().map(|plan| plan.channel.as_str()),
    )?;
    let capture_cargo_artifacts = build_like_cargo
        && cache_plan.has_rust_artifact_plan()
        && !cargo_args_have_message_format(args);
    if capture_cargo_artifacts {
        // Cargo's JSON stream is line-oriented and preserves rendered
        // diagnostics in the message payload. It lets us build an exact
        // package-aware closure while teeing the bytes unchanged below.
        command.arg("--message-format=json");
    }
    if build_like_cargo {
        let probe_path = cache_plan
            .target_dir_for_hooks(args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
        disk::maybe_emit_low_disk_warning(&probe_path);
        // Issue #574: host-volume disk watchdog. Distinct from the
        // legacy 2 GiB advisory above — this layer warns at 10 GiB and
        // aborts at 5 GiB so cross-repo target/ bloat surfaces before
        // the build sets the disk on fire. Returning Err here lets the
        // top-level dispatch print the error and exit with a non-zero
        // code (same path as any other SoldrError from the front door).
        let watchdog_path = cache_plan
            .target_dir_for_hooks(args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
        match gc::disk::check_disk_or_warn_or_block(&watchdog_path) {
            gc::disk::DiskCheckOutcome::Disabled | gc::disk::DiskCheckOutcome::Ok { .. } => {}
            gc::disk::DiskCheckOutcome::Warn {
                free_bytes,
                threshold_gib,
            } => {
                gc::disk::warn_and_reclaim(&watchdog_path, free_bytes, threshold_gib);
            }
            // soldr#2134: reclaim first, block only if that was not enough.
            gc::disk::DiskCheckOutcome::Block {
                free_bytes,
                threshold_gib,
            } => gc::disk::reclaim_then_block(&watchdog_path, free_bytes, threshold_gib)?,
        }
    }
    let restore_outcome = cache_plan.restore_rust_artifacts()?;

    // A preceding cached build may have materialized immutable outputs as
    // protected hardlinks to cache blobs. Whenever the finalized wrapper plan
    // has no embedded-cache session, detach shared target files locally
    // before the unmediated compiler can overwrite them. This must not depend
    // on the daemon being responsive. Conservatively include `install`:
    // configuration can select a persistent target root without a visible
    // command-line or environment override.
    if cargo_args_may_compile_unmediated(args) && cache_plan.zccache_session().is_none() {
        let report = no_cache_detach::prepare_target_for_unmediated_build(&cargo, args, &command)?;
        if report.detached_shared > 0 || report.made_writable > 0 {
            eprintln!(
                "soldr: no-cache preflight prepared {}: detached {} shared file(s), made {} private file(s) writable",
                report.target_dir.display(),
                report.detached_shared,
                report.made_writable,
            );
        }
    }
    // Two full recursive walks of target/ plus a `cargo metadata`
    // subprocess. Only runs without a zccache session (i.e. --no-cache).
    profile.mark("no_cache_detach");

    // Target-registry memoization for the wrapper hot path (#440).
    // Without this, every rustc invocation re-opens redb and writes
    // the same target row (~14 ms p50 on Windows in the issue #440
    // profile). The cargo front door runs once per build session and
    // already knows the target dir, so do the upsert here and
    // propagate a recorded-marker env var that lets the wrapper skip
    // its own redb work + daemon target-touch IPC.
    if build_like_cargo {
        let target_dir_for_memo: Option<std::path::PathBuf> = cache_plan.target_dir_for_hooks(args);
        if let Some(dir) = target_dir_for_memo.as_deref() {
            match scrub_cached_fallback_diagnostics_once(dir) {
                Ok(FallbackOutputScrub::AlreadyDone | FallbackOutputScrub::Complete(0)) => {}
                Ok(FallbackOutputScrub::DeferredForActiveBuild(_)) => {}
                Ok(FallbackOutputScrub::Complete(count)) => eprintln!(
                    "soldr: removed {count} stale compiler-cache fallback notice file(s) from {}",
                    dir.display()
                ),
                Err(error) => eprintln!(
                    "soldr warning: failed to remove stale compiler-cache fallback notices from {}: {error}",
                    dir.display()
                ),
            }
        }
    }

    // Capture build diagnostics and non-TTY #422/`-Zthreads` output.
    use std::io::IsTerminal;
    // `-Zthreads` also requires a diagnostic capture under a TTY.
    let capture_for_diagnostics = strip_diagnostics::should_capture(
        build_like_cargo,
        std::io::stderr().is_terminal(),
        zthreads_fallback::environment_mentions_zthreads(),
    );

    // Phase 2: start session correlation only after every fallible pre-cargo
    // preparation step (especially no-cache ownership detachment) succeeds.
    // From here, the cargo runner's success/error paths always pair this with
    // BuildSessionEnd and clear build_active, so a rejected preflight cannot
    // strand daemon maintenance in the "build active" state.
    let session_id = generate_build_session_id();
    command.env(
        crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR,
        session_id.to_string(),
    );
    let session_started_at_ms = current_unix_ms();
    let session_repo_root =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // soldr#1790: full invoked argv (the soldr binary + every arg),
    // captured once and reused by `write_always_on_build_log` at both the
    // cargo-run-error and normal-completion call sites below.
    let invoked_argv: Vec<String> = std::env::args().collect();
    let build_activity_lease = begin_build_activity_lease(&paths, session_id)?;
    profile.mark("build_activity_lease");
    // soldr#1843: publish BuildSessionStart concurrently with cargo (its ~740 ms IPC off the critical path), joined before BuildSessionEnd below.
    let session_publish = build_session::spawn_start_and_warn_on_jobs_drift(
        &paths,
        session_id,
        &session_repo_root,
        session_started_at_ms,
    );
    profile.mark("build_session_ipc_spawn");
    // soldr#1368 observability restore: snapshot the embedded zccache
    // compile counters just before cargo runs so `finish_zccache_session`
    // can diff start-vs-end into the per-build hit/miss summary written to
    // `last-session-stats.json`.
    if let Some(session) = cache_plan.zccache_session() {
        crate::cache::capture_build_baseline(&session.cache_dir, &session.session_id);
    }
    let compile_journal_start_len = file_len(&embedded_compile_journal_path(&paths));
    let compile_fallback_cursor = crate::compile_dispatch::compile_daemon_fallback_cursor(&paths);
    // soldr#2302: live per-unit HIT/MISS annotations (no-op for a --no-cache run).
    let cache_state_tail = cache_states::start_tail(&cache_plan, &paths, compile_journal_start_len);
    // Everything above is pure soldr overhead the user pays before Cargo
    // starts. Emit the breakdown here so the total excludes Cargo itself.
    profile.finish_labeled("cargo front door", "pre_spawn_tail");
    let cargo_run_result: CargoRunResult = if capture_cargo_artifacts {
        let target_dir = cache_plan
            .target_dir_for_hooks(args)
            .unwrap_or_else(|| disk::cargo_disk_space_probe_path(args));
        run_command_capturing_cargo_json(&mut command, &target_dir, cargo_wait_timeout)
            .map(|(status, captured, paths)| (status, Some(captured), Some(paths)))
    } else if capture_for_diagnostics && !debug_trace::observed_spawn_required() {
        // soldr#2546 slice 3: the diagnostic-tail capture observes
        // descendants by attaching the monitor to the spawned pid
        // post-hoc (running-process#1026), so on Unix the slice-2 trade —
        // which skipped this mode under --debug and with it the
        // post-failure diagnostics summary — is repaid: headless --debug
        // builds keep both the diagnostics and the descendant timeline.
        // Windows keeps the observed inherited-stdio spawn under --debug
        // instead: its descendant discovery is the Job Object wired at
        // spawn, so a post-hoc attach observes nothing there.
        run_command_capturing_diagnostic_tail(&mut command, cargo_wait_timeout)
            .map(|(status, captured)| (status, Some(captured), None))
    } else {
        // soldr#2546 slice 2: under --debug on a terminal, builds run
        // inherited-stdio through the running-process observer
        // (`with_observer_and_command`) so the timeline records
        // descendants without touching cargo's TTY output. The JSON
        // artifact-capture mode above keeps its load-bearing pipe
        // plumbing and observes via the same post-hoc attach.
        run_command_inheriting_stdio(&mut command, cargo_wait_timeout)
            .map(|status| (status, None, None))
    };
    // soldr#2302: cargo exited — drain + stop the per-unit tail before the tail
    // summary prints, on both the success and error paths.
    cache_states::stop_tail(cache_state_tail);
    // soldr#1843: BuildSessionStart must land before any BuildSessionEnd below.
    let _ = session_publish.join();
    let (status, diagnostic_capture, cargo_artifact_paths) = match cargo_run_result {
        Ok(outcome) => outcome,
        Err(err) => {
            let timeout = cargo_run_error_is_timeout(&err);
            let ended_at_ms = current_unix_ms();
            let daemon_finalized =
                crate::daemon::client::build_session_end(&paths, session_id, -1, ended_at_ms)
                    .is_ok();
            if !daemon_finalized {
                persist_build_session_end_fallback(&paths, session_id, -1, ended_at_ms);
            }
            let cleanup = cleanup_after_aborted_cargo_run(&cache_plan, args, timeout);
            let finish_result =
                cache_plan.finish_zccache_session(command_lifetime_shutdown_timeout);
            let build_log_paths = if let Some(session) = cache_plan.zccache_session() {
                persist_build_log_history(BuildLogHistoryRequest {
                    paths: &paths,
                    build_session_id: session_id,
                    repo_root: &session_repo_root,
                    started_at_ms: session_started_at_ms,
                    session,
                    compile_journal_start_len,
                    exit_code: -1,
                    ended_at_ms,
                    daemon_finalized,
                })
            } else {
                None
            };
            let build_log = write_always_on_build_log(
                &paths,
                session_id,
                &session_repo_root,
                &invoked_argv,
                session_started_at_ms,
                ended_at_ms,
                -1,
                compile_journal_start_len,
                &cargo,
                cache_plan.wrapper_identity(),
            );
            crate::cache_lib::build_active::set(false);
            drop(build_activity_lease);
            let compile_fallback_log =
                emit_compile_fallback_summary(&paths, &compile_fallback_cursor, session_id);
            // soldr#2302: whatever the cache managed before the abort.
            cache_states::emit_build_stats(&cache_plan);
            // soldr#1813: an aborted/timed-out cargo run is exactly when the
            // user most needs the log paths, and this arm always returns early —
            // so the summary is emitted here too rather than at the shared tail.
            log_summary::emit_session_log_summary(&log_summary::SessionLogs {
                build_log,
                build_log_paths,
                compile_fallback_log,
            });
            if let Err(finish_err) = finish_result {
                eprintln!(
                    "soldr warning: failed to finish zccache session after aborted cargo run: {finish_err}"
                );
            }
            let augmented = augment_aborted_cargo_error(err, cleanup, timeout);
            let auto_retry_planned =
                timeout && cargo_timeout_retry_allowed(cache_enabled_for_cargo, args);
            match append_cargo_abort_log(CargoAbortLogRequest {
                paths: &paths,
                session_id,
                repo_root: &session_repo_root,
                started_at_ms: session_started_at_ms,
                ended_at_ms,
                args,
                timeout,
                cargo_wait_timeout,
                cleanup,
                message: &augmented.to_string(),
                auto_retry_planned,
            }) {
                Ok(path) => eprintln!("soldr: cargo abort details written to {}", path.display()),
                Err(log_err) => {
                    eprintln!("soldr warning: failed to write cargo abort log: {log_err}")
                }
            }
            if auto_retry_planned {
                eprintln!(
                    "soldr: retrying timed-out cargo run without cache: soldr --no-cache cargo <same args>"
                );
                match retry_timed_out_cargo_without_cache(args, explicit_toolchain) {
                    Ok(status) => {
                        let code = status
                            .code()
                            .unwrap_or(if status.success() { 0 } else { 1 });
                        eprintln!("soldr: no-cache cargo retry exited with code {code}");
                        return Ok(code);
                    }
                    Err(retry_err) => {
                        return Err(SoldrError::Other(format!(
                            "{augmented}; no-cache retry failed: {retry_err}"
                        )));
                    }
                }
            }
            return Err(augmented);
        }
    };
    let captured_stderr_for_diagnosis = diagnostic_capture;
    let compile_fallback_log =
        emit_compile_fallback_summary(&paths, &compile_fallback_cursor, session_id);
    let strip_outcome = strip_diagnostics::StripOutcome::from_cargo(
        status.success(),
        captured_stderr_for_diagnosis.as_deref(),
    );
    let effective_exit_code = strip_outcome.effective_exit_code(&status);

    // Phase 2: send BuildSessionEnd before the success/failure
    // branches do any further work. Best-effort — never affects the
    // build's own outcome. soldr#1536: the daemon acknowledges once the
    // finalized aggregate and every session event are durable; on any
    // error we fall back to the direct-redb finalization below.
    let ended_at_ms = current_unix_ms();
    let daemon_finalized = crate::daemon::client::build_session_end(
        &paths,
        session_id,
        effective_exit_code,
        ended_at_ms,
    )
    .is_ok();
    if !daemon_finalized {
        persist_build_session_end_fallback(&paths, session_id, effective_exit_code, ended_at_ms);
    }
    let post_cargo_result: Result<(), SoldrError> = (|| {
        if status.success() && strip_outcome.permits_artifact_publication() {
            if let Some(paths) = cargo_artifact_paths.as_deref() {
                darwin_embed::embed_packed_dwarf_for_artifacts(
                    cache_plan.target_dir_for_hooks(args).as_deref(),
                    paths,
                )?;
                cache_plan.record_cargo_artifact_closure(paths, !paths.is_empty())?;
            }
            cache_plan.save_rust_artifacts(restore_outcome)?;
            if let Some(plan) = trampoline_plan.as_ref() {
                refresh_sidecar_after_cargo(plan);
            }
        } else if !status.success() {
            // A non-zero cargo exit can leave orphan `.rmeta` files (rmeta
            // emitted, then rustc aborted before the `.rlib` codegen pass)
            // in `target/<triple>/<profile>/deps/`. Subsequent invocations
            // then fail with `E0463: can't find crate` because cargo passes
            // `--extern X=orphan.rmeta` to dependents and rustc cannot link
            // an rmeta-only crate. Sweep them so the next build rebuilds
            // cleanly. See soldr#410.
            cache_plan.prune_orphan_rmetas_after_failed_build();
        }
        Ok(())
    })();

    // After cargo fails, look at whatever stderr we captured for a
    // recognizable build-script-spawn-ENOENT pattern (#422 — minimal
    // Rust containers without a host C toolchain). The capture
    // source is the diagnostic-tail buffer. TTY users captured
    // nothing — they see cargo's own error untouched and skip this
    // path.
    if !status.success() {
        if let Some(stderr_text) = captured_stderr_for_diagnosis.as_deref() {
            if let Some(diag) = crate::cargo_diagnostics::detect_build_script_failure(stderr_text) {
                let rendered = crate::cargo_diagnostics::render_diagnosis(&diag);
                let stderr = std::io::stderr();
                let _ = stderr.lock().write_all(rendered.as_bytes());
            }
        }
    }

    let finish_result = cache_plan.finish_zccache_session(command_lifetime_shutdown_timeout);
    let build_log_paths = if let Some(session) = cache_plan.zccache_session() {
        persist_build_log_history(BuildLogHistoryRequest {
            paths: &paths,
            build_session_id: session_id,
            repo_root: &session_repo_root,
            started_at_ms: session_started_at_ms,
            session,
            compile_journal_start_len,
            exit_code: effective_exit_code,
            ended_at_ms,
            daemon_finalized,
        })
    } else {
        None
    };
    let build_log = write_always_on_build_log(
        &paths,
        session_id,
        &session_repo_root,
        &invoked_argv,
        session_started_at_ms,
        ended_at_ms,
        effective_exit_code,
        compile_journal_start_len,
        &cargo,
        cache_plan.wrapper_identity(),
    );
    // soldr#2302: automatic cache-stats summary from the session baseline-diff
    // (precisely build-scoped), printed just above the log-paths block.
    cache_states::emit_build_stats(&cache_plan);
    // soldr#1813: tell the user where the logs went. Printed here because this
    // is the last point both the success and the compiler-failure paths pass
    // through — everything below can bail out via `?` or the zthreads retry.
    log_summary::emit_session_log_summary(&log_summary::SessionLogs {
        build_log,
        build_log_paths,
        compile_fallback_log,
    });
    // History is now copied, sanitized, indexed, and marked complete. Keep the
    // lease through that publication boundary so migration GC cannot remove a
    // half-written archive.
    crate::cache_lib::build_active::set(false);
    drop(build_activity_lease);
    if build_like_cargo {
        gc::maybe_spawn_auto_gc_sweeper(&paths);
    }
    finish_result?;
    post_cargo_result?;
    strip_outcome.into_result()?;
    if status.success() && dylint_entrypoint {
        if let Some(plan) = dylint_plan.as_ref() {
            crate::dylint_toolchain::write_success_marker(plan)?;
        }
    }
    if !status.success() {
        if let Some(plan) = zthreads_fallback::plan_from_environment() {
            if zthreads_fallback::diagnostic_matches(
                captured_stderr_for_diagnosis.as_deref().unwrap_or_default(),
            ) && !resolved_toolchain_is_nightly(explicit_toolchain)
            {
                emit_zthreads_fallback_warning(&plan.value);
                return retry_zthreads_without_flag(
                    &zthreads_retry_context,
                    explicit_toolchain,
                    &plan,
                );
            }
        } else if !env_flag_truthy(zthreads_fallback::ATTEMPTED_ENV)
            && zthreads_fallback::diagnostic_matches(
                captured_stderr_for_diagnosis.as_deref().unwrap_or_default(),
            )
        {
            eprintln!("{}", zthreads_fallback::render_config_hint());
        }
    }
    drop(trampoline_plan);
    Ok(status.code().unwrap_or(1))
}

/// soldr#2334: hint (never fail) when a foreign `--target` goes through
/// the verbatim cargo passthrough with no routed target C toolchain.
///
/// Fires only when every condition holds:
/// - the subcommand compiles (`build`/`b`/`test`/`t`/`bench`/`run`/`r`),
/// - an explicit `--target` names a triple that is not the host,
/// - no target-scoped C compiler is in scope: neither the blessed prep
///   (which exports `CC_<triple>` into the process before the front door
///   runs) nor a caller-managed override.
fn maybe_hint_foreign_target_passthrough(args: &[String]) {
    let compiles = matches!(
        first_cargo_subcommand(args),
        Some("build" | "b" | "test" | "t" | "bench" | "run" | "r")
    );
    if !compiles {
        return;
    }
    let Some(triple) = extract_target_arg(args) else {
        return;
    };
    if !foreign_target_passthrough_needs_hint(
        triple,
        crate::pyo3_detect::host_triple(),
        |key| std::env::var_os(key).is_some(),
    ) {
        return;
    }
    eprintln!(
        "soldr: note: `--target {triple}` through the bare cargo passthrough uses \
         whatever C toolchain cargo finds on this host, so cc-built dependencies \
         may compile as host objects and fail the final link (soldr#2334). The \
         blessed cross route is `soldr build --target {triple}`, which manages \
         the target C toolchain and sysroot."
    );
}

/// Pure decision core for the soldr#2334 hint, so the discrimination
/// matrix (host-native, blessed-prep, caller-managed, bare passthrough)
/// is unit-testable without process env.
fn foreign_target_passthrough_needs_hint(
    triple: &str,
    host_triple: &str,
    env_present: impl Fn(&str) -> bool,
) -> bool {
    if triple == host_triple {
        return false;
    }
    // Only hint for target families soldr actually manages a C toolchain
    // for — an exotic triple gets whatever cargo does today, unhinted.
    let managed_family = triple.ends_with("-unknown-linux-gnu")
        || triple.ends_with("-unknown-linux-musl")
        || triple.ends_with("-pc-windows-gnu")
        || triple.ends_with("-apple-darwin");
    if !managed_family {
        return false;
    }
    let suffix = triple.replace('-', "_");
    // Blessed prep exports CC_<triple>; callers doing it by hand set the
    // same var. Either way a routed toolchain is in scope: stay quiet.
    !env_present(&format!("CC_{suffix}"))
}

#[cfg(test)]
mod foreign_target_hint_tests {
    use super::foreign_target_passthrough_needs_hint as needs_hint;

    #[test]
    fn hint_matrix() {
        let host = "x86_64-unknown-linux-gnu";
        let none = |_: &str| false;
        // Foreign managed family, nothing routed: hint.
        assert!(needs_hint("x86_64-pc-windows-gnu", host, none));
        assert!(needs_hint("aarch64-unknown-linux-gnu", host, none));
        // Host-native: never.
        assert!(!needs_hint(host, host, none));
        // Unmanaged family: never.
        assert!(!needs_hint("wasm32-unknown-unknown", host, none));
        assert!(!needs_hint("x86_64-pc-windows-msvc", host, none));
        // Routed toolchain in scope (blessed prep or caller): quiet.
        let routed = |key: &str| key == "CC_x86_64_pc_windows_gnu";
        assert!(!needs_hint("x86_64-pc-windows-gnu", host, routed));
    }
}
