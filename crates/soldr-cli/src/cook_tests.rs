//! Unit tests for [`crate::cook`] — arg parsing, manifest discovery, and
//! the cargo-chef argv builders. Lives in a sibling file referenced via
//! `#[path]` so `cook.rs` stays under the 1000-LOC ceiling.

use super::*;

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

fn run_git_in(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?} failed in {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_git_repo_with_tracked_lock(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    run_git_in(repo, &["init", "-q", "-b", "main"]);
    run_git_in(repo, &["config", "user.email", "cook@example.com"]);
    run_git_in(repo, &["config", "user.name", "cook test"]);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"cook_index_no_daemon\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("Cargo.lock"), "# lockfile\n").unwrap();
    run_git_in(repo, &["add", "Cargo.toml", "Cargo.lock"]);
    run_git_in(repo, &["commit", "-q", "-m", "init"]);
}

#[test]
fn parse_cook_args_recognises_release_and_workspace_flags() {
    let parsed = parse_cook_args(&argv(&["--release", "--workspace"])).unwrap();
    assert!(parsed.release);
    assert!(parsed.workspace);
    assert!(parsed.profile.is_none());
    assert!(parsed.target.is_none());
    assert!(!parsed.keep_recipe);
    assert!(!parsed.prepare_only);
    assert!(!parsed.cook_only);
}

#[test]
fn parse_cook_args_parses_target_in_both_forms() {
    let space = parse_cook_args(&argv(&["--target", "x86_64-unknown-linux-musl"])).unwrap();
    assert_eq!(space.target.as_deref(), Some("x86_64-unknown-linux-musl"));
    let equals = parse_cook_args(&argv(&["--target=aarch64-apple-darwin"])).unwrap();
    assert_eq!(equals.target.as_deref(), Some("aarch64-apple-darwin"));
}

#[test]
fn parse_cook_args_parses_profile_in_both_forms() {
    let space = parse_cook_args(&argv(&["--profile", "ci"])).unwrap();
    assert_eq!(space.profile.as_deref(), Some("ci"));
    let equals = parse_cook_args(&argv(&["--profile=release-with-debug"])).unwrap();
    assert_eq!(equals.profile.as_deref(), Some("release-with-debug"));
}

#[test]
fn parse_cook_args_collects_packages() {
    let parsed = parse_cook_args(&argv(&["-p", "a", "--package", "b", "--package=c"])).unwrap();
    assert_eq!(parsed.packages, vec!["a", "b", "c"]);
}

#[test]
fn parse_cook_args_passthrough_after_double_dash() {
    let parsed = parse_cook_args(&argv(&[
        "--release",
        "--",
        "--features",
        "extra,fast",
        "--no-default-features",
    ]))
    .unwrap();
    assert!(parsed.release);
    assert_eq!(
        parsed.passthrough,
        vec!["--features", "extra,fast", "--no-default-features"]
    );
}

#[test]
fn parse_cook_args_rejects_unknown_flag_before_passthrough() {
    let err = parse_cook_args(&argv(&["--bogus"])).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown flag"));
    assert!(msg.contains("--bogus"));
}

#[test]
fn parse_cook_args_rejects_prepare_and_cook_only_together() {
    let err = parse_cook_args(&argv(&[
        "--prepare-only",
        "--cook-only",
        "--recipe-path",
        "r.json",
    ]))
    .unwrap_err();
    assert!(err.to_string().contains("mutually exclusive"));
}

#[test]
fn parse_cook_args_rejects_cook_only_without_recipe_path() {
    let err = parse_cook_args(&argv(&["--cook-only"])).unwrap_err();
    assert!(err.to_string().contains("--cook-only"));
    assert!(err.to_string().contains("--recipe-path"));
}

#[test]
fn parse_cook_args_keep_recipe_flag() {
    let parsed = parse_cook_args(&argv(&["--keep-recipe"])).unwrap();
    assert!(parsed.keep_recipe);
}

#[test]
fn parse_cook_args_no_trim_flag() {
    // Default is trim-on (issue #459).
    let default = parse_cook_args(&argv(&["--release"])).unwrap();
    assert!(!default.no_trim);
    let opt_out = parse_cook_args(&argv(&["--no-trim"])).unwrap();
    assert!(opt_out.no_trim);
}

#[test]
fn parse_cook_args_recipe_path_in_both_forms() {
    let space = parse_cook_args(&argv(&["--recipe-path", "/tmp/r.json"])).unwrap();
    assert_eq!(
        space.recipe_path.as_deref(),
        Some(std::path::Path::new("/tmp/r.json"))
    );
    let equals = parse_cook_args(&argv(&["--recipe-path=./out/recipe.json"])).unwrap();
    assert_eq!(
        equals.recipe_path.as_deref(),
        Some(std::path::Path::new("./out/recipe.json"))
    );
}

#[test]
fn build_chef_prepare_args_minimal() {
    let ctx = CookContext {
        manifest_dir: PathBuf::from("/proj"),
        recipe_path: PathBuf::from("/tmp/recipe.json"),
        recipe_owned_tempdir: true,
    };
    assert_eq!(
        build_chef_prepare_args(&ctx),
        vec!["chef", "prepare", "--recipe-path", "/tmp/recipe.json"]
    );
}

#[test]
fn build_chef_cook_args_release_workspace_target() {
    let ctx = CookContext {
        manifest_dir: PathBuf::from("/proj"),
        recipe_path: PathBuf::from("/tmp/recipe.json"),
        recipe_owned_tempdir: true,
    };
    let args = CookArgs {
        release: true,
        workspace: true,
        target: Some("x86_64-unknown-linux-musl".into()),
        ..Default::default()
    };
    let argv = build_chef_cook_args(&ctx, &args);
    assert!(argv.starts_with(&[
        "chef".to_string(),
        "cook".to_string(),
        "--recipe-path".to_string(),
        "/tmp/recipe.json".to_string(),
    ]));
    assert!(argv.iter().any(|a| a == "--release"));
    assert!(argv.iter().any(|a| a == "--workspace"));
    let target_pos = argv.iter().position(|a| a == "--target").unwrap();
    assert_eq!(argv[target_pos + 1], "x86_64-unknown-linux-musl");
}

#[test]
fn build_chef_cook_args_appends_passthrough_after_double_dash() {
    let ctx = CookContext {
        manifest_dir: PathBuf::from("/proj"),
        recipe_path: PathBuf::from("/tmp/recipe.json"),
        recipe_owned_tempdir: false,
    };
    let args = CookArgs {
        passthrough: vec!["--features".into(), "extra".into()],
        ..Default::default()
    };
    let argv = build_chef_cook_args(&ctx, &args);
    let sep = argv.iter().position(|a| a == "--").unwrap();
    assert_eq!(argv[sep + 1], "--features");
    assert_eq!(argv[sep + 2], "extra");
}

#[test]
fn build_chef_cook_args_profile_is_forwarded() {
    let ctx = CookContext {
        manifest_dir: PathBuf::from("/proj"),
        recipe_path: PathBuf::from("/tmp/recipe.json"),
        recipe_owned_tempdir: false,
    };
    let args = CookArgs {
        profile: Some("ci".into()),
        ..Default::default()
    };
    let argv = build_chef_cook_args(&ctx, &args);
    let pos = argv.iter().position(|a| a == "--profile").unwrap();
    assert_eq!(argv[pos + 1], "ci");
}

#[test]
fn build_chef_cook_args_packages_are_repeated() {
    let ctx = CookContext {
        manifest_dir: PathBuf::from("/proj"),
        recipe_path: PathBuf::from("/tmp/recipe.json"),
        recipe_owned_tempdir: false,
    };
    let args = CookArgs {
        packages: vec!["a".into(), "b".into()],
        ..Default::default()
    };
    let argv = build_chef_cook_args(&ctx, &args);
    let positions: Vec<usize> = argv
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--package")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(positions.len(), 2);
    assert_eq!(argv[positions[0] + 1], "a");
    assert_eq!(argv[positions[1] + 1], "b");
}

#[test]
fn resolve_manifest_dir_walks_up_to_find_cargo_toml() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0\"\n",
    )
    .unwrap();
    let sub = root.join("crates").join("inner");
    std::fs::create_dir_all(&sub).unwrap();
    let resolved = resolve_manifest_dir(&sub).unwrap();
    // Use canonical paths for resilience against Windows 8.3 short-paths
    // and symlinked tmpdirs (`/private/tmp` on macOS).
    let resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
    let root_canon = std::fs::canonicalize(root).unwrap_or(root.to_path_buf());
    assert_eq!(resolved, root_canon);
}

#[test]
fn resolve_manifest_dir_returns_error_when_no_manifest_above() {
    let tmp = tempfile::tempdir().unwrap();
    let err = resolve_manifest_dir(tmp.path()).unwrap_err();
    assert!(err.to_string().contains("no Cargo.toml"));
}

#[test]
fn build_cook_context_uses_explicit_recipe_path_relative_to_manifest_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0\"\n",
    )
    .unwrap();
    let args = CookArgs {
        recipe_path: Some(PathBuf::from("out/recipe.json")),
        ..Default::default()
    };
    let (ctx, guard) = build_cook_context(root, &args).unwrap();
    assert!(guard.is_none(), "explicit --recipe-path skips the tempdir");
    assert!(!ctx.recipe_owned_tempdir);
    assert!(ctx.recipe_path.ends_with("out/recipe.json"));
    assert!(ctx.recipe_path.is_absolute());
}

#[test]
fn build_cook_context_keep_recipe_drops_recipe_in_manifest_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0\"\n",
    )
    .unwrap();
    let args = CookArgs {
        keep_recipe: true,
        ..Default::default()
    };
    let (ctx, guard) = build_cook_context(root, &args).unwrap();
    assert!(guard.is_none());
    assert!(!ctx.recipe_owned_tempdir);
    assert_eq!(ctx.recipe_path.file_name().unwrap(), "recipe.json");
}

#[test]
fn build_cook_context_ephemeral_recipe_lives_in_owned_tempdir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0\"\n",
    )
    .unwrap();
    let args = CookArgs::default();
    let (ctx, guard) = build_cook_context(root, &args).unwrap();
    let _guard = guard.expect("ephemeral recipe must own a tempdir guard");
    assert!(ctx.recipe_owned_tempdir);
    assert!(ctx.recipe_path.ends_with("recipe.json"));
    assert!(ctx.recipe_path.is_absolute());
}

#[test]
fn cargo_chef_pin_constant_matches_known_tools_registry() {
    assert_eq!(
        crate::fetch::CARGO_CHEF_PINNED_VERSION,
        crate::fetch::lookup_by_crate("cargo-chef")
            .and_then(|s| s.pinned_version)
            .expect("cargo-chef must carry a pinned_version")
    );
}

#[test]
fn strip_generated_plugin_lines_only_removes_boolean_plugin_fields() {
    let input = "[[bin]]\nname = \"tool\"\nplugin = false\nproc-macro = false\n[package.metadata]\nplugin = \"keep\"\n[lib]\nplugin = true\n";

    let (sanitized, removed) = strip_generated_plugin_lines(input);

    assert_eq!(removed, 2);
    assert!(!sanitized.contains("plugin = false"));
    assert!(!sanitized.contains("plugin = true"));
    assert!(sanitized.contains("plugin = \"keep\""));
    assert!(sanitized.contains("proc-macro = false"));
}

#[test]
fn sanitize_cargo_chef_recipe_removes_generated_plugin_lines_from_manifests() {
    let tmp = tempfile::tempdir().unwrap();
    let recipe_path = tmp.path().join("recipe.json");
    let recipe = serde_json::json!({
        "skeleton": {
            "manifests": [
                {
                    "relative_path": "crates/a/Cargo.toml",
                    "contents": "[[bin]]\nname = \"a\"\nplugin = false\nproc-macro = false\n[[bench]]\nname = \"a_bench\"\nplugin = false\n[[test]]\nname = \"a_test\"\nplugin = false\n[lib]\nname = \"a\"\nplugin = false\n"
                },
                {
                    "relative_path": "crates/b/Cargo.toml",
                    "contents": "[package]\nname = \"b\"\n[package.metadata]\nplugin = \"user-data\"\n"
                }
            ]
        }
    });
    std::fs::write(&recipe_path, serde_json::to_vec(&recipe).unwrap()).unwrap();

    let removed = sanitize_cargo_chef_recipe(&recipe_path).unwrap();
    let updated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&recipe_path).unwrap()).unwrap();
    let manifests = updated["skeleton"]["manifests"].as_array().unwrap();
    let first = manifests[0]["contents"].as_str().unwrap();
    let second = manifests[1]["contents"].as_str().unwrap();

    assert_eq!(removed, 4);
    assert!(!first.contains("plugin = false"));
    assert!(first.contains("proc-macro = false"));
    assert!(second.contains("plugin = \"user-data\""));
}

#[test]
fn index_cooked_artifact_skips_archive_pack_when_daemon_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_git_repo_with_tracked_lock(&repo);
    let target_debug_deps = repo.join("target").join("debug").join("deps");
    std::fs::create_dir_all(&target_debug_deps).unwrap();
    std::fs::write(target_debug_deps.join("libdep.rlib"), b"dep").unwrap();

    let paths = SoldrPaths::with_root(tmp.path().join("soldr-cache"));
    let ctx = CookContext {
        manifest_dir: repo,
        recipe_path: tmp.path().join("recipe.json"),
        recipe_owned_tempdir: false,
    };
    let args = CookArgs {
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        ..Default::default()
    };
    let mut packer_called = false;

    index_cooked_artifact_with_packer(&ctx, &args, &paths, |_, _| {
        packer_called = true;
        panic!("packer must not run when CookLookup cannot reach the daemon")
    })
    .unwrap();

    assert!(
        !packer_called,
        "daemon-unavailable cook index path must not invoke archive packing"
    );
    assert!(
        !crate::cache_lib::cook_archive::cook_cache_dir(&paths)
            .join(".tmp")
            .exists(),
        "skipping the packer should leave no cook archive temp directory"
    );
}

// --- soldr#566: snapshot/restore the project around `cargo chef cook` ---

#[test]
fn snapshot_restore_undoes_cargo_chef_in_place_skeleton() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // A small workspace-ish project: root manifest + a member, with real
    // sources, plus target/ (cooked deps) and .git/ that must be preserved.
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("crates/inner/src")).unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let root_manifest = "[workspace.package]\nversion = \"1.2.3\"\n";
    let inner_manifest = "[package]\nname = \"inner\"\nversion.workspace = true\n";
    let main_rs = "fn main() {\n    println!(\"real binary\");\n}\n";
    let inner_lib = "pub fn answer() -> i32 {\n    42\n}\n";
    std::fs::write(root.join("Cargo.toml"), root_manifest).unwrap();
    std::fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();
    std::fs::write(root.join("src/main.rs"), main_rs).unwrap();
    std::fs::write(root.join("crates/inner/Cargo.toml"), inner_manifest).unwrap();
    std::fs::write(root.join("crates/inner/src/lib.rs"), inner_lib).unwrap();
    // A cooked-dep artifact + a generated .rs under target/ — must survive.
    std::fs::write(root.join("target/debug/libdep.rlib"), b"artifact").unwrap();
    std::fs::write(root.join("target/debug/out.rs"), b"// generated").unwrap();
    std::fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();

    let snapshot = snapshot_project_source(root).unwrap();
    // 4 source files captured (2 Cargo.toml + Cargo.lock + main.rs + lib.rs).
    assert_eq!(snapshot.len(), 5);

    // Simulate cargo-chef's in-place skeleton reconstruction.
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace.package]\nversion = \"0.0.1\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("crates/inner/Cargo.toml"),
        "[package]\nname = \"inner\"\nversion = \"0.0.1\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(root.join("crates/inner/src/lib.rs"), "").unwrap();
    // chef can add a spurious stub crate root that wasn't there originally.
    std::fs::write(root.join("crates/inner/src/main.rs"), "fn main() {}").unwrap();

    restore_project_source(root, &snapshot).unwrap();

    // Originals restored byte-for-byte.
    assert_eq!(
        std::fs::read_to_string(root.join("Cargo.toml")).unwrap(),
        root_manifest
    );
    assert_eq!(
        std::fs::read_to_string(root.join("crates/inner/Cargo.toml")).unwrap(),
        inner_manifest
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
        main_rs
    );
    assert_eq!(
        std::fs::read_to_string(root.join("crates/inner/src/lib.rs")).unwrap(),
        inner_lib
    );
    // Spurious chef-added crate root removed.
    assert!(!root.join("crates/inner/src/main.rs").exists());
    // target/ (cooked deps, incl. a generated .rs) and .git/ untouched.
    assert_eq!(
        std::fs::read(root.join("target/debug/libdep.rlib")).unwrap(),
        b"artifact"
    );
    assert!(root.join("target/debug/out.rs").exists());
    assert!(root.join(".git/HEAD").exists());
}

// ---------------------------------------------------------------------------
// #621 warm-cook marker round-trip
// ---------------------------------------------------------------------------

#[test]
fn cook_marker_round_trip_preserves_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".soldr-cook-marker.json");
    let marker = CookMarker {
        version: COOK_MARKER_VERSION,
        recipe_sha256: "deadbeef".repeat(8),
        rustc_version: "rustc 1.94.1 (abc 2025-12-25)".to_string(),
        soldr_version: "0.7.99".to_string(),
    };
    write_cook_marker(&path, &marker).unwrap();
    let read_back = read_cook_marker(&path).expect("marker round-trips");
    assert_eq!(read_back, marker);
}

#[test]
fn cook_marker_read_returns_none_for_version_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".soldr-cook-marker.json");
    let body = serde_json::json!({
        "version": 999, // wrong version
        "recipe_sha256": "x",
        "rustc_version": "x",
        "soldr_version": "x",
    });
    std::fs::write(&path, body.to_string()).unwrap();
    assert!(
        read_cook_marker(&path).is_none(),
        "marker with non-matching schema version must be ignored so we never short-circuit Phase 2 on stale data",
    );
}

#[test]
fn cook_marker_read_returns_none_for_missing_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".soldr-cook-marker.json");
    let body = serde_json::json!({
        "version": COOK_MARKER_VERSION,
        // missing recipe_sha256, rustc_version, soldr_version
    });
    std::fs::write(&path, body.to_string()).unwrap();
    assert!(read_cook_marker(&path).is_none());
}

#[test]
fn cook_marker_read_returns_none_for_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does-not-exist.json");
    assert!(read_cook_marker(&path).is_none());
}

#[test]
fn cook_marker_inequality_when_any_field_differs() {
    let a = CookMarker {
        version: COOK_MARKER_VERSION,
        recipe_sha256: "a".into(),
        rustc_version: "rustc 1".into(),
        soldr_version: "0.7.50".into(),
    };
    let mut b = CookMarker {
        version: a.version,
        recipe_sha256: a.recipe_sha256.clone(),
        rustc_version: a.rustc_version.clone(),
        soldr_version: a.soldr_version.clone(),
    };
    b.recipe_sha256 = "b".into();
    assert_ne!(a, b, "different recipe must NOT warm-skip");
    b.recipe_sha256 = a.recipe_sha256.clone();
    b.rustc_version = "rustc 2".into();
    assert_ne!(a, b, "different rustc must NOT warm-skip");
    b.rustc_version = a.rustc_version.clone();
    b.soldr_version = "0.7.51".into();
    assert_ne!(a, b, "different soldr must NOT warm-skip");
}
