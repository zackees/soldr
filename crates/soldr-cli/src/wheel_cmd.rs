//! `soldr wheel --target <triple>` — the blessed Python wheel surface
//! (soldr#2139 gap 1).
//!
//! # Why a verb rather than a flag
//!
//! `soldr build --wheel` would overload the blessed-toolchain verb with a
//! different *output artifact*, and `soldr maturin build` looks like a
//! passthrough while silently doing sysroot preparation. `wheel` says what it
//! produces and is honest about doing work first.
//!
//! # What it is
//!
//! A thin front end over the existing `soldr maturin ...` execution path. It
//! resolves the friendly target alias, picks maturin's `--compatibility`
//! value from the target family, and hands a `maturin build ...` argument
//! vector back to the ordinary dispatcher — which already owns maturin
//! provisioning, toolchain env pinning, the build lease, target preparation
//! (`target_lifecycle::prepare_for_invocation`), and the PyO3 plan. Nothing
//! about the wheel *naming* contract is touched; that is downstream-visible
//! and is maturin's to decide.
//!
//! # Scope: abi3 only
//!
//! A non-abi3 extension module has to link against a CPython built for the
//! *target*, which is a materially harder problem than mounting a sysroot.
//! abi3 needs no target-side interpreter, and it covers soldr's own wheel.
//! Anything the PyO3 planner cannot place in an interpreter-free mode is
//! refused with a message naming `soldr maturin build` as the escape hatch,
//! rather than silently degrading into a wheel built against the host's
//! Python.
//!
//! # Note on glibc floors
//!
//! `--compatibility manylinux_2_17` is a *tag*, and the suffixed-triple floor
//! (`...-linux-gnu.2.17`, soldr#2202) means "ask zig for this floor", never
//! "guarantee this floor" — the effective floor is the max of what zig was
//! asked for and every symbol the vendored C dependencies reference. The
//! suffixed spelling is therefore rejected here rather than being quietly
//! accepted into a wheel tag that would read as a promise.

use crate::core::SoldrError;
use crate::pyo3_detect::PlanMode;

/// Arguments for `soldr wheel`.
///
/// `--target` must precede any passthrough arguments, because everything
/// after the first free argument is forwarded to maturin verbatim.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct WheelArgs {
    /// Target triple or friendly alias (for example `linux-arm64`)
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,
    /// Extra arguments forwarded verbatim to `maturin build`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

/// maturin's `--compatibility` value for a resolved Rust target triple.
///
/// Mirrors the release lane (`release-auto.yml`): linux-gnu wheels are tagged
/// `manylinux_2_17`, linux-musl wheels `musllinux_1_2`, and everything else
/// keeps maturin's `pypi` auto-tagging.
pub fn compatibility_for_target(triple: &str) -> &'static str {
    if triple.contains("-linux-musl") {
        "musllinux_1_2"
    } else if triple.contains("-linux-gnu") {
        "manylinux_2_17"
    } else {
        "pypi"
    }
}

fn has_flag(args: &[String], flag: &str) -> bool {
    let prefix = format!("{flag}=");
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == flag || arg.starts_with(&prefix))
}

/// Pure argv builder: `(target, passthrough) -> maturin argument vector`.
///
/// No I/O, no env reads — this is the piece worth unit-testing, and it is the
/// only place the wheel surface decides anything.
pub fn maturin_build_argv(
    target: Option<&str>,
    rest: &[String],
) -> Result<Vec<String>, SoldrError> {
    let requested = target
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            SoldrError::Other(
                "soldr wheel: --target <triple> is required (friendly aliases such as \
                 `linux-arm64` are accepted). Use `soldr maturin build` for a host wheel."
                    .to_string(),
            )
        })?;

    if has_flag(rest, "--target") {
        return Err(SoldrError::Other(
            "soldr wheel: pass the target once, as `soldr wheel --target <triple> [...]`; \
             a second --target in the forwarded arguments would silently disagree with the \
             sysroot soldr prepared."
                .to_string(),
        ));
    }

    if let Some((base, floor)) = crate::target_alias::split_glibc_floor(requested) {
        return Err(SoldrError::Other(format!(
            "soldr wheel: glibc-floor targets (`{base}.{floor}`) are not supported by the \
             wheel surface. A floor is a request to zig, not a guarantee — the effective \
             floor is also bounded by every symbol the vendored C dependencies reference — \
             so folding it into a manylinux tag would publish a promise soldr cannot keep. \
             Use `soldr wheel --target {base}` (tagged \
             `{}`), or `soldr build --target {base}.{floor}` for a bare binary.",
            compatibility_for_target(base)
        )));
    }

    let resolved = crate::target_alias::resolve_soldr_target(requested).map_err(|err| {
        // `AliasError` renders itself for the `soldr build` surface. Re-point
        // it at the verb the user actually typed rather than telling them to
        // fix a command they did not run.
        SoldrError::Other(format!(
            "soldr wheel: {}",
            err.to_string()
                .replace("soldr build --target", "soldr wheel --target")
        ))
    })?;
    let triple = resolved.rust_triple;

    let mut argv = vec!["maturin".to_string(), "build".to_string()];
    // `--debug` is maturin's spelling for "not --release"; honour it rather
    // than emitting a contradictory pair.
    if !has_flag(rest, "--release") && !has_flag(rest, "--debug") {
        argv.push("--release".to_string());
    }
    if !has_flag(rest, "--compatibility") && !has_flag(rest, "--manylinux") {
        argv.push("--compatibility".to_string());
        argv.push(compatibility_for_target(&triple).to_string());
    }
    argv.push("--target".to_string());
    argv.push(triple);
    argv.extend(rest.iter().cloned());
    Ok(argv)
}

/// Read back the `--target` this module wrote into the argv.
fn target_in_argv(argv: &[String]) -> Option<&str> {
    argv.iter()
        .position(|arg| arg == "--target")
        .and_then(|idx| argv.get(idx + 1))
        .map(String::as_str)
}

/// The abi3-only scope gate, expressed over the PyO3 planner's decision.
///
/// Every allowed mode is one where the build needs no CPython built for the
/// target: a host-native build, a workspace with no PyO3 at all, abi3
/// (`PYO3_NO_PYTHON`), modern Windows `raw-dylib`, an explicitly opted-in
/// Python sysroot, or a caller who configured `PYO3_*` themselves.
pub fn abi3_scope_check(mode: PlanMode, target: &str) -> Result<(), SoldrError> {
    match mode {
        PlanMode::Native
        | PlanMode::NoPyo3
        | PlanMode::Abi3NoPython
        | PlanMode::ModernWindowsRawDylib
        | PlanMode::CompatibilitySysroot
        | PlanMode::CallerConfigured => Ok(()),
        PlanMode::ExtensionDefault | PlanMode::RequiresExplicitCompatibility => {
            Err(SoldrError::Other(format!(
                "soldr wheel: cross-building a wheel for `{target}` needs a CPython built \
                 for that target, because this workspace's PyO3 extension is not proven \
                 abi3. The first cut of `soldr wheel` is abi3-only (soldr#2139): enable an \
                 `abi3-py3xx` feature on pyo3, or set SOLDR_PYO3_COMPATIBILITY=sysroot, or \
                 drive maturin yourself with `soldr maturin build`."
            )))
        }
        PlanMode::Unresolved => Err(SoldrError::Other(format!(
            "soldr wheel: could not read Cargo metadata, so soldr cannot prove this \
             workspace is abi3-safe for `{target}`. The first cut of `soldr wheel` is \
             abi3-only (soldr#2139); run `soldr maturin build --target {target}` to build \
             anyway."
        ))),
    }
}

/// Build the full soldr argv that `soldr wheel` re-enters as.
///
/// The returned vector is a complete `soldr` command line minus argv[0]:
/// leading global flags, then `maturin build ...`. Re-entering the dispatcher
/// is what keeps the maturin provisioning ladder, toolchain pinning, build
/// lease, target preparation, and PyO3 planning in exactly one place.
pub(crate) fn maturin_invocation(
    args: &WheelArgs,
    no_cache: bool,
    trust_inherited_soldr_env: bool,
) -> Result<Vec<String>, SoldrError> {
    let build = maturin_build_argv(args.target.as_deref(), &args.rest)?;
    let triple = target_in_argv(&build)
        .expect("maturin_build_argv always writes --target")
        .to_string();

    // The gate only has something to say about cross builds; a host build
    // resolves to `PlanMode::Native` without touching Cargo metadata anyway,
    // so skip the `cargo metadata` round trip entirely.
    if triple != crate::pyo3_detect::host_triple() {
        let workspace_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let plan =
            crate::pyo3_detect::resolve_for_invocation(&workspace_root, &build, Some(&triple));
        abi3_scope_check(plan.mode, &triple)?;
    }

    let mut argv = Vec::with_capacity(build.len() + 2);
    if no_cache {
        argv.push("--no-cache".to_string());
    }
    if trust_inherited_soldr_env {
        argv.push("--trust-inherited-soldr-env".to_string());
    }
    argv.extend(build);
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timed_test;

    fn build(target: &str, rest: &[&str]) -> Vec<String> {
        let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
        maturin_build_argv(Some(target), &rest).expect("argv should build")
    }

    fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        argv.iter()
            .position(|arg| arg == flag)
            .and_then(|idx| argv.get(idx + 1))
            .map(String::as_str)
    }

    timed_test!(gnu_target_is_tagged_manylinux_2_17, {
        let argv = build("x86_64-unknown-linux-gnu", &[]);
        assert_eq!(argv[0], "maturin");
        assert_eq!(argv[1], "build");
        assert!(argv.contains(&"--release".to_string()), "{argv:?}");
        assert_eq!(flag_value(&argv, "--compatibility"), Some("manylinux_2_17"));
        assert_eq!(
            flag_value(&argv, "--target"),
            Some("x86_64-unknown-linux-gnu")
        );
    });

    timed_test!(musl_target_is_tagged_musllinux_1_2, {
        let argv = build("aarch64-unknown-linux-musl", &[]);
        assert_eq!(flag_value(&argv, "--compatibility"), Some("musllinux_1_2"));
        assert_eq!(
            flag_value(&argv, "--target"),
            Some("aarch64-unknown-linux-musl")
        );
    });

    timed_test!(non_linux_targets_keep_maturin_pypi_tagging, {
        for triple in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ] {
            let argv = build(triple, &[]);
            assert_eq!(
                flag_value(&argv, "--compatibility"),
                Some("pypi"),
                "{triple}"
            );
        }
    });

    timed_test!(gnueabihf_is_still_a_gnu_target, {
        assert_eq!(
            compatibility_for_target("armv7-unknown-linux-gnueabihf"),
            "manylinux_2_17"
        );
    });

    timed_test!(friendly_aliases_resolve_to_rust_triples, {
        for (alias, expected) in [
            ("linux-arm64", "aarch64-unknown-linux-gnu"),
            ("mac-arm64", "aarch64-apple-darwin"),
            ("win-x64", "x86_64-pc-windows-msvc"),
        ] {
            let argv = build(alias, &[]);
            assert_eq!(flag_value(&argv, "--target"), Some(expected), "{alias}");
        }
        // The alias must not survive into the argv maturin (and therefore
        // cargo) sees — rustc has never heard of `linux-arm64`.
        let argv = build("linux-arm64", &[]);
        assert!(!argv.iter().any(|arg| arg == "linux-arm64"), "{argv:?}");
    });

    timed_test!(alias_resolution_picks_the_musl_tag_for_musl_aliases, {
        let argv = build("linux-arm64-musl", &[]);
        assert_eq!(
            flag_value(&argv, "--target"),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(flag_value(&argv, "--compatibility"), Some("musllinux_1_2"));
    });

    timed_test!(passthrough_args_are_forwarded_after_soldr_defaults, {
        let argv = build("linux-x64", &["--out", "dist", "--locked"]);
        let tail = &argv[argv.len() - 3..];
        assert_eq!(tail, ["--out", "dist", "--locked"]);
    });

    timed_test!(caller_compatibility_and_debug_are_not_duplicated, {
        let argv = build("x86_64-unknown-linux-gnu", &["--compatibility", "linux"]);
        assert_eq!(
            argv.iter().filter(|arg| *arg == "--compatibility").count(),
            1,
            "{argv:?}"
        );
        assert_eq!(flag_value(&argv, "--compatibility"), Some("linux"));

        let argv = build("x86_64-unknown-linux-gnu", &["--debug"]);
        assert!(!argv.iter().any(|arg| arg == "--release"), "{argv:?}");

        let argv = build("x86_64-unknown-linux-gnu", &["--manylinux=2014"]);
        assert!(!argv.iter().any(|arg| arg == "--compatibility"), "{argv:?}");
    });

    timed_test!(missing_target_is_a_clear_error, {
        let err = maturin_build_argv(None, &[]).expect_err("--target is required");
        let message = err.to_string();
        assert!(message.contains("--target"), "{message}");
        assert!(message.contains("soldr wheel"), "{message}");

        let err = maturin_build_argv(Some("   "), &[]).expect_err("blank --target is required");
        assert!(err.to_string().contains("--target"), "{err}");
    });

    timed_test!(unknown_target_errors_with_a_suggestion, {
        let err = maturin_build_argv(Some("linux-arm65"), &[]).expect_err("unknown target");
        let message = err.to_string();
        assert!(message.contains("soldr wheel"), "{message}");
        assert!(message.contains("linux-arm65"), "{message}");
        // AliasError carries a Jaro-Winkler suggestion; it must survive the
        // wrap so the user is not left guessing.
        assert!(message.contains("linux-arm64"), "{message}");
    });

    timed_test!(ambiguous_and_32bit_targets_are_refused_not_degraded, {
        let err = maturin_build_argv(Some("linux-arm"), &[]).expect_err("ambiguous target");
        assert!(err.to_string().contains("linux-arm64"), "{err}");
        let err = maturin_build_argv(Some("win-x86"), &[]).expect_err("32-bit target");
        assert!(err.to_string().contains("32-bit"), "{err}");
    });

    timed_test!(
        glibc_floor_targets_are_refused_with_the_ask_not_guarantee_reason,
        {
            let err = maturin_build_argv(Some("x86_64-unknown-linux-gnu.2.17"), &[])
                .expect_err("glibc floor is out of scope for wheels");
            let message = err.to_string();
            assert!(message.contains("not a guarantee"), "{message}");
            assert!(message.contains("soldr build --target"), "{message}");
        }
    );

    timed_test!(a_second_target_in_the_passthrough_is_refused, {
        let rest = vec!["--target".to_string(), "aarch64-apple-darwin".to_string()];
        let err = maturin_build_argv(Some("linux-x64"), &rest).expect_err("duplicate --target");
        assert!(err.to_string().contains("pass the target once"), "{err}");
    });

    timed_test!(abi3_gate_allows_only_interpreter_free_modes, {
        for mode in [
            PlanMode::Native,
            PlanMode::NoPyo3,
            PlanMode::Abi3NoPython,
            PlanMode::ModernWindowsRawDylib,
            PlanMode::CompatibilitySysroot,
            PlanMode::CallerConfigured,
        ] {
            assert!(
                abi3_scope_check(mode, "aarch64-unknown-linux-gnu").is_ok(),
                "{mode:?} should be in scope"
            );
        }
        for mode in [
            PlanMode::ExtensionDefault,
            PlanMode::RequiresExplicitCompatibility,
            PlanMode::Unresolved,
        ] {
            let err = abi3_scope_check(mode, "aarch64-unknown-linux-gnu")
                .expect_err("out-of-scope mode must refuse");
            let message = err.to_string();
            assert!(message.contains("abi3-only"), "{mode:?}: {message}");
            assert!(
                message.contains("soldr maturin build"),
                "{mode:?}: {message}"
            );
        }
    });

    timed_test!(global_flags_precede_the_subcommand_in_the_reentry_argv, {
        // Host triple on purpose: a cross target would send the abi3 gate
        // through `cargo metadata`, and this test is about argv ordering.
        let args = WheelArgs {
            target: Some(crate::pyo3_detect::host_triple().to_string()),
            rest: Vec::new(),
        };
        let argv = maturin_invocation(&args, true, true).expect("invocation should build");
        assert_eq!(argv[0], "--no-cache");
        assert_eq!(argv[1], "--trust-inherited-soldr-env");
        assert_eq!(argv[2], "maturin");
        assert_eq!(argv[3], "build");

        let argv = maturin_invocation(&args, false, false).expect("invocation should build");
        assert_eq!(argv[0], "maturin");
    });
}
