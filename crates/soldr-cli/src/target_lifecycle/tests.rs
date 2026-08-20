//! Unit tests for the target-lifecycle argv helpers.
//!
//! Split out of `target_lifecycle.rs` (soldr#2493). Converting the retired
//! watchdog-macro call sites to plain `#[test] fn` costs one line per test,
//! which took that
//! file from 992 to 1008 lines -- past the 1000-line production ceiling
//! soldr#2493 itself introduced. The two test modules are the natural seam,
//! and the layout matches the sibling `cargo_front_door/tests.rs`.

use super::*;

use super::*;

#[test]
fn every_canonical_target_has_a_stable_capability_plan() {
    for target in crate::core::CANONICAL_TARGETS {
        let plan = plan(target).unwrap_or_else(|error| panic!("{target}: {error}"));
        assert!(plan.canonical, "{target}");
        assert_eq!(plan.schema_version, 1);
        assert_eq!(plan.canonical_target, *target);
        assert!(plan.canonical_alias.is_some(), "{target}");
        let expected = if *target == "x86_64-pc-windows-gnu" {
            vec![
                "prepare",
                "build",
                "clippy",
                "test-no-run",
                "nextest-archive",
            ]
        } else {
            vec![
                "prepare",
                "build",
                "clippy",
                "test-no-run",
                "nextest-archive",
                "pep517-wheel",
                "pep517-sdist",
            ]
        };
        assert_eq!(plan.supported_operations, expected);
        assert!(
            plan.environment
                .keys
                .windows(2)
                .all(|pair| pair[0] < pair[1]),
            "environment keys must be sorted and unique: {:?}",
            plan.environment.keys
        );
    }
}

#[test]
fn linux_arm64_plan_uses_catalogue_gnu_toolchain_without_zig() {
    let plan = plan_for_host("aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(plan.toolchain.family, "linux-gnu");
    assert_eq!(plan.toolchain.linker, "managed gcc");
    assert_eq!(plan.platform.kind, "gnu-linux-sysroot");
    assert_eq!(plan.platform.provider, "soldr-toolchain");
    assert!(plan.cache_identity.contains("gnu-linux-toolchain/"));
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        !json.contains("zig"),
        "normal GNU plans must not advertise Zig: {json}"
    );
    assert!(!json.contains("cargo-zigbuild"));
}

// soldr#2309: the linux-gnu plan advertises the C++ stdlib pin; every
// other family stays unchanged (musl/windows/darwin ship no such pin).
#[test]
fn only_linux_gnu_plans_advertise_the_cxx_stdlib_pin() {
    let host = "x86_64-unknown-linux-gnu";
    let gnu = plan_for_host("aarch64-unknown-linux-gnu", host).unwrap();
    assert!(gnu
        .environment
        .keys
        .contains(&"CXXSTDLIB_aarch64_unknown_linux_gnu".to_string()));
    assert!(gnu.environment.keys.contains(&"CXXSTDLIB".to_string()));
    for other in [
        "aarch64-unknown-linux-musl",
        "x86_64-pc-windows-msvc",
        "aarch64-apple-darwin",
    ] {
        let plan = plan_for_host(other, host).unwrap();
        assert!(
            !plan
                .environment
                .keys
                .iter()
                .any(|key| key.starts_with("CXXSTDLIB")),
            "{other} must not carry a C++ stdlib pin: {:?}",
            plan.environment.keys
        );
    }
}

#[test]
fn linux_musl_plan_uses_catalogue_toolchain_without_zig() {
    let plan = plan_for_host("aarch64-unknown-linux-musl", "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(plan.toolchain.family, "linux-musl");
    assert_eq!(plan.toolchain.linker, "managed gcc");
    assert_eq!(plan.platform.kind, "musl-linux-sysroot");
    assert_eq!(plan.platform.provider, "soldr-toolchain");
    assert!(plan.cache_identity.contains("musl-linux-toolchain/"));
    let json = serde_json::to_string(&plan).unwrap();
    assert!(
        !json.contains("zig"),
        "normal musl plans must not advertise Zig: {json}"
    );
    assert!(!json.contains("cargo-zigbuild"));
}

// soldr#2519 deleted `host_native_gnu_uses_catalogue_toolchain` with the
// helper it called. Its point -- that a GNU target never inherits the
// runner's linker via the Zig fallback -- is now structural rather than
// conditional: there is no Zig fallback to reach. The catalogue assertions
// below cover what a GNU target actually gets.

#[test]
fn the_reported_linux_plans_match_the_catalogue_lifecycle() {
    let plan = plan_for_host("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(plan.toolchain.linker, "managed gcc");
    assert_eq!(plan.platform.provider, "soldr-toolchain");
    assert_eq!(plan.platform.kind, "gnu-linux-sysroot");

    let musl = plan_for_host("x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl").unwrap();
    assert_eq!(musl.toolchain.linker, "managed gcc");
    assert_eq!(musl.platform.provider, "soldr-toolchain");
    assert_eq!(musl.platform.kind, "musl-linux-sysroot");
}

// soldr#2519 deleted `only_explicit_legacy_musl_can_reach_zig_fallback` and
// `normal_musl_never_reaches_legacy_zig` with the helper they called. There
// is no longer an explicit-legacy branch for them to distinguish: the Zig
// musl fallback and its `SOLDR_USE_LEGACY_ZIGBUILD` gate are both gone, so
// every musl target takes the catalogue path asserted just above.

#[test]
fn unsupported_noncanonical_targets_have_no_capability_plan() {
    let error = plan("i686-unknown-linux-gnu").unwrap_err();
    assert!(error.to_string().contains("i686"));
}

#[test]
fn required_msvc_flags_merge_with_target_and_project_flags() {
    let required = "-C link-arg=/LIBPATH:/soldr/sdk";
    let merged = merge_flag_values(
        required,
        Some("-C target-cpu=x86-64-v3"),
        Some("-Dwarnings"),
    );
    assert!(merged.contains("/LIBPATH:/soldr/sdk"));
    assert!(merged.contains("target-cpu=x86-64-v3"));
    assert!(merged.contains("-Dwarnings"));
}

#[test]
fn required_c_flags_merge_with_project_flags() {
    let merged = merge_flag_values(
        "/imsvc/soldr/sdk/include",
        Some("-DPROJECT_TARGET=1"),
        Some("-DPROJECT_GLOBAL=1"),
    );
    assert!(merged.contains("/imsvc/soldr/sdk/include"));
    assert!(merged.contains("-DPROJECT_TARGET=1"));
    assert!(merged.contains("-DPROJECT_GLOBAL=1"));
}

#[test]
fn encoded_project_rustflags_keep_required_linker_search_path() {
    let merged = merge_encoded_rustflags(
        "-Dwarnings\u{1f}-C\u{1f}target-cpu=x86-64-v3",
        "-C link-arg=/LIBPATH:/soldr/sdk",
        None,
        Some("-C target-feature=+crt-static"),
    );
    let tokens: Vec<_> = merged.split('\u{1f}').collect();
    assert!(tokens.contains(&"-Dwarnings"));
    assert!(tokens.contains(&"target-cpu=x86-64-v3"));
    assert!(tokens.contains(&"target-feature=+crt-static"));
    assert!(tokens.contains(&"link-arg=/LIBPATH:/soldr/sdk"));
    assert_eq!(
        tokens.iter().filter(|token| **token == "-C").count(),
        3,
        "each encoded codegen option retains its -C prefix"
    );
}

#[test]
fn encoded_required_flags_preserve_paths_with_spaces() {
    let merged = merge_encoded_rustflags(
        "",
        "-C linker-flavor=lld-link -C link-arg=/LIBPATH:/soldr sdk/lib",
        None,
        None,
    );
    let tokens: Vec<_> = merged.split('\u{1f}').collect();
    assert!(tokens.contains(&"link-arg=/LIBPATH:/soldr sdk/lib"));
}

#[test]
fn applying_target_flags_consumes_higher_precedence_globals() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let target_key = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS";
    let names = [target_key, "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"];
    let previous = names.map(std::env::var_os);
    std::env::remove_var(target_key);
    std::env::set_var("RUSTFLAGS", "-Dwarnings");
    std::env::set_var("CARGO_ENCODED_RUSTFLAGS", "-C\u{1f}target-cpu=x86-64-v3");
    let mut prep = BlessedPrep::default();
    prep.env.push((
        target_key.to_string(),
        "-C link-arg=/LIBPATH:/soldr/sdk".to_string(),
    ));

    apply_to_process(&prep);
    let rustflags = std::env::var_os("RUSTFLAGS");
    let encoded = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap();
    for (name, value) in names.into_iter().zip(previous) {
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }

    assert!(rustflags.is_none());
    let tokens: Vec<_> = encoded.split('\u{1f}').collect();
    assert!(tokens.contains(&"-Dwarnings"));
    assert!(tokens.contains(&"target-cpu=x86-64-v3"));
    assert!(tokens.contains(&"link-arg=/LIBPATH:/soldr/sdk"));
}

#[test]
fn all_compiling_cargo_surfaces_enter_the_lifecycle() {
    for command in [
        "build --target linux-arm64",
        "clippy --target linux-arm64",
        "test --no-run --target linux-arm64",
        "nextest archive --target linux-arm64",
    ] {
        let args = command
            .split_ascii_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(cargo_operation_requires_prep(&args), "{command}");
    }
    let clean = ["clean", "--target", "linux-arm64"].map(str::to_string);
    assert!(!cargo_operation_requires_prep(&clean));
}
