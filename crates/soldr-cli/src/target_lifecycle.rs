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
    // soldr#2139: `x86_64-unknown-linux-gnu.2.17` is a soldr-level spelling.
    // rustc, rustup and the catalogue sysroot tables know only the base triple,
    // so everything below works on `base`; the sysroot's pinned 2.17 ABI is the
    // mechanism that enforces accepted GNU floor requests.
    let glibc_floor = crate::target_alias::split_glibc_floor(target);
    if let Some((base, floor)) = glibc_floor {
        if !crate::target_alias::glibc_floor_is_supported(base, floor) {
            return Err(SoldrError::Other(
                crate::target_alias::reject_glibc_versioned(target)
                    .expect_err("unsupported glibc floor must be rejected")
                    .to_string(),
            ));
        }
    }
    let base = glibc_floor.map_or(target, |(base, _)| base);

    let attrs = classify_target(base)?;
    let host = crate::pyo3_detect::host_triple();
    crate::prepare_cmd::rustup_add_target(base)?;
    let mut prep = crate::blessed_build::prepare(paths, base).await?;

    let legacy_xwin = std::env::var_os(crate::blessed_build::USE_LEGACY_XWIN_ENV_VAR)
        .is_some_and(|value| !value.is_empty() && value != "0");
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux
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

    let gnu_uses_catalogue_toolchain =
        attrs.os == TargetOs::Linux && attrs.abi == Some(TargetAbi::Gnu);
    if gnu_uses_catalogue_toolchain {
        // Env keys come from the base triple: a dot is not legal in an
        // environment variable name, so `CC_x86_64_unknown_linux_gnu.2.17`
        // would be silently unusable.
        let suffix = base.replace('-', "_");
        let upper = suffix.to_ascii_uppercase();
        // The catalogue-backed GNU bundle supplies a pinned glibc sysroot as
        // well as the compiler. Keep every consumer (cc-rs, rustc's linker,
        // CMake, and pkg-config) on that root rather than allowing the runner
        // to leak host headers or libraries into a blessed artifact.
        let bundle = crate::fetch::gnu_linux_toolchain::GnuLinuxToolchainTarget::for_triple(base)
            .ok_or_else(|| {
            SoldrError::UnsupportedPlatform(format!(
                "no catalogue-backed GNU/Linux toolchain is available for `{target}`"
            ))
        })?;
        let toolchain = crate::fetch::gnu_linux_toolchain::ensure(paths, base).await?;
        debug_assert_eq!(toolchain.target, bundle);
        prep.path_dirs.push(toolchain.bin_dir.clone());
        let mut env = crate::fetch::gnu_linux_toolchain::env_for_target(&toolchain, base);
        // soldr#2309: pin the C++ stdlib end to end. The catalogue GNU driver
        // ships libstdc++ only, so any phase that decides on `-lc++` (CMake
        // detecting a clang-family compiler, zig's bundled clang on the
        // legacy path) fails the final link with `ld: cannot find -lc++`.
        // Setdefault semantics: a caller-set value through any spelling in
        // cc-rs's lookup chain wins, matching the CARGO_BUILD_TARGET /
        // CMAKE injection precedent.
        env.extend(crate::fetch::gnu_linux_toolchain::cxx_stdlib_pin_env(
            base,
            crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux,
            |key| std::env::var_os(key).is_some(),
        ));
        let sysroot = toolchain.sysroot.to_string_lossy().into_owned();
        env.push((
            format!("CARGO_TARGET_{upper}_RUSTFLAGS"),
            format!("-C link-arg=--sysroot={sysroot}"),
        ));
        prep.env.extend(env);
        add_link_self_contained_flag(&mut prep, base, &upper);
    } else if attrs.os == TargetOs::Linux && attrs.abi == Some(TargetAbi::Musl) && !legacy_zigbuild
    {
        let suffix = base.replace('-', "_");
        let upper = suffix.to_ascii_uppercase();
        let bundle = crate::fetch::musl_linux_toolchain::MuslLinuxToolchainTarget::for_triple(base)
            .ok_or_else(|| {
                SoldrError::UnsupportedPlatform(format!(
                    "no catalogue-backed musl/Linux toolchain is available for `{target}`"
                ))
            })?;
        let toolchain = crate::fetch::musl_linux_toolchain::ensure(paths, base).await?;
        debug_assert_eq!(toolchain.target, bundle);
        prep.path_dirs.push(toolchain.bin_dir.clone());
        let mut env = crate::fetch::musl_linux_toolchain::env_for_target(&toolchain, base);
        let sysroot = toolchain.sysroot.to_string_lossy().into_owned();
        env.push((
            format!("CARGO_TARGET_{upper}_RUSTFLAGS"),
            format!("-C link-arg=--sysroot={sysroot}"),
        ));
        prep.env.extend(env);
    } else if should_prepare_managed_linux(attrs.os, attrs.abi, base, host, legacy_zigbuild) {
        eprintln!(
            "soldr: {}=1 selects the legacy Zig musl diagnostic path; it is unsupported for normal builds and will be removed in soldr 0.9.0",
            crate::blessed_build::USE_LEGACY_ZIGBUILD_ENV_VAR
        );
        let suffix = base.replace('-', "_");
        let upper = suffix.to_ascii_uppercase();
        let tools = crate::linux_cross::prepare(paths, target).await?;
        prep.path_dirs.push(tools.bin_dir);
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
        ]);
        add_link_self_contained_flag(&mut prep, base, &upper);
    }
    Ok(prep)
}

fn add_link_self_contained_flag(prep: &mut BlessedPrep, base: &str, upper: &str) {
    // `-C link-self-contained` is not accepted on every target, and
    // passing it where it is unsupported is a hard rustc error rather
    // than a warning:
    //
    //   error: option `-C link-self-contained` is not supported on this target
    //
    // It exists here to stop rustc bundling its own startup objects when
    // zig is the linker, which is an x86_64 concern; targets that reject
    // the flag do not do that bundling in the first place, so omitting it
    // is not a behaviour change for them.
    //
    // This only surfaces on a *host-native* aarch64 build, because the
    // cross lanes drive aarch64 from an x86_64 host. The release workflow
    // is the one place that builds aarch64 natively, so it broke there
    // and nowhere else.
    if supports_link_self_contained(base) {
        let key = format!("CARGO_TARGET_{upper}_RUSTFLAGS");
        if let Some((_, flags)) = prep.env.iter_mut().find(|(name, _)| name == &key) {
            flags.push_str(" -C link-self-contained=no");
        } else {
            prep.env
                .push((key, "-C link-self-contained=no".to_string()));
        }
    }
}

/// Whether `rustc` accepts `-C link-self-contained` for `target`.
///
/// Only x86_64 GNU accepts the compatibility flag. Musl now uses its managed
/// CRT directly, so injecting it would revive the old duplicate `_start`
/// failure mode from the Zig wrapper path.
///
/// Getting this wrong fails in *both* directions, so neither half is
/// cosmetic:
///
/// - Passing it where unsupported is a hard rustc error
///   (`option -C link-self-contained is not supported on this target`).
///
/// Unknown targets default to omitting the flag because passing it where it
/// is unsupported is a hard error.
fn supports_link_self_contained(base_triple: &str) -> bool {
    base_triple == "x86_64-unknown-linux-gnu"
}

/// Whether this target still needs the Zig-backed Linux preparation path.
///
/// This is deliberately only the explicit legacy diagnostic override. Normal
/// GNU and musl targets are both catalogue-backed above.
fn should_prepare_managed_linux(
    os: TargetOs,
    abi: Option<TargetAbi>,
    target: &str,
    host: &str,
    legacy_zigbuild: bool,
) -> bool {
    os == TargetOs::Linux && abi == Some(TargetAbi::Musl) && legacy_zigbuild && target != host
}

/// Preserve Cargo's custom target-spec passthrough while using the unified
/// lifecycle for every target family soldr recognizes.
pub(crate) async fn prepare_for_invocation(
    paths: &SoldrPaths,
    target: &str,
) -> Result<BlessedPrep, SoldrError> {
    // soldr#2139: `soldr build` reaches here having only run
    // `normalize_target_aliases_in_args`, which leaves an unrecognised target
    // untouched -- so a glibc-versioned triple used to walk straight into the
    // sysroot table and emit "no <lib> sysroot recipe for target …" per
    // library before continuing anyway. `soldr prepare` already rejected it,
    // via the resolver, with an error that names `soldr build`. Reject it on
    // every prep entry so the blessed surface says the same thing.
    crate::target_alias::reject_glibc_versioned(target)
        .map_err(|error| SoldrError::Other(error.to_string()))?;
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
///
/// This function folds any *ambient* `CARGO_ENCODED_RUSTFLAGS` into the target
/// key, which is what lets the target key carry a complete value. Note that it
/// does not follow that the target key is what ends up winning: both callers
/// also export the encoded form afterwards —
/// `apply_to_process` into the process, and
/// `prepare_cmd::apply_blessed_prep_env` into `$GITHUB_ENV` — and
/// `CARGO_ENCODED_RUSTFLAGS` outranks `CARGO_TARGET_<triple>_RUSTFLAGS` in
/// Cargo's precedence order.
///
/// The practical consequence for consumers: a `CARGO_TARGET_*_RUSTFLAGS` set
/// *before* preparation is folded in and survives, but one set *afterwards* is
/// inert, because the exported encoded value already outranks it. Downstream
/// build scripts that want to add a flag after `soldr prepare` must append to
/// `CARGO_ENCODED_RUSTFLAGS` (see zackees/clud#732).
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

fn plan_for_host(target: &str, _host: &str) -> Result<TargetPlan, SoldrError> {
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
            (TargetOs::Linux, Some(TargetAbi::Gnu), _) => {
                let upper = target.replace('-', "_").to_ascii_uppercase();
                (
                    ToolchainPlan {
                        family: "linux-gnu",
                        c_compiler: "managed gcc",
                        cxx_compiler: "managed g++",
                        linker: "managed gcc",
                        archiver: "managed ar",
                    },
                    PlatformPlan {
                        kind: "gnu-linux-sysroot",
                        provider: "soldr-toolchain",
                        identity: format!(
                            "gnu-linux-toolchain/{}/{target}",
                            crate::fetch::gnu_linux_toolchain::GNU_LINUX_TOOLCHAIN_VERSION
                        ),
                        root_env: Some("SOLDR_GNU_LINUX_TOOLCHAIN_ROOT"),
                    },
                    {
                        let mut keys =
                            crate::fetch::gnu_linux_toolchain::env_keys_for_target(target);
                        keys.push(format!("CARGO_TARGET_{upper}_RUSTFLAGS"));
                        keys
                    },
                )
            }
            (TargetOs::Linux, Some(TargetAbi::Musl), _) => (
                ToolchainPlan {
                    family: "linux-musl",
                    c_compiler: "managed gcc",
                    cxx_compiler: "managed g++",
                    linker: "managed gcc",
                    archiver: "managed ar",
                },
                PlatformPlan {
                    kind: "musl-linux-sysroot",
                    provider: "soldr-toolchain",
                    identity: format!(
                        "musl-linux-toolchain/{}/{target}",
                        crate::fetch::musl_linux_toolchain::MUSL_LINUX_TOOLCHAIN_VERSION
                    ),
                    root_env: Some("SOLDR_MUSL_LINUX_TOOLCHAIN_ROOT"),
                },
                {
                    let upper = target.replace('-', "_").to_ascii_uppercase();
                    let mut keys = crate::fetch::musl_linux_toolchain::env_keys_for_target(target);
                    keys.push(format!("CARGO_TARGET_{upper}_RUSTFLAGS"));
                    keys
                },
            ),
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

    #[test]
    fn every_canonical_target_has_a_stable_capability_plan() {
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

    #[test]
    fn legacy_linux_override_does_not_mix_blessed_wrappers() {
        assert!(!should_prepare_managed_linux(
            TargetOs::Linux,
            Some(TargetAbi::Gnu),
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            true,
        ));
    }

    #[test]
    fn host_native_gnu_uses_catalogue_toolchain() {
        // GNU target dispatch occurs before the Zig fallback, including
        // host-native x64, so the ABI cannot inherit the runner's linker.
        assert!(!should_prepare_managed_linux(
            TargetOs::Linux,
            Some(TargetAbi::Gnu),
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            false,
        ));
    }

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

    #[test]
    fn normal_musl_never_reaches_legacy_zig() {
        assert!(!should_prepare_managed_linux(
            TargetOs::Linux,
            Some(TargetAbi::Musl),
            "x86_64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
            false,
        ));
    }

    #[test]
    fn only_explicit_legacy_musl_can_reach_zig_fallback() {
        assert!(should_prepare_managed_linux(
            TargetOs::Linux,
            Some(TargetAbi::Musl),
            "aarch64-unknown-linux-musl",
            "x86_64-unknown-linux-gnu",
            true,
        ));
        assert!(!should_prepare_managed_linux(
            TargetOs::Linux,
            Some(TargetAbi::Gnu),
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            false,
        ));
    }

    #[test]
    fn noncanonical_targets_do_not_advertise_blessed_operations() {
        let plan = plan("x86_64-pc-windows-gnu").unwrap();
        assert!(!plan.canonical);
        assert!(plan.supported_operations.is_empty());
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
}

#[cfg(test)]
mod link_self_contained_tests {
    use super::supports_link_self_contained;

    // A native aarch64 release build failed with
    //   error: option `-C link-self-contained` is not supported on this target
    // because managed-zig prep injected the flag unconditionally. Only the
    // release workflow builds aarch64 natively -- every other lane drives it
    // from an x86_64 host -- so nothing else exercised this path.
    // The release failure: rustc rejects the flag outright here.
    #[test]
    fn aarch64_gnu_does_not_get_link_self_contained() {
        assert!(!supports_link_self_contained("aarch64-unknown-linux-gnu"));
    }

    // The managed musl CRT owns startup objects. Re-injecting the Zig-only
    // self-contained override can produce a duplicate `_start` at link time.
    #[test]
    fn managed_musl_never_gets_the_zig_startup_override() {
        assert!(!supports_link_self_contained("aarch64-unknown-linux-musl"));
        assert!(!supports_link_self_contained("x86_64-unknown-linux-musl"));
    }

    #[test]
    fn x86_64_gnu_still_gets_it() {
        assert!(supports_link_self_contained("x86_64-unknown-linux-gnu"));
    }

    // Unknown targets must default to *not* passing the flag: passing it
    // where unsupported is a hard error, while omitting it merely restores
    // the pre-managed-zig linking behaviour.
    #[test]
    fn an_unknown_target_defaults_to_omitting_the_flag() {
        assert!(!supports_link_self_contained("riscv64gc-unknown-linux-gnu"));
        assert!(!supports_link_self_contained("powerpc64le-unknown-linux"));
    }
}
