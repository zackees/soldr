pub(crate) fn run_zccache_rust_plan(
    plan: &RustArtifactPlanContext,
    operation: &'static str,
    _include_session: bool,
) -> Result<zccache::artifact::RustPlanSummary, SoldrError> {
    // soldr#1368 + zccache#960: rust-plan save/restore runs in-process
    // against the compiled-in zccache artifact library — no `zccache
    // rust-plan` subprocess and no managed daemon. `gha`/`auto` use the
    // library's GHA backend (GitHub Actions cache) and fall back to local
    // when GHA is not configured (e.g. outside CI). The journal + session
    // id ride in the plan file itself, so the old `--journal` /
    // `--session-id` plumbing is dropped.
    if !matches!(operation, "save" | "restore") {
        return Err(SoldrError::Other(format!(
            "unknown rust-plan operation {operation:?}"
        )));
    }

    let loaded = zccache::artifact::RustArtifactPlanV1::load(&plan.path).map_err(|e| {
        SoldrError::Other(format!(
            "failed to load rust-plan {}: {e}",
            plan.path.display()
        ))
    })?;

    let run_local = |op: &str| match op {
        "save" => zccache::artifact::save_rust_plan_local(&loaded, &plan.cache_dir),
        _ => zccache::artifact::restore_rust_plan_local(&loaded, &plan.cache_dir),
    };

    let summary = if plan.backend == "gha" || plan.backend == "auto" {
        // GHA save/restore is async and `run_zccache_rust_plan` may run
        // inside a tokio runtime (the cargo front door), so run it on a
        // dedicated scoped thread with its own runtime — creating/entering
        // a runtime inline would panic ("cannot start a runtime from within
        // a runtime"). The scope keeps the `loaded`/`plan` borrows valid.
        let gha_res: Result<_, zccache::artifact::RustPlanGhaError> = std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        return Err(zccache::artifact::RustPlanGhaError::Failure(format!(
                            "failed to create runtime for gha rust-plan: {e}"
                        )))
                    }
                };
                match operation {
                    "save" => rt.block_on(zccache::artifact::save_rust_plan_gha(
                        &loaded,
                        &plan.cache_dir,
                    )),
                    _ => rt.block_on(zccache::artifact::restore_rust_plan_gha(
                        &loaded,
                        &plan.cache_dir,
                    )),
                }
            });
            match handle.join() {
                Ok(r) => r,
                Err(_) => Err(zccache::artifact::RustPlanGhaError::Failure(
                    "gha rust-plan worker thread panicked".to_string(),
                )),
            }
        });
        match gha_res {
            Ok(s) => s,
            // GHA not configured (no ACTIONS_CACHE_URL, e.g. local dev) —
            // fall back to the local backend, matching `auto` semantics.
            Err(zccache::artifact::RustPlanGhaError::Unavailable(msg)) => {
                eprintln!(
                    "soldr: rust-plan gha backend unavailable ({msg}); using local for {operation}."
                );
                run_local(operation).map_err(|e| {
                    SoldrError::Other(format!("zccache rust-plan {operation} failed: {e}"))
                })?
            }
            Err(e) => {
                return Err(SoldrError::Other(format!(
                    "zccache rust-plan gha {operation} failed: {e}"
                )))
            }
        }
    } else {
        run_local(operation)
            .map_err(|e| SoldrError::Other(format!("zccache rust-plan {operation} failed: {e}")))?
    };

    if let Ok(rendered) = serde_json::to_string(&summary) {
        eprintln!("soldr: zccache rust-plan {operation} summary");
        eprintln!("{rendered}");
    }
    if operation == "restore" {
        warn_if_rust_plan_restore_incomplete(&summary);
    }

    if operation == "save" && plan.cache_profile == Some("thin-v2") {
        // soldr#1538: `plan.cache_dir` is normally the *accumulated*
        // rust-plan cache root (`rust-plan-cache/`), shared across every
        // cache key ever saved locally (different profiles, target
        // triples, packages, ...). Walking + sorting it here on every save
        // — as this used to do unconditionally — re-walked every bundle
        // the cache root has ever seen, not just the bundle
        // `save_rust_plan_local` just produced, so the diagnostic manifest
        // step scaled with the lifetime size of the cache directory rather
        // than the current build.
        //
        // `docs/THIN_TARGET_CACHE_PRUNING.md` §5.1.b documents
        // `assert_thin_manifest.py <bundle_dir>/manifest.v2.json
        // <bundle_dir>` against a `SOLDR_TARGET_CACHE_BUNDLE_DIR`-pinned
        // directory — i.e. the documented/CI-verified contract is that
        // `plan.cache_dir` *is* the single bundle directory when the env
        // var is explicitly set, and that behavior (walk `plan.cache_dir`
        // as-is) is preserved unchanged here. Only the *default*,
        // unpinned, multi-key-accumulating cache dir is rescoped to the
        // bundle this save just produced.
        let manifest_root = if non_empty_env_path(TARGET_CACHE_BUNDLE_DIR_ENV_VAR).is_some() {
            plan.cache_dir.clone()
        } else {
            let cache_key = zccache::artifact::rust_plan_cache_key(&loaded);
            zccache::artifact::rust_plan_bundle_dir(&plan.cache_dir, &cache_key).into_path_buf()
        };
        if let Err(e) = write_thin_manifest(&manifest_root, plan.cache_profile) {
            // Manifest emission is diagnostic; never fail the build because
            // we could not write it. Log so it shows up in CI logs.
            eprintln!(
                "soldr warning: failed to write thin-slice manifest at {}: {e}",
                manifest_root.display()
            );
        }
    }
    Ok(summary)
}

// What `restore_rust_artifacts` did this invocation.
//
// The save path uses this outcome to avoid rewriting a warm rust-plan
// bundle when restore was skipped and the build produced no compile units.
