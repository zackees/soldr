async fn run_blessed_build(
    args: Vec<String>,
    cache_enabled: bool,
    trust_inherited_soldr_env: bool,
) -> Result<(), SoldrError> {
    // soldr#1012: prepare a recognized `--target X` / `--target=X`
    // through the blessed catalogue before entering the cargo front
    // door. This materializes the required SDK/sysroot and compiler
    // shims, then applies their target-scoped environment.
    // `SOLDR_USE_LEGACY_XWIN=1` opts out of the blessed path
    // and falls through to the explicit cargo-xwin flow.
    let mut full_args = Vec::with_capacity(args.len() + 1);
    full_args.push("build".to_string());
    full_args.extend(args);
    target_alias::normalize_target_aliases_in_args(&mut full_args);

    // Try to recognize a target from argv so we can prep
    // before invoking cargo. If the user didn't pass `--target`,
    // only the host-side prep (managed cmake/ninja) runs and we
    // forward otherwise unchanged.
    if let Some(target_triple) = extract_target_from_args(&full_args) {
        let paths = crate::core::SoldrPaths::new()?;
        // soldr#1543: start a bounded `cargo fetch
        // --target <T>` NOW so dependency acquisition overlaps
        // the catalogue/SDK materialization below. Joined
        // right after prep (before the front door spawns
        // cargo), so the two cargos never race; on a prep
        // error the child is reaped via kill_on_drop. Fetch
        // failures are logged + ignored — the main build owns
        // real dependency errors.
        let dep_prefetch =
            crate::fetch_overlap::spawn_for_blessed_build(&full_args, &target_triple);
        let prep =
            crate::target_lifecycle::prepare_for_invocation(&paths, &target_triple).await?;
        let cargo_args = prep.cargo_args.clone();
        crate::target_lifecycle::apply_to_process(&prep);

        // soldr#882/#1081/#1248: auto-dispatch cargo subcommand
        // based on target. Windows MSVC stays on plain cargo
        // build when blessed_build materialized the xwin-cache
        // and injected clang/lld/MSVC SDK env. Linux GNU/musl use the
        // managed-Zig env from target_lifecycle. Darwin stays on plain
        // cargo build after blessed_build injects the target-
        // shaped Apple SDK/clang env, unless
        // SOLDR_USE_LEGACY_ZIGBUILD=1 is set for diagnostic
        // comparison. cfg-gated to linux hosts — native
        // msvc/darwin host builds keep using plain cargo build.
        let msvc_blessed_cache_ready = prep.xwin_cache_dir.is_some();
        if let Some(subcmd) =
            pick_cross_subcommand(&target_triple, msvc_blessed_cache_ready)
        {
            full_args = rewrite_build_args_for_subcommand(full_args, subcmd);
        }
        full_args = insert_cargo_config_args(full_args, &cargo_args);

        // Join the overlapped dependency prefetch before the
        // main cargo build spawns (soldr#1543).
        if let Some(dep_prefetch) = dep_prefetch {
            dep_prefetch.join().await;
        }
    } else {
        // Native host build (no --target): the cross-compile
        // sysroot prep doesn't apply, but the managed cmake +
        // ninja injection does — cmake-based *-sys build
        // scripts run on the host regardless of target, and
        // "use whatever cmake/make PATH serves" is exactly the
        // failure mode soldr exists to remove (a pip-installed
        // MSYS make + "MSYS Makefiles" generator broke native
        // libz-ng-sys builds — see fetch::cmake_tools).
        let paths = crate::core::SoldrPaths::new()?;
        let mut prep = crate::blessed_build::BlessedPrep::default();
        crate::blessed_build::inject_cmake_tooling(&paths, &mut prep).await;
        for (k, v) in &prep.env {
            std::env::set_var(k, v);
        }
        for dir in &prep.path_dirs {
            prepend_to_path_env(dir);
        }
    }

    // soldr#1079: ensure native Windows MSVC builds get LIB /
    // INCLUDE / PATH (link.exe) injected from the host VS
    // install, so users invoking `soldr build` from a plain
    // PowerShell don't have to set `$env:LIB` themselves.
    // No-op when not on Windows, when the user opted out via
    // `SOLDR_MSVC_DISCOVERY=off`, when LIB is already set, or
    // when the resolved target is non-MSVC.
    ensure_msvc_host_env_for_native(&full_args).await;

    guarded_exit(
        cargo_front_door::run_cargo_front_door(
            &full_args,
            cache_enabled,
            trust_inherited_soldr_env,
        )
        .await?,
    );
}
