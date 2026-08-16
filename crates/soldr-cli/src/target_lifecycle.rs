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

    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Linux
        && attrs.os == TargetOs::Windows
        && attrs.abi == Some(TargetAbi::Msvc)
        && prep.xwin_cache_dir.is_none()
    {
        // soldr#2519: the blessed cache is the only MSVC path now, so
        // failing to materialize it is terminal rather than a nudge toward
        // cargo-xwin's unpinned live download.
        return Err(SoldrError::Other(format!(
            "blessed MSVC SDK preparation did not produce an xwin cache for {target}"
        )));
    }

    if attrs.os == TargetOs::Darwin && target != host && prep.sdkroot.is_none() {
        // soldr#2519: no cargo-zigbuild fallback remains to point at.
        return Err(SoldrError::Other(format!(
            "blessed Apple SDK preparation did not produce an SDK for {target}"
        )));
    }

    // soldr#2437: the catalogue GNU/musl Linux toolchains ship Linux ELF
    // compilers. A Windows host can download them but not execute them --
    // the old behavior fetched hundreds of megabytes and then died deep in
    // the first *-sys build with `%1 is not a valid Win32 application
    // (os error 193)`. Fail fast with the supported paths instead.
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows
        && attrs.os == TargetOs::Linux
    {
        return Err(SoldrError::UnsupportedPlatform(format!(
            concat!(
                "Windows-hosted cross-builds for `{target}` are not supported: ",
                "the catalogue toolchain for Linux targets ships Linux ELF ",
                "compilers that cannot execute on Windows (soldr#2437). Build ",
                "on a Linux host or the Docker Linux harness (`uv run ",
                "--no-project python ci/perf_local.py cargo build --target ",
                "{target} ...`), or use the explicit legacy passthrough ",
                "(`soldr cargo zigbuild --target {target}`) if a ",
                "Windows-hosted build is unavoidable."
            ),
            target = target
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
    } else if attrs.os == TargetOs::Linux && attrs.abi == Some(TargetAbi::Musl) {
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

// soldr#2519 removed `should_prepare_managed_linux`. It gated the Zig-backed
// musl path on the legacy diagnostic override, so with that override gone it
// was unreachable. GNU and musl are both catalogue-backed above.

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
mod tests;

#[cfg(test)]
mod link_self_contained_tests;
