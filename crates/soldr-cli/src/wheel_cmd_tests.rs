//! Unit coverage split from `wheel_cmd.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;

/// A host that is deliberately not a legal target spelling, so every
/// `build()` below is unambiguously a *cross* build regardless of which
/// machine runs the suite. Nothing about the tag policy may depend on the
/// test host — that dependency is the bug this module now guards.
const CROSS_HOST: &str = "never-equal-to-any-target";

/// An x86_64 Linux host: the catalogue GNU bundle runs here.
fn x86_64_linux() -> WheelHost {
    WheelHost {
        triple: "x86_64-unknown-linux-gnu".to_string(),
        gnu_bundle_fitness: crate::fetch::catalogue_linux_host::BundleHostFitness::Runnable,
        glibc_version: Some("2.39".to_string()),
    }
}

/// A native aarch64 Linux host: every catalogue GNU bundle is
/// x86_64-hosted (soldr#2874), so it cannot run here.
fn aarch64_linux() -> WheelHost {
    WheelHost {
        triple: "aarch64-unknown-linux-gnu".to_string(),
        gnu_bundle_fitness: crate::fetch::catalogue_linux_host::BundleHostFitness::WrongArch,
        glibc_version: Some("2.39".to_string()),
    }
}

fn host_named(triple: &str) -> WheelHost {
    WheelHost {
        triple: triple.to_string(),
        gnu_bundle_fitness: if triple == "x86_64-unknown-linux-gnu" {
            crate::fetch::catalogue_linux_host::BundleHostFitness::Runnable
        } else {
            crate::fetch::catalogue_linux_host::BundleHostFitness::WrongArch
        },
        glibc_version: None,
    }
}

fn wheel_args(target: Option<&str>, release: bool, host_glibc: bool, rest: &[&str]) -> WheelArgs {
    WheelArgs {
        target: target.map(str::to_string),
        release,
        host_glibc,
        rest: rest.iter().map(|s| s.to_string()).collect(),
    }
}

fn maturin_build_argv_for_host(
    target: Option<&str>,
    release: bool,
    rest: &[String],
    host: &str,
) -> Result<Vec<String>, SoldrError> {
    let args = WheelArgs {
        target: target.map(str::to_string),
        release,
        host_glibc: false,
        rest: rest.to_vec(),
    };
    plan_for_host(&args, &host_named(host)).map(|plan| plan.argv)
}

/// Release + cross: the one shape in which soldr may claim a floor.
fn build(target: &str, rest: &[&str]) -> Vec<String> {
    let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
    maturin_build_argv_for_host(Some(target), true, &rest, CROSS_HOST).expect("argv should build")
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

// ---- soldr#3432: a release linux-gnu wheel always enforces 2.17.

#[test]
fn a_host_target_release_gnu_wheel_prepares_and_claims_manylinux_2_17() {
    // The RED case from soldr#3432: on an x86_64 Linux host, a host-target
    // release wheel used to skip target preparation and claim nothing, so
    // it linked the runner's glibc (manylinux_2_34+ on modern distros).
    let plan = plan_for_host(
        &wheel_args(Some("x86_64-unknown-linux-gnu"), true, false, &[]),
        &x86_64_linux(),
    )
    .expect("host-target release wheel must plan");
    assert!(
        plan.prepare_host_target,
        "the catalogue sysroot must be prepared for the host target: {plan:?}"
    );
    assert_eq!(
        flag_value(&plan.argv, "--compatibility"),
        Some("manylinux_2_17"),
        "{plan:?}"
    );
    // `--target` omitted is the same request.
    let implicit = plan_for_host(&wheel_args(None, true, false, &[]), &x86_64_linux())
        .expect("implicit host target");
    assert_eq!(implicit, plan);
}

#[test]
fn the_release_gnu_plan_emits_the_glibc_2_17_info_line() {
    let plan = plan_for_host(&wheel_args(None, true, false, &[]), &x86_64_linux()).expect("plan");
    let notice = plan
        .notice
        .expect("a release gnu wheel always announces its floor");
    assert_eq!(
        notice.message(),
        "soldr: info: building release wheel against glibc 2.17 (manylinux_2_17) for \
         maximum Linux compatibility; pass --host-glibc to link against this host's glibc \
         instead"
    );
    // Cross: same floor, but --host-glibc is not offered where it is refused.
    let cross = plan_for_host(
        &wheel_args(Some("aarch64-unknown-linux-gnu"), true, false, &[]),
        &x86_64_linux(),
    )
    .expect("cross plan");
    assert!(
        !cross.prepare_host_target,
        "cross prep is the dispatcher's own gate"
    );
    let message = cross.notice.expect("cross gnu notice").message();
    assert!(
        message.contains("aarch64-unknown-linux-gnu against glibc 2.17"),
        "{message}"
    );
    assert!(message.contains("only to a host-target build"), "{message}");
}

#[test]
fn the_info_line_is_green_only_when_color_is_on() {
    let notice = GlibcNotice::Catalogue {
        cross_target: None,
        tag: Some("manylinux_2_17"),
    };
    let plain = notice.render(false);
    assert_eq!(plain, notice.message());
    assert!(!plain.contains('\x1b'), "{plain:?}");
    let green = notice.render(true);
    assert_eq!(green, format!("\x1b[32m{}\x1b[0m", notice.message()));
}

#[test]
fn no_color_and_a_non_tty_stderr_give_plain_text() {
    // The rule `maturin_invocation` feeds into `GlibcNotice::render`.
    // soldr#3437: the predicate moved to `crate::color_choice` and gained its
    // third input — GitHub Actions colors a non-TTY stream — so every case
    // below now names the stream's terminality *and* the Actions bit.
    use crate::color_choice::enabled;
    assert!(
        enabled(false, false, true),
        "a terminal with no NO_COLOR is green"
    );
    assert!(!enabled(true, false, true), "NO_COLOR wins over a terminal");
    assert!(
        !enabled(true, true, true),
        "NO_COLOR wins over GitHub Actions too"
    );
    assert!(
        enabled(false, true, false),
        "GitHub Actions colors a captured stream (soldr#3437)"
    );
    assert!(
        !enabled(false, false, false),
        "a redirected stderr is plain"
    );
    assert!(!enabled(true, false, false));
}

#[test]
fn dev_and_non_gnu_wheels_print_no_floor_notice() {
    let dev =
        plan_for_host(&wheel_args(None, false, false, &[]), &x86_64_linux()).expect("dev plan");
    assert_eq!(dev.notice, None);
    assert!(!dev.prepare_host_target);
    for target in ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"] {
        let plan = plan_for_host(&wheel_args(Some(target), true, false, &[]), &x86_64_linux())
            .expect("non-linux plan");
        assert_eq!(plan.notice, None, "{target}");
        assert!(!plan.prepare_host_target, "{target}");
    }
}

#[test]
fn host_glibc_opts_out_of_the_floor_with_its_own_info_line() {
    let plan = plan_for_host(&wheel_args(None, true, true, &[]), &x86_64_linux())
        .expect("--host-glibc plan");
    assert!(!plan.prepare_host_target, "{plan:?}");
    assert!(plan.argv.contains(&"--release".to_string()), "{plan:?}");
    assert_eq!(
        flag_value(&plan.argv, "--compatibility"),
        Some("pypi"),
        "only the bytes-derived tag is backed without the sysroot: {plan:?}"
    );
    assert_eq!(
        plan.notice.as_ref().map(GlibcNotice::message).as_deref(),
        Some(
            "soldr: info: --host-glibc: building wheel against this host's glibc, not \
             glibc 2.17; it will require glibc 2.39 or newer, and maturin tags it from its \
             bytes instead of manylinux_2_17"
        )
    );
    // An undetectable version still produces an honest line.
    let mut host = x86_64_linux();
    host.glibc_version = None;
    let plan = plan_for_host(&wheel_args(None, true, true, &[]), &host).expect("plan");
    let message = plan.notice.expect("notice").message();
    assert!(message.contains("version not detected"), "{message}");
}

#[test]
fn host_glibc_is_not_the_default() {
    let args = <WheelArgs as Default>::default();
    assert!(!args.host_glibc);
}

#[test]
fn host_glibc_with_a_cross_target_is_refused() {
    let err = plan_for_host(
        &wheel_args(Some("aarch64-unknown-linux-gnu"), true, true, &[]),
        &x86_64_linux(),
    )
    .expect_err("--host-glibc has no meaning for a cross target");
    let message = err.to_string();
    assert!(message.contains("--host-glibc"), "{message}");
    assert!(message.contains("cross target"), "{message}");
    assert!(message.contains("Drop --host-glibc"), "{message}");
}

#[test]
fn host_glibc_with_a_non_glibc_target_is_refused() {
    let err = plan_for_host(
        &wheel_args(None, true, true, &[]),
        &host_named("aarch64-apple-darwin"),
    )
    .expect_err("macOS has no glibc");
    assert!(
        err.to_string()
            .contains("only applies to a `*-linux-gnu` wheel"),
        "{err}"
    );
}

#[test]
fn aarch64_host_target_release_wheel_is_refused_not_silently_degraded() {
    // No aarch64-hosted catalogue bundle exists (soldr#2874), so the floor
    // cannot be enforced here. Refuse with both remedies rather than fall
    // back to the host glibc under a `pypi` tag.
    let err = plan_for_host(&wheel_args(None, true, false, &[]), &aarch64_linux())
        .expect_err("aarch64 host-target release wheel must be refused");
    let message = err.to_string();
    assert!(message.contains("soldr#2874"), "{message}");
    assert!(message.contains("soldr#3432"), "{message}");
    assert!(message.contains("x86_64-unknown-linux-gnu"), "{message}");
    assert!(message.contains("--host-glibc"), "{message}");

    // Both remedies work: a dev wheel, and the explicit opt-out.
    assert!(plan_for_host(&wheel_args(None, false, false, &[]), &aarch64_linux()).is_ok());
    let opted_out = plan_for_host(&wheel_args(None, true, true, &[]), &aarch64_linux())
        .expect("--host-glibc is the explicit opt-out");
    assert_eq!(flag_value(&opted_out.argv, "--compatibility"), Some("pypi"));
}

#[test]
fn a_musl_os_host_target_release_wheel_names_the_libc_reason() {
    // soldr#3435: a `-linux-gnu` host triple on an OS whose runtime libc is
    // musl cannot start the glibc-dynamic GNU bundle. The refusal must say
    // so rather than blame the architecture.
    let host = WheelHost {
        gnu_bundle_fitness: crate::fetch::catalogue_linux_host::BundleHostFitness::WrongLibc,
        ..x86_64_linux()
    };
    let err = plan_for_host(&wheel_args(None, true, false, &[]), &host)
        .expect_err("a musl OS cannot run the GNU bundle");
    let message = err.to_string();
    assert!(message.contains("musl libc"), "{message}");
    assert!(message.contains("glibc's loader"), "{message}");
    assert!(message.contains("soldr#3435"), "{message}");
    assert!(!message.contains("soldr#2874"), "{message}");
    assert!(message.contains("--host-glibc"), "{message}");
}

#[test]
fn musl_host_target_release_wheel_is_unchanged() {
    // Intended (soldr#3435): soldr#3432's host-target preparation does not
    // extend to musl, so a host-target musl release wheel keeps `pypi`.
    let host = host_named("x86_64-unknown-linux-musl");
    let plan = plan_for_host(&wheel_args(None, true, false, &[]), &host).expect("plan");
    assert!(!plan.prepare_host_target);
    assert_eq!(flag_value(&plan.argv, "--compatibility"), Some("pypi"));
    assert_eq!(plan.notice, None);
}

#[test]
fn a_caller_supplied_tag_keeps_the_floor_but_the_notice_claims_no_tag() {
    let plan = plan_for_host(
        &wheel_args(None, true, false, &["--compatibility", "linux"]),
        &x86_64_linux(),
    )
    .expect("plan");
    assert!(plan.prepare_host_target);
    assert_eq!(flag_value(&plan.argv, "--compatibility"), Some("linux"));
    let message = plan.notice.expect("notice").message();
    assert!(!message.contains("manylinux_2_17"), "{message}");
    assert!(message.contains("glibc 2.17"), "{message}");
}

#[test]
fn the_dispatcher_prepares_a_host_target_only_when_a_wheel_plan_asked() {
    // A value no real host/target uses, so no other test can collide.
    let target = "x86_64-unknown-linux-gnu-soldr-3432-test";
    assert!(!maturin_target_needs_prep(target, target));
    request_host_target_prep(target);
    assert!(maturin_target_needs_prep(target, target));
    // Cross targets are always prepared, requested or not.
    assert!(maturin_target_needs_prep(
        "aarch64-unknown-linux-gnu",
        target
    ));
}

#[test]
fn floor_claim_needs_release_and_no_opt_out() {
    let host = x86_64_linux();
    let tag = |target: Option<&str>, release: bool, host_glibc: bool| {
        plan_for_host(&wheel_args(target, release, host_glibc, &[]), &host)
            .map(|plan| flag_value(&plan.argv, "--compatibility").map(str::to_string))
            .expect("plan")
    };
    let cross = Some("aarch64-unknown-linux-gnu");
    assert_eq!(tag(cross, true, false).as_deref(), Some("manylinux_2_17"));
    assert_eq!(tag(cross, false, false).as_deref(), Some("pypi"));
    assert_eq!(tag(None, true, false).as_deref(), Some("manylinux_2_17"));
    assert_eq!(tag(None, false, false).as_deref(), Some("pypi"));
    assert_eq!(tag(None, true, true).as_deref(), Some("pypi"));
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
    // soldr#3390: the old `"soldr wheel: " + reworded-body` shape doubled
    // the verb (`soldr wheel: soldr wheel --target ...`). Exactly one.
    assert_eq!(message.matches("soldr wheel").count(), 1, "{message}");
    assert!(message.contains("soldr wheel --target"), "{message}");
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
    let err =
        maturin_build_argv_for_host(Some("x86_64-unknown-linux-gnu.2.17"), true, &[], CROSS_HOST)
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
        // A dev wheel: a host-target release gnu wheel would be refused on
        // a native aarch64 runner (soldr#3432), and this is about argv order.
        release: false,
        host_glibc: false,
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

/// zackees/soldr#3468: bundled bins are built with the same toolchain as the
/// extension, so a host-target release gnu wheel's CLI gets glibc 2.17 too.
#[test]
fn bundled_bins_follow_the_extensions_target_preparation_and_profile() {
    let x86 = "x86_64-unknown-linux-gnu";
    let release =
        plan_for_host(&wheel_args(Some(x86), true, false, &[]), &x86_64_linux()).expect("plan");
    assert_eq!(release.bundle.target.as_deref(), Some(x86), "{release:?}");
    assert_eq!(release.bundle.profile_args, vec!["--release".to_string()]);

    let host_glibc =
        plan_for_host(&wheel_args(Some(x86), true, true, &[]), &x86_64_linux()).expect("plan");
    assert_eq!(host_glibc.bundle.target, None, "{host_glibc:?}");

    let dev = plan_for_host(&wheel_args(None, false, false, &[]), &x86_64_linux()).expect("plan");
    assert_eq!(dev.bundle.target, None, "{dev:?}");
    assert!(dev.bundle.profile_args.is_empty());

    let cross = plan_for_host(
        &wheel_args(Some("aarch64-unknown-linux-gnu"), true, false, &[]),
        &x86_64_linux(),
    )
    .expect("plan");
    assert_eq!(
        cross.bundle.target.as_deref(),
        Some("aarch64-unknown-linux-gnu")
    );
}

#[test]
fn forwarded_short_release_flag_gets_release_policy() {
    // soldr#3636: `-r` / `--profile release` are maturin's release spellings.
    let cases: [&[&str]; 3] = [&["-r"], &["--profile", "release"], &["--profile=release"]];
    for rest in cases {
        let plan = plan_for_host(&wheel_args(None, false, false, rest), &x86_64_linux())
            .expect("forwarded release profile must plan");
        assert!(plan.prepare_host_target, "{rest:?}: {plan:?}");
        assert_eq!(
            flag_value(&plan.argv, "--compatibility"),
            Some("manylinux_2_17"),
            "{rest:?}: {plan:?}"
        );
        assert_eq!(
            plan.bundle.profile_args,
            vec!["--release".to_string()],
            "{rest:?}: {plan:?}"
        );
        // soldr must not add its own `--release`; the caller's spelling is
        // forwarded once.
        assert_eq!(
            plan.argv.iter().filter(|a| *a == "--release").count(),
            0,
            "{rest:?}: {plan:?}"
        );
        assert_eq!(
            plan.argv.iter().filter(|a| *a == rest[0]).count(),
            1,
            "{rest:?}: {plan:?}"
        );
    }
}

#[test]
fn forwarded_short_release_with_debug_is_refused() {
    assert!(plan_for_host(
        &wheel_args(None, false, false, &["-r", "--debug"]),
        &x86_64_linux()
    )
    .is_err());
}

#[test]
fn profile_dev_is_not_release() {
    let plan = plan_for_host(
        &wheel_args(None, false, false, &["--profile", "dev"]),
        &x86_64_linux(),
    )
    .expect("dev profile must plan");
    assert!(!plan.prepare_host_target, "{plan:?}");
}
