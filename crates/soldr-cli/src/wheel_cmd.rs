//! `soldr wheel [--release] [--target <triple>]` — the blessed Python wheel
//! surface (soldr#2139 gap 1).
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
//! # Grammar
//!
//! ```text
//! soldr wheel                          # quick dev wheel, host target
//! soldr wheel --release                # release wheel, host target
//! soldr wheel --release --target XXX   # release wheel, cross target
//! ```
//!
//! `--release` is opt-in, matching `cargo` and `soldr build`: the default is a
//! fast dev-profile wheel. `--target` defaults to the host triple.
//!
//! # Note on glibc floors — only claim a floor soldr enforced
//!
//! `--compatibility manylinux_2_17` is a *tag*, and the suffixed-triple floor
//! (`...-linux-gnu.2.17`, soldr#2202) means "ask zig for this floor", never
//! "guarantee this floor" — the effective floor is the max of what zig was
//! asked for and every symbol the vendored C dependencies reference. The
//! suffixed spelling is therefore rejected here rather than being quietly
//! accepted into a wheel tag that would read as a promise.
//!
//! The same honesty rule now governs the tag soldr emits at all. The first cut
//! of this module tagged **every** `*-linux-gnu` target `manylinux_2_17`,
//! including a host-target build — but the maturin execution path only runs
//! `target_lifecycle::prepare_for_invocation` when the target differs from the
//! host (`soldr_main.rs`, the `maturin_build && maturin_target !=
//! host_triple()` gate). On a host build no catalogue sysroot is mounted, the
//! binary links against whatever glibc the machine has (2.39 on ubuntu-24.04),
//! and the 2.17 tag is a claim nothing backed. `verify_wheel_glibc.py` exists
//! precisely because pip *trusts* that claim and installs the wheel anyway.
//!
//! So the floor is claimed only when soldr actually enforced it — a release
//! build on the cross path — and otherwise soldr passes `--compatibility pypi`,
//! maturin's "work the tag out from the bytes" pseudo-option. A dev wheel
//! tagged from reality is correct and useful; a dev wheel tagged
//! `manylinux_2_17` is a lie pip will act on.
//!
//! Note that maturin does **not** paper over the bad case: with an explicit
//! `--compatibility manylinux_2_17` and an ELF needing `GLIBC_2.39`,
//! `auditwheel_rs` (maturin `src/auditwheel/linux.rs`) returns
//! `VersionedSymbolTooNewError` from the explicit-tag branch and the build
//! fails with "Error ensuring manylinux_2_17 compliance" — it downgrades only
//! when *no* tag was requested. `AuditWheelMode::Repair` is maturin's
//! `#[default]`, so `release-auto.yml`'s explicit `--auditwheel repair`
//! restates the default and its absence here changes nothing. The old
//! behaviour therefore did not ship a mis-tagged wheel; it made
//! `soldr wheel --target <host-linux-gnu>` fail on any modern distro, with a
//! maturin compliance error that named neither soldr nor the missing prep.

use crate::core::SoldrError;
use crate::pyo3_detect::PlanMode;

/// Arguments for `soldr wheel`.
///
/// `--target` and `--release` must precede any passthrough arguments, because
/// everything after the first free argument is forwarded to maturin verbatim.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct WheelArgs {
    /// Target triple or friendly alias (for example `linux-arm64`).
    /// Defaults to the host triple.
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,
    /// Build with the release profile. Default is a quick dev-profile wheel,
    /// matching `cargo` and `soldr build`.
    #[arg(long)]
    pub release: bool,
    /// Extra arguments forwarded verbatim to `maturin build`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

/// Whether soldr actually *enforced* a platform floor for this build, and may
/// therefore stamp a `manylinux` / `musllinux` claim on the wheel.
///
/// Two conditions, both load-bearing:
///
/// * **cross** — `target_lifecycle::prepare_for_invocation` runs on the
///   maturin path only when the target differs from the host
///   (`soldr_main.rs`). That preparation is what mounts the catalogue sysroot
///   whose glibc defines the floor. On a host build nothing is mounted and
///   the floor is whatever the machine happens to have.
/// * **release** — a dev-profile wheel is a local artifact, not a
///   distributable one. soldr does not stamp a distribution promise on a build
///   whose whole point is to be quick.
pub fn floor_claim_is_backed(triple: &str, host: &str, release: bool) -> bool {
    release && triple != host
}

/// maturin's `--compatibility` value for a resolved Rust target triple.
///
/// When the floor claim is backed this mirrors the release lane
/// (`release-auto.yml`): linux-gnu wheels are tagged `manylinux_2_17`,
/// linux-musl wheels `musllinux_1_2`. Otherwise — and for every non-Linux
/// target, which has no such floor to claim — soldr passes `pypi`, maturin's
/// pseudo-option meaning "derive the platform tag from the bytes and validate
/// the resulting filename for PyPI". That is a description, not a promise.
pub fn compatibility_for_target(triple: &str, floor_backed: bool) -> &'static str {
    if !floor_backed {
        "pypi"
    } else if triple.contains("-linux-musl") {
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

/// Pure argv builder: `(target, release, passthrough) -> maturin argv`.
///
/// No I/O, no env reads beyond the host triple — this is the piece worth
/// unit-testing, and it is the only place the wheel surface decides anything.
pub fn maturin_build_argv(
    target: Option<&str>,
    release: bool,
    rest: &[String],
) -> Result<Vec<String>, SoldrError> {
    maturin_build_argv_for_host(target, release, rest, crate::pyo3_detect::host_triple())
}

/// [`maturin_build_argv`] with the host triple injected, so the tag policy can
/// be tested from any machine rather than only from the host it describes.
pub fn maturin_build_argv_for_host(
    target: Option<&str>,
    release: bool,
    rest: &[String],
    host: &str,
) -> Result<Vec<String>, SoldrError> {
    let requested = target
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(host);

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
             Use `soldr wheel --release --target {base}` (tagged \
             `{}`), or `soldr build --target {base}.{floor}` for a bare binary.",
            compatibility_for_target(base, floor_claim_is_backed(base, host, true))
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

    // `--debug` is maturin's spelling for "not --release". A caller who wrote
    // both is asking for two different profiles; say so rather than picking
    // one and building something they did not ask for.
    let release_in_rest = has_flag(rest, "--release");
    let debug_in_rest = has_flag(rest, "--debug");
    if release && debug_in_rest {
        return Err(SoldrError::Other(
            "soldr wheel: `--release` and a forwarded `--debug` ask for different profiles. \
             Drop one — `soldr wheel` alone already builds the dev profile."
                .to_string(),
        ));
    }
    let is_release = release || release_in_rest;

    let mut argv = vec!["maturin".to_string(), "build".to_string()];
    if is_release && !release_in_rest {
        argv.push("--release".to_string());
    }
    if !has_flag(rest, "--compatibility") && !has_flag(rest, "--manylinux") {
        argv.push("--compatibility".to_string());
        let backed = floor_claim_is_backed(&triple, host, is_release);
        argv.push(compatibility_for_target(&triple, backed).to_string());
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
pub fn abi3_scope_check(
    mode: PlanMode,
    target: &str,
    diagnostic: Option<&str>,
) -> Result<(), SoldrError> {
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
        PlanMode::Unresolved => {
            // Surface the probe's real failure (soldr#2576): the summary
            // alone sent users chasing `cargo metadata` by hand, which
            // succeeds through the front door and proves nothing about
            // this probe's environment.
            let detail = diagnostic
                .map(|text| format!("\n  probe error: {text}"))
                .unwrap_or_default();
            Err(SoldrError::Other(format!(
                "soldr wheel: could not read Cargo metadata, so soldr cannot prove this \
                 workspace is abi3-safe for `{target}`. The first cut of `soldr wheel` is \
                 abi3-only (soldr#2139); run `soldr maturin build --target {target}` to build \
                 anyway.{detail}"
            )))
        }
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
    let build = maturin_build_argv(args.target.as_deref(), args.release, &args.rest)?;
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
        abi3_scope_check(plan.mode, &triple, plan.diagnostic.as_deref())?;
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

    /// A host that is deliberately not a legal target spelling, so every
    /// `build()` below is unambiguously a *cross* build regardless of which
    /// machine runs the suite. Nothing about the tag policy may depend on the
    /// test host — that dependency is the bug this module now guards.
    const CROSS_HOST: &str = "never-equal-to-any-target";

    /// Release + cross: the one shape in which soldr may claim a floor.
    fn build(target: &str, rest: &[&str]) -> Vec<String> {
        let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
        maturin_build_argv_for_host(Some(target), true, &rest, CROSS_HOST)
            .expect("argv should build")
    }

    fn build_on(target: &str, release: bool, host: &str, rest: &[&str]) -> Vec<String> {
        let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
        maturin_build_argv_for_host(Some(target), release, &rest, host).expect("argv should build")
    }

    fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        argv.iter()
            .position(|arg| arg == flag)
            .and_then(|idx| argv.get(idx + 1))
            .map(String::as_str)
    }

    #[test]
    fn gnu_target_is_tagged_manylinux_2_17() {
        let argv = build("x86_64-unknown-linux-gnu", &[]);
        assert_eq!(argv[0], "maturin");
        assert_eq!(argv[1], "build");
        assert!(argv.contains(&"--release".to_string()), "{argv:?}");
        assert_eq!(flag_value(&argv, "--compatibility"), Some("manylinux_2_17"));
        assert_eq!(
            flag_value(&argv, "--target"),
            Some("x86_64-unknown-linux-gnu")
        );
    }

    #[test]
    fn musl_target_is_tagged_musllinux_1_2() {
        let argv = build("aarch64-unknown-linux-musl", &[]);
        assert_eq!(flag_value(&argv, "--compatibility"), Some("musllinux_1_2"));
        assert_eq!(
            flag_value(&argv, "--target"),
            Some("aarch64-unknown-linux-musl")
        );
    }

    #[test]
    fn non_linux_targets_keep_maturin_pypi_tagging() {
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
    }

    #[test]
    fn gnueabihf_is_still_a_gnu_target() {
        assert_eq!(
            compatibility_for_target("armv7-unknown-linux-gnueabihf", true),
            "manylinux_2_17"
        );
        assert_eq!(
            compatibility_for_target("armv7-unknown-linux-gnueabihf", false),
            "pypi"
        );
    }

    // ---- soldr#2139 follow-up: the tag is a claim, so only make backed ones.

    #[test]
    fn a_dev_wheel_does_not_claim_a_manylinux_floor() {
        // Same target, same host, only the profile differs. `--release` is the
        // difference between "soldr prepared and verified a distributable
        // build" and "give me something quick".
        let dev = build_on("aarch64-unknown-linux-gnu", false, CROSS_HOST, &[]);
        assert!(!dev.contains(&"--release".to_string()), "{dev:?}");
        assert_eq!(
            flag_value(&dev, "--compatibility"),
            Some("pypi"),
            "a dev wheel must be tagged from the bytes, not from a promise: {dev:?}"
        );

        let release = build_on("aarch64-unknown-linux-gnu", true, CROSS_HOST, &[]);
        assert!(release.contains(&"--release".to_string()), "{release:?}");
        assert_eq!(
            flag_value(&release, "--compatibility"),
            Some("manylinux_2_17")
        );
    }

    #[test]
    fn a_host_target_wheel_does_not_claim_a_manylinux_floor() {
        // The regression this guards: `prepare_for_invocation` is gated on
        // `maturin_target != host_triple()` in soldr_main.rs, so a host-target
        // linux-gnu build mounts no catalogue sysroot and links against the
        // machine's own glibc (2.39 on ubuntu-24.04). Claiming 2.17 there is
        // exactly what `verify_wheel_glibc.py` was written to catch.
        let host = "x86_64-unknown-linux-gnu";
        let argv = build_on(host, true, host, &[]);
        assert_eq!(
            flag_value(&argv, "--compatibility"),
            Some("pypi"),
            "no target prep ran, so there is no floor to claim: {argv:?}"
        );
        // ...and the same triple from a different host does claim it.
        let cross = build_on(host, true, "aarch64-apple-darwin", &[]);
        assert_eq!(
            flag_value(&cross, "--compatibility"),
            Some("manylinux_2_17")
        );
    }

    #[test]
    fn floor_claim_needs_both_release_and_cross() {
        let target = "aarch64-unknown-linux-gnu";
        assert!(floor_claim_is_backed(target, CROSS_HOST, true));
        assert!(!floor_claim_is_backed(target, CROSS_HOST, false));
        assert!(!floor_claim_is_backed(target, target, true));
        assert!(!floor_claim_is_backed(target, target, false));
    }

    #[test]
    fn the_default_wheel_is_a_quick_dev_build() {
        // `soldr wheel` with no flags at all: host target, dev profile.
        let argv = maturin_build_argv_for_host(None, false, &[], "x86_64-unknown-linux-gnu")
            .expect("bare `soldr wheel` must work");
        assert!(!argv.contains(&"--release".to_string()), "{argv:?}");
        assert_eq!(
            flag_value(&argv, "--target"),
            Some("x86_64-unknown-linux-gnu"),
            "--target defaults to the host: {argv:?}"
        );
        assert_eq!(flag_value(&argv, "--compatibility"), Some("pypi"));

        // A blank/whitespace --target is the same request as none at all.
        let argv = maturin_build_argv_for_host(Some("  "), false, &[], "aarch64-apple-darwin")
            .expect("blank --target falls back to the host");
        assert_eq!(flag_value(&argv, "--target"), Some("aarch64-apple-darwin"));
    }

    #[test]
    fn release_and_a_forwarded_debug_are_refused_not_reconciled() {
        let rest = vec!["--debug".to_string()];
        let err = maturin_build_argv_for_host(Some("linux-arm64"), true, &rest, CROSS_HOST)
            .expect_err("contradictory profiles must be refused");
        assert!(err.to_string().contains("different profiles"), "{err}");
    }

    #[test]
    fn friendly_aliases_resolve_to_rust_triples() {
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
    }

    #[test]
    fn alias_resolution_picks_the_musl_tag_for_musl_aliases() {
        let argv = build("linux-arm64-musl", &[]);
        assert_eq!(
            flag_value(&argv, "--target"),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(flag_value(&argv, "--compatibility"), Some("musllinux_1_2"));
    }

    #[test]
    fn passthrough_args_are_forwarded_after_soldr_defaults() {
        let argv = build("linux-x64", &["--out", "dist", "--locked"]);
        let tail = &argv[argv.len() - 3..];
        assert_eq!(tail, ["--out", "dist", "--locked"]);
    }

    #[test]
    fn caller_flags_are_honoured_and_never_duplicated() {
        let argv = build("x86_64-unknown-linux-gnu", &["--compatibility", "linux"]);
        assert_eq!(
            argv.iter().filter(|arg| *arg == "--compatibility").count(),
            1,
            "{argv:?}"
        );
        assert_eq!(flag_value(&argv, "--compatibility"), Some("linux"));

        // A forwarded `--debug` on an otherwise-default (dev) wheel is
        // redundant but harmless, and must not produce a second profile flag.
        let argv = build_on("x86_64-unknown-linux-gnu", false, CROSS_HOST, &["--debug"]);
        assert!(!argv.iter().any(|arg| arg == "--release"), "{argv:?}");

        // A forwarded `--release` is equivalent to the flag: one copy, and it
        // still backs the floor claim.
        let argv = build_on(
            "x86_64-unknown-linux-gnu",
            false,
            CROSS_HOST,
            &["--release"],
        );
        assert_eq!(
            argv.iter().filter(|arg| *arg == "--release").count(),
            1,
            "{argv:?}"
        );
        assert_eq!(flag_value(&argv, "--compatibility"), Some("manylinux_2_17"));

        let argv = build("x86_64-unknown-linux-gnu", &["--manylinux=2014"]);
        assert!(!argv.iter().any(|arg| arg == "--compatibility"), "{argv:?}");
    }

    #[test]
    fn unknown_target_errors_with_a_suggestion() {
        let err = maturin_build_argv_for_host(Some("linux-arm65"), true, &[], CROSS_HOST)
            .expect_err("unknown target");
        let message = err.to_string();
        assert!(message.contains("soldr wheel"), "{message}");
        assert!(message.contains("linux-arm65"), "{message}");
        // AliasError carries a Jaro-Winkler suggestion; it must survive the
        // wrap so the user is not left guessing.
        assert!(message.contains("linux-arm64"), "{message}");
    }

    #[test]
    fn ambiguous_and_32bit_targets_are_refused_not_degraded() {
        let err = maturin_build_argv_for_host(Some("linux-arm"), true, &[], CROSS_HOST)
            .expect_err("ambiguous target");
        assert!(err.to_string().contains("linux-arm64"), "{err}");
        let err = maturin_build_argv_for_host(Some("win-x86"), true, &[], CROSS_HOST)
            .expect_err("32-bit target");
        assert!(err.to_string().contains("32-bit"), "{err}");
    }

    #[test]
    fn glibc_floor_targets_are_refused_with_the_ask_not_guarantee_reason() {
        let err = maturin_build_argv_for_host(
            Some("x86_64-unknown-linux-gnu.2.17"),
            true,
            &[],
            CROSS_HOST,
        )
        .expect_err("glibc floor is out of scope for wheels");
        let message = err.to_string();
        assert!(message.contains("not a guarantee"), "{message}");
        assert!(message.contains("soldr build --target"), "{message}");
    }

    #[test]
    fn a_second_target_in_the_passthrough_is_refused() {
        let rest = vec!["--target".to_string(), "aarch64-apple-darwin".to_string()];
        let err = maturin_build_argv_for_host(Some("linux-x64"), true, &rest, CROSS_HOST)
            .expect_err("duplicate --target");
        assert!(err.to_string().contains("pass the target once"), "{err}");
    }

    #[test]
    fn abi3_gate_allows_only_interpreter_free_modes() {
        for mode in [
            PlanMode::Native,
            PlanMode::NoPyo3,
            PlanMode::Abi3NoPython,
            PlanMode::ModernWindowsRawDylib,
            PlanMode::CompatibilitySysroot,
            PlanMode::CallerConfigured,
        ] {
            assert!(
                abi3_scope_check(mode, "aarch64-unknown-linux-gnu", None).is_ok(),
                "{mode:?} should be in scope"
            );
        }
        for mode in [
            PlanMode::ExtensionDefault,
            PlanMode::RequiresExplicitCompatibility,
            PlanMode::Unresolved,
        ] {
            let err = abi3_scope_check(mode, "aarch64-unknown-linux-gnu", None)
                .expect_err("out-of-scope mode must refuse");
            let message = err.to_string();
            assert!(message.contains("abi3-only"), "{mode:?}: {message}");
            assert!(
                message.contains("soldr maturin build"),
                "{mode:?}: {message}"
            );
        }
    }

    #[test]
    fn global_flags_precede_the_subcommand_in_the_reentry_argv() {
        // Host triple on purpose: a cross target would send the abi3 gate
        // through `cargo metadata`, and this test is about argv ordering.
        let args = WheelArgs {
            target: Some(crate::pyo3_detect::host_triple().to_string()),
            release: true,
            rest: Vec::new(),
        };
        let argv = maturin_invocation(&args, true, true).expect("invocation should build");
        assert_eq!(argv[0], "--no-cache");
        assert_eq!(argv[1], "--trust-inherited-soldr-env");
        assert_eq!(argv[2], "maturin");
        assert_eq!(argv[3], "build");

        let argv = maturin_invocation(&args, false, false).expect("invocation should build");
        assert_eq!(argv[0], "maturin");
    }
}
