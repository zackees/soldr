//! Empirical regression tests for issue #1539: the plan's cross-worktree
//! content identity ([`compute_plan_content_identity`]) is path-independent
//! (invariant across `workspace_root` / `target_dir`), while divergence in
//! any real content input (toolchain, lockfile, manifests, target triple,
//! profile, features/env/rustflags) still forces a different identity. A
//! companion regression test proves the pre-existing, path-sensitive
//! warm-restore sentinel contract (`compute_plan_inputs_hash` combined with
//! the sentinel's separate `target_dir` field) was left unchanged.

use crate::rust_plan::{
    compute_plan_content_identity, compute_plan_inputs_hash, RustArtifactPlan, RustPlanInputs,
    RustPlanPackages, RustToolchainIdentity,
};

/// Builds a baseline plan for worktree `workspace_root`/`target_dir`. All
/// content-bearing fields (toolchain, triple, profile, inputs, packages,
/// artifact classes) are fixed so callers can vary exactly one field per
/// test case.
fn base_plan(workspace_root: &str, target_dir: &str) -> RustArtifactPlan {
    RustArtifactPlan {
        schema_version: 1,
        mode: "thin".to_string(),
        cache_profile: Some("thin-v2"),
        workspace_root: workspace_root.to_string(),
        target_dir: target_dir.to_string(),
        toolchain: RustToolchainIdentity {
            rustc: "rustc 1.94.1 (aaaaaaaaa 2026-01-01)".to_string(),
            cargo: "cargo 1.94.1 (bbbbbbbbb 2026-01-01)".to_string(),
            channel: "1.94.1".to_string(),
            host: "x86_64-unknown-linux-gnu".to_string(),
        },
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        profile: "release".to_string(),
        inputs: RustPlanInputs {
            features_hash: "features-hash-1".to_string(),
            rustflags_hash: "rustflags-hash-1".to_string(),
            env_hash: "env-hash-1".to_string(),
            lockfile_hash: "lockfile-hash-1".to_string(),
            cargo_config_hash: "cargo-config-hash-1".to_string(),
            manifest_hashes: vec!["manifest-hash-1".to_string(), "manifest-hash-2".to_string()],
        },
        packages: RustPlanPackages {
            selected_package_ids: vec!["registry+serde@1.0.0".to_string()],
            workspace_package_ids: vec!["path+app@0.1.0".to_string()],
            excluded_path_package_ids: vec![],
        },
        allowed_artifact_classes: vec!["dep_info", "cargo_fingerprint_meta"],
        dropped_artifact_classes: vec!["rlib", "rmeta"],
        cargo_artifact_paths: Vec::new(),
        cargo_artifacts_complete: false,
        cache_schema_version: 2,
        journal_log_path: None,
    }
}

// ---------------------------------------------------------------------------
// RED test 1: identical content, different worktree roots -> same identity.
// ---------------------------------------------------------------------------

crate::timed_test!(
    identical_content_across_sibling_worktrees_shares_content_identity,
    {
        let worktree_a = base_plan(
            "/repo/.claude/issue-1539",
            "/repo/.claude/issue-1539/target",
        );
        let worktree_b = base_plan(
            "/repo/.claude/issue-1539-sibling",
            "/repo/.claude/issue-1539-sibling/target",
        );

        assert_ne!(
            worktree_a.workspace_root, worktree_b.workspace_root,
            "test setup: worktree roots must differ"
        );
        assert_ne!(
            worktree_a.target_dir, worktree_b.target_dir,
            "test setup: target dirs must differ"
        );

        assert_eq!(
            compute_plan_content_identity(&worktree_a),
            compute_plan_content_identity(&worktree_b),
            "byte-identical plan content must produce the same cross-worktree \
             identity regardless of absolute workspace_root/target_dir"
        );
    }
);

// ---------------------------------------------------------------------------
// RED test 2: divergence in real content inputs forces a different identity.
// ---------------------------------------------------------------------------

crate::timed_test!(different_toolchain_forces_different_content_identity, {
    let base = base_plan("/repo/a", "/repo/a/target");
    let mut divergent = base_plan("/repo/a", "/repo/a/target");
    divergent.toolchain.rustc = "rustc 1.95.0 (ccccccccc 2026-02-01)".to_string();
    divergent.toolchain.channel = "1.95.0".to_string();

    assert_ne!(
        compute_plan_content_identity(&base),
        compute_plan_content_identity(&divergent),
        "a different toolchain identity must force a different content identity, \
         i.e. conservative fallback to a normal (non-shared) build"
    );
});

crate::timed_test!(different_lockfile_hash_forces_different_content_identity, {
    let base = base_plan("/repo/a", "/repo/a/target");
    let mut divergent = base_plan("/repo/a", "/repo/a/target");
    divergent.inputs.lockfile_hash = "lockfile-hash-DIFFERENT".to_string();

    assert_ne!(
        compute_plan_content_identity(&base),
        compute_plan_content_identity(&divergent),
        "a different Cargo.lock hash must force a different content identity"
    );
});

crate::timed_test!(different_manifest_hash_forces_different_content_identity, {
    let base = base_plan("/repo/a", "/repo/a/target");
    let mut divergent = base_plan("/repo/a", "/repo/a/target");
    divergent.inputs.manifest_hashes = vec!["manifest-hash-DIFFERENT".to_string()];

    assert_ne!(
        compute_plan_content_identity(&base),
        compute_plan_content_identity(&divergent),
        "a different manifest hash set must force a different content identity"
    );
});

crate::timed_test!(different_target_triple_forces_different_content_identity, {
    let base = base_plan("/repo/a", "/repo/a/target");
    let mut divergent = base_plan("/repo/a", "/repo/a/target");
    divergent.target_triple = "aarch64-apple-darwin".to_string();

    assert_ne!(
        compute_plan_content_identity(&base),
        compute_plan_content_identity(&divergent),
        "a different target triple must force a different content identity"
    );
});

crate::timed_test!(
    different_features_rustflags_env_hash_forces_different_content_identity,
    {
        let base = base_plan("/repo/a", "/repo/a/target");

        let mut different_features = base_plan("/repo/a", "/repo/a/target");
        different_features.inputs.features_hash = "features-hash-DIFFERENT".to_string();
        assert_ne!(
            compute_plan_content_identity(&base),
            compute_plan_content_identity(&different_features),
            "a different features hash must force a different content identity"
        );

        let mut different_rustflags = base_plan("/repo/a", "/repo/a/target");
        different_rustflags.inputs.rustflags_hash = "rustflags-hash-DIFFERENT".to_string();
        assert_ne!(
            compute_plan_content_identity(&base),
            compute_plan_content_identity(&different_rustflags),
            "a different rustflags hash must force a different content identity"
        );

        let mut different_env = base_plan("/repo/a", "/repo/a/target");
        different_env.inputs.env_hash = "env-hash-DIFFERENT".to_string();
        assert_ne!(
            compute_plan_content_identity(&base),
            compute_plan_content_identity(&different_env),
            "a different build-env hash must force a different content identity"
        );
    }
);

crate::timed_test!(
    build_script_sensitive_artifact_classes_conservatively_change_identity,
    {
        // Representative of "a path-baked build-script output being
        // conservatively excluded/rebuilt": if the plan's allowed/dropped
        // artifact-class set changes (e.g. because `build_script_output` is
        // pruned from the shareable slice for a unit proven to bake
        // absolute OUT_DIR paths), the content identity must change too,
        // so such units fall back to a normal, non-shared rebuild rather
        // than being silently treated as identical to a plan that still
        // ships that class.
        let base = base_plan("/repo/a", "/repo/a/target");
        let mut divergent = base_plan("/repo/a", "/repo/a/target");
        divergent.dropped_artifact_classes = vec!["rlib", "rmeta", "build_script_output"];

        assert_ne!(
            compute_plan_content_identity(&base),
            compute_plan_content_identity(&divergent),
            "excluding a path-sensitive artifact class must change the content identity"
        );
    }
);

// ---------------------------------------------------------------------------
// RED test 3: the pre-existing warm-restore sentinel hash is unchanged.
// ---------------------------------------------------------------------------

crate::timed_test!(
    warm_restore_plan_inputs_hash_stays_path_independent_and_equal_to_content_identity,
    {
        // `compute_plan_inputs_hash` (the warm-restore sentinel's content
        // half) was already path-independent before this change — the
        // sentinel's SAME-TREE guarantee comes from its separate
        // `target_dir` field, not from this hash. This test locks that
        // property so a future contributor cannot silently fold
        // `workspace_root`/`target_dir` into the sentinel hash's payload
        // (which would break both the sentinel's stability across
        // no-op path churn AND any future cross-worktree reuse of the
        // content identity that piggybacks on the same computation).
        let worktree_a = base_plan(
            "/repo/.claude/issue-1539",
            "/repo/.claude/issue-1539/target",
        );
        let worktree_b = base_plan(
            "/repo/.claude/issue-1539-sibling",
            "/repo/.claude/issue-1539-sibling/target",
        );

        assert_eq!(
            compute_plan_inputs_hash(&worktree_a),
            compute_plan_inputs_hash(&worktree_b),
            "warm-restore plan_inputs_hash must remain path-independent \
             (the sentinel gates on tree identity via a separate field)"
        );

        // The two hashes are deliberately computed from the same payload
        // today (see doc comment on `compute_plan_content_identity`), so
        // this also proves nothing in this change made the sentinel more
        // permissive than before.
        assert_eq!(
            compute_plan_inputs_hash(&worktree_a),
            compute_plan_content_identity(&worktree_a),
            "plan_inputs_hash and the new content identity must agree today"
        );

        // Content divergence still changes the sentinel hash exactly as it
        // did before this change (regression guard for the SAME-tree,
        // SAME-job short-circuit).
        let mut divergent = base_plan(
            "/repo/.claude/issue-1539",
            "/repo/.claude/issue-1539/target",
        );
        divergent.inputs.lockfile_hash = "lockfile-hash-DIFFERENT".to_string();
        assert_ne!(
            compute_plan_inputs_hash(&worktree_a),
            compute_plan_inputs_hash(&divergent),
            "warm-restore sentinel hash must still react to real content changes"
        );
    }
);
