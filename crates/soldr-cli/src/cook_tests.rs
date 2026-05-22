//! Unit tests for [`crate::cook`] — arg parsing, manifest discovery, and
//! the cargo-chef argv builders. Lives in a sibling file referenced via
//! `#[path]` so `cook.rs` stays under the 1000-LOC ceiling.

use super::*;

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
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
