//! One authoritative lifecycle for soldr's canonical cross targets.
//!
//! Every compiling surface calls [`prepare_target`].  This keeps compiler,
//! linker, SDK/sysroot, Rust standard-library, environment merging, and
//! machine-readable capabilities keyed only by the requested target triple.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::blessed_build::BlessedPrep;
use crate::core::{SoldrError, SoldrPaths};
use crate::prepare_cmd::{classify_target, TargetAbi, TargetOs};

pub(crate) const TARGET_PLAN_SCHEMA_VERSION: u64 = 1;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct TargetPlan {
    pub(crate) schema_version: u64,
    pub(crate) canonical: bool,
    pub(crate) canonical_target: String,
    pub(crate) canonical_alias: Option<String>,
    pub(crate) toolchain: ToolchainPlan,
    pub(crate) platform: PlatformPlan,
    pub(crate) environment: EnvironmentPlan,
    pub(crate) cache_identity: String,
    pub(crate) supported_operations: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ToolchainPlan {
    pub(crate) family: &'static str,
    pub(crate) c_compiler: &'static str,
    pub(crate) cxx_compiler: &'static str,
    pub(crate) linker: &'static str,
    pub(crate) archiver: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PlatformPlan {
    pub(crate) kind: &'static str,
    pub(crate) provider: &'static str,
    pub(crate) identity: String,
    pub(crate) root_env: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct EnvironmentPlan {
    pub(crate) keys: Vec<String>,
    pub(crate) path_prepend: bool,
}

pub(crate) async fn prepare_target(
    paths: &SoldrPaths,
    target: &str,
) -> Result<BlessedPrep, SoldrError> {
    let attrs = classify_target(target)?;
    let host = crate::pyo3_detect::host_triple();
    crate::prepare_cmd::rustup_add_target(target)?;
    let mut prep = crate::blessed_build::prepare(paths, target).await?;

    let legacy_xwin = std::env::var_os(crate::blessed_build::USE_LEGACY_XWIN_ENV_VAR)
        .is_some_and(|value| !value.is_empty() && value != "0");
    if cfg!(target_os = "linux")
        && attrs.os == TargetOs::Windows
        && attrs.abi == Some(TargetAbi::Msvc)
        && prep.xwin_cache_dir.is_none()
        && !legacy_xwin
    {
        return Err(SoldrError::Other(format!(
            "blessed MSVC SDK preparation did not produce a cache for {target}; \
             cargo-xwin is available only through the diagnostic \
             {}=1 override",
            crate::blessed_build::USE_LEGACY_XWIN_ENV_VAR
        )));
    }

    let legacy_zigbuild = std::env::var_os(crate::blessed_build::USE_LEGACY_ZIGBUILD_ENV_VAR)
        .is_some_and(|value| !value.is_empty() && value != "0");
    if attrs.os == TargetOs::Darwin && target != host && prep.sdkroot.is_none() && !legacy_zigbuild
    {
        return Err(SoldrError::Other(format!(
            "blessed Apple SDK preparation did not produce an SDK for {target}; \
             the legacy wrapper is available only through the diagnostic \
             {}=1 override",
            crate::blessed_build::USE_LEGACY_ZIGBUILD_ENV_VAR
        )));
    }

    if should_prepare_managed_linux(attrs.os, target, host, legacy_zigbuild) {
        let tools = crate::linux_cross::prepare(paths, target).await?;
        prep.path_dirs.push(tools.bin_dir);
        let suffix = target.replace('-', "_");
        let upper = suffix.to_ascii_uppercase();
        prep.env.extend([
            (
                format!("CC_{suffix}"),
                tools.cc.to_string_lossy().into_owned(),
            ),
            (
                format!("CXX_{suffix}"),
                tools.cxx.to_string_lossy().into_owned(),
            ),
            (
                format!("AR_{suffix}"),
                tools.ar.to_string_lossy().into_owned(),
            ),
            (
                format!("RANLIB_{suffix}"),
                tools.ranlib.to_string_lossy().into_owned(),
            ),
            (
                format!("CARGO_TARGET_{upper}_LINKER"),
                tools.linker.to_string_lossy().into_owned(),
            ),
            (
                format!("CARGO_TARGET_{upper}_RUSTFLAGS"),
                "-C link-self-contained=no".to_string(),
            ),
        ]);
    }
    Ok(prep)
}

/// Opt out of routing a **host-native** `-gnu` build through managed
/// zig, restoring the pre-soldr#2145 behaviour of linking against the
/// host's own glibc. Falsy values (`0`/`false`/`no`/`off`) disable.
pub(crate) const NATIVE_GNU_LINK_ENV_VAR: &str = "SOLDR_NATIVE_GNU_LINK";

fn should_prepare_managed_linux(
    os: TargetOs,
    target: &str,
    host: &str,
    legacy_zigbuild: bool,
) -> bool {
    if os != TargetOs::Linux || legacy_zigbuild {
        return false;
    }
    if target != host {
        return true;
    }
    // soldr#2145 / soldr#1060 item 3. A host-native `-gnu` build skipped
    // this arm entirely and linked against whatever glibc the machine
    // happens to run — 2.39 on the `ubuntu-24.04` release runner, which
    // is the floor the published x86_64 artifact inherited. That is the
    // most-downloaded Linux artifact and it was 11 glibc versions worse
    // than the aarch64 one, purely because aarch64 is cross-built here
    // and x86_64 is not.
    //
    // Routing it through the same managed zig the cross lanes use gives
    // it the same floor (2.28, zig's default for the target) without any
    // change to how it is built. musl is untouched: it is statically
    // linked, so it has no glibc floor to improve.
    target.ends_with("-unknown-linux-gnu") && native_gnu_link_enabled()
}

fn native_gnu_link_enabled() -> bool {
    match std::env::var_os(NATIVE_GNU_LINK_ENV_VAR) {
        None => true,
        Some(value) => {
            let raw = value.to_string_lossy();
            let trimmed = raw.trim();
            !(trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("0")
                || trimmed.eq_ignore_ascii_case("false")
                || trimmed.eq_ignore_ascii_case("no")
                || trimmed.eq_ignore_ascii_case("off"))
        }
    }
}

/// Preserve Cargo's custom target-spec passthrough while using the unified
/// lifecycle for every target family soldr recognizes.
pub(crate) async fn prepare_for_invocation(
    paths: &SoldrPaths,
    target: &str,
) -> Result<BlessedPrep, SoldrError> {
    if classify_target(target).is_ok() {
        prepare_target(paths, target).await
    } else {
        crate::blessed_build::prepare(paths, target).await
    }
}

pub(crate) async fn prepare_cargo_invocation(
    mut args: Vec<String>,
) -> Result<Vec<String>, SoldrError> {
    crate::target_alias::normalize_target_aliases_in_args(&mut args);
    if !cargo_operation_requires_prep(&args) {
        return Ok(args);
    }
    let Some(target) = crate::cli_dispatch::extract_target_from_args(&args) else {
        return Ok(args);
    };
    if !crate::core::is_canonical(&target) {
        return Ok(args);
    }
    let paths = SoldrPaths::new()?;
    let prep = prepare_target(&paths, &target).await?;
    apply_to_process(&prep);
    Ok(crate::cli_dispatch::insert_cargo_config_args(
        args,
        &prep.cargo_args,
    ))
}

fn cargo_operation_requires_prep(args: &[String]) -> bool {
    matches!(
        crate::cargo_front_door::first_cargo_subcommand(args),
        Some("build" | "check" | "clippy" | "test" | "bench" | "run" | "doc" | "nextest")
    )
}

/// Resolve target-scoped environment values while preserving project flags.
///
/// `apply_to_process` additionally promotes Rust flags into Cargo's encoded
/// highest-precedence form and clears the lower-precedence global variable.
/// Export-only preparation folds encoded flags into the target key instead.
pub(crate) fn resolved_env(prep: &BlessedPrep) -> Vec<(String, String)> {
    prep.env
        .iter()
        .map(|(key, required)| {
            let value = if let Some(global_key) = flag_global_key(key) {
                let existing = std::env::var(key).ok();
                let mut global = std::env::var(global_key).ok();
                if global_key == "RUSTFLAGS" {
                    if let Some(encoded) = std::env::var("CARGO_ENCODED_RUSTFLAGS")
                        .ok()
                        .filter(|value| !value.is_empty())
                    {
                        let decoded = encoded.replace('\u{1f}', " ");
                        global = Some(merge_flag_values(
                            global.as_deref().unwrap_or(""),
                            None,
                            Some(&decoded),
                        ));
                    }
                }
                merge_flag_values(required, existing.as_deref(), global.as_deref())
            } else {
                required.clone()
            };
            (key.clone(), value)
        })
        .collect()
}

pub(crate) fn apply_to_process(prep: &BlessedPrep) {
    let encoded_rustflags = encoded_rustflags_for_prep(prep);
    for (key, value) in resolved_env(prep) {
        std::env::set_var(key, value);
    }
    if let Some(encoded) = encoded_rustflags {
        std::env::set_var("CARGO_ENCODED_RUSTFLAGS", encoded);
        std::env::remove_var("RUSTFLAGS");
    }
    crate::prepend_path_dirs_to_env(&prep.path_prefix());
}

pub(crate) fn insert_args_before_separator(args: &mut Vec<String>, extra: Vec<String>) {
    let insertion = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.splice(insertion..insertion, extra);
}

pub(crate) fn resolve_prepare_targets(
    target: &str,
    exporting_environment: bool,
) -> Result<Vec<String>, SoldrError> {
    let targets = match crate::prepare_cmd::parse_target_arg(target)? {
        crate::prepare_cmd::ParsedTargetArg::All => {
            crate::cargo_metadata_soldr::resolve_all_targets()?
        }
        crate::prepare_cmd::ParsedTargetArg::Explicit(inputs) => inputs
            .into_iter()
            .map(|input| {
                crate::target_alias::resolve_soldr_target(&input)
                    .map(|resolved| resolved.rust_triple)
                    .map_err(|error| SoldrError::Other(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    if targets.len() > 1 && exporting_environment {
        return Err(SoldrError::Other(
            "soldr prepare cannot export multiple target lifecycles to one GitHub environment \
             file; prepare each target in its own job, or invoke soldr build/clippy/test with \
             the selected target"
                .to_string(),
        ));
    }
    Ok(targets)
}

pub(crate) fn encoded_rustflags_for_prep(prep: &BlessedPrep) -> Option<String> {
    prep.env
        .iter()
        .find(|(key, _)| is_target_rustflags_key(key))
        .map(|(key, required)| {
            let target = std::env::var(key).ok();
            let global = std::env::var("RUSTFLAGS").ok();
            merge_encoded_rustflags(
                &std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default(),
                required,
                target.as_deref(),
                global.as_deref(),
            )
        })
}

pub(crate) fn plan(target: &str) -> Result<TargetPlan, SoldrError> {
    plan_for_host(target, crate::pyo3_detect::host_triple())
}

fn plan_for_host(target: &str, host: &str) -> Result<TargetPlan, SoldrError> {
    let attrs = classify_target(target)?;
    let canonical = crate::core::CANONICAL_TARGETS.contains(&target);
    let canonical_alias = crate::target_alias::CANONICAL_ALIASES
        .iter()
        .find_map(|(alias, triple)| (*triple == target).then(|| (*alias).to_string()));

    let (toolchain, platform, keys): (ToolchainPlan, PlatformPlan, Vec<String>) =
        match (attrs.os, attrs.abi, attrs.arch) {
            (TargetOs::Windows, Some(TargetAbi::Msvc), _) => {
                let suffix = target.replace('-', "_");
                let upper = suffix.to_ascii_uppercase();
                (
                    ToolchainPlan {
                        family: "windows-msvc",
                        c_compiler: "clang-cl",
                        cxx_compiler: "clang-cl",
                        linker: "lld-link",
                        archiver: "llvm-lib",
                    },
                    PlatformPlan {
                        kind: "windows-sdk",
                        provider: "soldr-toolchain",
                        identity: format!("windows-msvc/{target}"),
                        root_env: Some("XWIN_CACHE_DIR"),
                    },
                    vec![
                        format!("AR_{suffix}"),
                        format!("CC_{suffix}"),
                        format!("CXXFLAGS_{suffix}"),
                        format!("CXX_{suffix}"),
                        format!("CARGO_TARGET_{upper}_LINKER"),
                        format!("CARGO_TARGET_{upper}_RUSTFLAGS"),
                        format!("CFLAGS_{suffix}"),
                        "XWIN_CACHE_DIR".to_string(),
                    ],
                )
            }
            (TargetOs::Darwin, None, _) => {
                let suffix = target.replace('-', "_");
                let upper = suffix.to_ascii_uppercase();
                (
                    ToolchainPlan {
                        family: "apple-darwin",
                        c_compiler: "clang",
                        cxx_compiler: "clang++",
                        linker: "clang",
                        archiver: "llvm-ar",
                    },
                    PlatformPlan {
                        kind: "apple-sdk",
                        provider: "soldr-toolchain",
                        identity: format!("apple-darwin/{target}"),
                        root_env: Some("SDKROOT"),
                    },
                    vec![
                        format!("AR_{suffix}"),
                        format!("CC_{suffix}"),
                        format!("CXXFLAGS_{suffix}"),
                        format!("CXX_{suffix}"),
                        format!("CARGO_TARGET_{upper}_LINKER"),
                        format!("CARGO_TARGET_{upper}_RUSTFLAGS"),
                        format!("CFLAGS_{suffix}"),
                        "SDKROOT".to_string(),
                    ],
                )
            }
            // Host-native. soldr#2145: `-gnu` no longer stops here — it
            // falls through to the managed-zig arm below so the reported
            // plan matches what `prepare_target` actually does. A plan
            // that says `cc`/`host-sysroot` while the build links through
            // zig is worse than no plan at all.
            (TargetOs::Linux, Some(abi @ (TargetAbi::Gnu | TargetAbi::Musl)), _)
                if target == host && (abi == TargetAbi::Musl || !native_gnu_link_enabled()) =>
            {
                let family = if abi == TargetAbi::Musl {
                    "linux-musl"
                } else {
                    "linux-gnu"
                };
                (
                    ToolchainPlan {
                        family,
                        c_compiler: "cc",
                        cxx_compiler: "c++",
                        linker: "cc",
                        archiver: "ar",
                    },
                    PlatformPlan {
                        kind: "host-sysroot",
                        provider: "host",
                        identity: format!("{family}/{target}"),
                        root_env: None,
                    },
                    Vec::new(),
                )
            }
            (TargetOs::Linux, Some(abi @ (TargetAbi::Gnu | TargetAbi::Musl)), _) => {
                let suffix = target.replace('-', "_");
                let upper = suffix.to_ascii_uppercase();
                let family = if abi == TargetAbi::Musl {
                    "linux-musl"
                } else {
                    "linux-gnu"
                };
                (
                    ToolchainPlan {
                        family,
                        c_compiler: "zig cc",
                        cxx_compiler: "zig c++",
                        linker: "zig cc",
                        archiver: "zig ar",
                    },
                    PlatformPlan {
                        kind: "zig-sysroot",
                        provider: "soldr-managed-zig",
                        identity: format!("zig-{}/{target}", crate::fetch::MANAGED_ZIG_VERSION),
                        root_env: None,
                    },
                    vec![
                        format!("AR_{suffix}"),
                        format!("CC_{suffix}"),
                        format!("CXX_{suffix}"),
                        format!("CARGO_TARGET_{upper}_LINKER"),
                        format!("CARGO_TARGET_{upper}_RUSTFLAGS"),
                        format!("RANLIB_{suffix}"),
                    ],
                )
            }
            (TargetOs::Windows, Some(TargetAbi::Gnu), _) => (
                ToolchainPlan {
                    family: "windows-gnu",
                    c_compiler: "gcc",
                    cxx_compiler: "g++",
                    linker: "gcc",
                    archiver: "ar",
                },
                PlatformPlan {
                    kind: "mingw-sysroot",
                    provider: "soldr-toolchain",
                    identity: format!("windows-gnu/{target}"),
                    root_env: Some("MINGW_W64_GCC_ROOT"),
                },
                Vec::new(),
            ),
            _ => {
                return Err(SoldrError::UnsupportedPlatform(format!(
                    "no blessed target plan for `{target}`"
                )))
            }
        };

    let keys = keys
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(TargetPlan {
        schema_version: TARGET_PLAN_SCHEMA_VERSION,
        canonical,
        canonical_target: target.to_string(),
        canonical_alias,
        cache_identity: platform.identity.clone(),
        toolchain,
        platform,
        environment: EnvironmentPlan {
            path_prepend: true,
            keys,
        },
        supported_operations: if canonical {
            vec![
                "prepare",
                "build",
                "clippy",
                "test-no-run",
                "nextest-archive",
                "pep517-wheel",
                "pep517-sdist",
            ]
        } else {
            Vec::new()
        },
    })
}

fn flag_global_key(key: &str) -> Option<&'static str> {
    if is_target_rustflags_key(key) {
        Some("RUSTFLAGS")
    } else if key.starts_with("CFLAGS_") {
        Some("CFLAGS")
    } else if key.starts_with("CXXFLAGS_") {
        Some("CXXFLAGS")
    } else {
        None
    }
}

fn merge_flag_values(required: &str, existing: Option<&str>, global: Option<&str>) -> String {
    let required = required.trim();
    let mut merged = existing.unwrap_or("").trim().to_string();
    if merged.is_empty() {
        merged.push_str(required);
    } else if !merged.contains(required) {
        merged = format!("{required} {merged}");
    }
    let global = global.unwrap_or("").trim();
    if !global.is_empty() && !merged.contains(global) {
        merged.push(' ');
        merged.push_str(global);
    }
    merged
}

fn is_target_rustflags_key(key: &str) -> bool {
    key.starts_with("CARGO_TARGET_") && key.ends_with("_RUSTFLAGS")
}

fn merge_encoded_rustflags(
    encoded: &str,
    required: &str,
    target: Option<&str>,
    global: Option<&str>,
) -> String {
    let mut tokens: Vec<String> = encoded
        .split('\u{1f}')
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    append_flag_tokens(&mut tokens, required, true);
    if let Some(target) = target {
        append_flag_tokens(&mut tokens, target, false);
    }
    if let Some(global) = global {
        append_flag_tokens(&mut tokens, global, false);
    }
    tokens.join("\u{1f}")
}

fn append_flag_tokens(tokens: &mut Vec<String>, flags: &str, structured_codegen: bool) {
    if structured_codegen {
        if let Some(rest) = flags.trim().strip_prefix("-C ") {
            for value in rest.split(" -C ").filter(|value| !value.is_empty()) {
                if !contains_codegen_pair(tokens, value) {
                    tokens.push("-C".to_string());
                    tokens.push(value.to_string());
                }
            }
            return;
        }
    }
    let values: Vec<_> = flags.split_ascii_whitespace().collect();
    let mut index = 0;
    while index < values.len() {
        if values[index] == "-C" && index + 1 < values.len() {
            if !contains_codegen_pair(tokens, values[index + 1]) {
                tokens.push("-C".to_string());
                tokens.push(values[index + 1].to_string());
            }
            index += 2;
        } else {
            if !tokens.iter().any(|token| token == values[index]) {
                tokens.push(values[index].to_string());
            }
            index += 1;
        }
    }
}

fn contains_codegen_pair(tokens: &[String], value: &str) -> bool {
    tokens
        .windows(2)
        .any(|pair| pair[0] == "-C" && pair[1] == value)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(every_canonical_target_has_a_stable_capability_plan, {
        for target in crate::core::CANONICAL_TARGETS {
            let plan = plan(target).unwrap_or_else(|error| panic!("{target}: {error}"));
            assert!(plan.canonical, "{target}");
            assert_eq!(plan.schema_version, 1);
            assert_eq!(plan.canonical_target, *target);
            assert!(plan.canonical_alias.is_some(), "{target}");
            assert_eq!(
                plan.supported_operations,
                [
                    "prepare",
                    "build",
                    "clippy",
                    "test-no-run",
                    "nextest-archive",
                    "pep517-wheel",
                    "pep517-sdist",
                ]
            );
            assert!(
                plan.environment
                    .keys
                    .windows(2)
                    .all(|pair| pair[0] < pair[1]),
                "environment keys must be sorted and unique: {:?}",
                plan.environment.keys
            );
        }
    });

    crate::timed_test!(linux_arm64_plan_uses_managed_zig_without_legacy_wrapper, {
        let plan = plan_for_host("aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(plan.toolchain.family, "linux-gnu");
        assert_eq!(plan.toolchain.linker, "zig cc");
        assert_eq!(plan.platform.provider, "soldr-managed-zig");
        let json = serde_json::to_string(&plan).unwrap();
        assert!(!json.contains("cargo-zigbuild"));
    });

    crate::timed_test!(legacy_linux_override_does_not_mix_blessed_wrappers, {
        assert!(!should_prepare_managed_linux(
            TargetOs::Linux,
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            true,
        ));
    });

    crate::timed_test!(host_native_gnu_still_goes_through_managed_zig, {
        // soldr#2145: this is the case that used to be skipped, which is
        // why the published x86_64 artifact inherited the release
        // runner's glibc 2.39 while aarch64 -- cross-built -- got 2.28.
        assert!(should_prepare_managed_linux(
            TargetOs::Linux,
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            false,
        ));
    });

    crate::timed_test!(the_reported_plan_matches_what_prepare_actually_does, {
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        std::env::remove_var(NATIVE_GNU_LINK_ENV_VAR);
        // A plan that claims `cc`/`host-sysroot` while the build links
        // through zig is worse than no plan at all.
        let plan = plan_for_host("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(plan.toolchain.linker, "zig cc");
        assert_eq!(plan.platform.provider, "soldr-managed-zig");

        // ...and the opt-out has to move the plan back too.
        std::env::set_var(NATIVE_GNU_LINK_ENV_VAR, "0");
        let opted_out =
            plan_for_host("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(opted_out.toolchain.linker, "cc");
        assert_eq!(opted_out.platform.provider, "host");
        std::env::remove_var(NATIVE_GNU_LINK_ENV_VAR);

        // musl is host-native either way.
        let musl = plan_for_host("x86_64-unknown-linux-musl", "x86_64-unknown-linux-musl").unwrap();
        assert_eq!(musl.toolchain.linker, "cc");
    });

    crate::timed_test!(host_native_musl_is_left_alone, {
        // musl is statically linked, so it has no glibc floor to improve
        // and nothing to gain from the detour.
        assert!(!should_prepare_managed_linux(
            TargetOs::Linux,
            "x86_64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
            false,
        ));
    });

    crate::timed_test!(the_native_gnu_opt_out_restores_the_old_behaviour, {
        let _lock = crate::TEST_PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for falsy in ["0", "false", "no", "off", ""] {
            std::env::set_var(NATIVE_GNU_LINK_ENV_VAR, falsy);
            assert!(
                !should_prepare_managed_linux(
                    TargetOs::Linux,
                    "x86_64-unknown-linux-gnu",
                    "x86_64-unknown-linux-gnu",
                    false,
                ),
                "{falsy:?} must disable the native-gnu detour"
            );
        }
        std::env::set_var(NATIVE_GNU_LINK_ENV_VAR, "1");
        assert!(should_prepare_managed_linux(
            TargetOs::Linux,
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            false,
        ));
        std::env::remove_var(NATIVE_GNU_LINK_ENV_VAR);
        // Cross-compiling is unaffected by the opt-out either way.
        std::env::set_var(NATIVE_GNU_LINK_ENV_VAR, "0");
        assert!(should_prepare_managed_linux(
            TargetOs::Linux,
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            false,
        ));
        std::env::remove_var(NATIVE_GNU_LINK_ENV_VAR);
    });

    crate::timed_test!(noncanonical_targets_do_not_advertise_blessed_operations, {
        let plan = plan("x86_64-pc-windows-gnu").unwrap();
        assert!(!plan.canonical);
        assert!(plan.supported_operations.is_empty());
    });

    crate::timed_test!(required_msvc_flags_merge_with_target_and_project_flags, {
        let required = "-C link-arg=/LIBPATH:/soldr/sdk";
        let merged = merge_flag_values(
            required,
            Some("-C target-cpu=x86-64-v3"),
            Some("-Dwarnings"),
        );
        assert!(merged.contains("/LIBPATH:/soldr/sdk"));
        assert!(merged.contains("target-cpu=x86-64-v3"));
        assert!(merged.contains("-Dwarnings"));
    });

    crate::timed_test!(required_c_flags_merge_with_project_flags, {
        let merged = merge_flag_values(
            "/imsvc/soldr/sdk/include",
            Some("-DPROJECT_TARGET=1"),
            Some("-DPROJECT_GLOBAL=1"),
        );
        assert!(merged.contains("/imsvc/soldr/sdk/include"));
        assert!(merged.contains("-DPROJECT_TARGET=1"));
        assert!(merged.contains("-DPROJECT_GLOBAL=1"));
    });

    crate::timed_test!(
        encoded_project_rustflags_keep_required_linker_search_path,
        {
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
    );

    crate::timed_test!(encoded_required_flags_preserve_paths_with_spaces, {
        let merged = merge_encoded_rustflags(
            "",
            "-C linker-flavor=lld-link -C link-arg=/LIBPATH:/soldr sdk/lib",
            None,
            None,
        );
        let tokens: Vec<_> = merged.split('\u{1f}').collect();
        assert!(tokens.contains(&"link-arg=/LIBPATH:/soldr sdk/lib"));
    });

    crate::timed_test!(applying_target_flags_consumes_higher_precedence_globals, {
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
    });

    crate::timed_test!(all_compiling_cargo_surfaces_enter_the_lifecycle, {
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
    });
}
