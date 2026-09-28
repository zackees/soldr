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
//! soldr wheel                              # quick dev wheel, host target
//! soldr wheel --release                    # release wheel, host target
//! soldr wheel --release --target XXX       # release wheel, cross target
//! soldr wheel --release --host-glibc       # release wheel, host glibc floor
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
//! The same honesty rule governs the tag soldr emits at all: soldr claims
//! `manylinux_2_17` only for a build in which
//! `target_lifecycle::prepare_for_invocation` ran and mounted the catalogue
//! glibc-2.17 sysroot that *creates* the floor. `verify_wheel_glibc.py` exists
//! precisely because pip *trusts* that claim and installs the wheel anyway.
//!
//! The maturin execution path prepares a target on its own only when the
//! target differs from the host. soldr#3432 closed the gap that left: a
//! `--release` `*-linux-gnu` wheel **always** gets target preparation, host
//! target included, via `maturin_target_needs_prep`. A release wheel is the
//! thing that goes to PyPI, and it must never silently inherit the build
//! machine's glibc floor (2.39 on ubuntu-24.04). So "release + linux-gnu"
//! means an enforced 2.17 floor, and soldr says so on stderr with one green
//! `info` line ([`GlibcNotice`]) so the floor is never a silent choice.
//!
//! The opt-out is explicit: `--host-glibc` skips the forced preparation for a
//! host-target build, links against this host's glibc, and passes
//! `--compatibility pypi` — maturin's "work the tag out from the bytes"
//! pseudo-option — because that is the only claim soldr can back. A dev wheel
//! gets the same `pypi` treatment: it is a local artifact, and a dev wheel
//! tagged `manylinux_2_17` without the sysroot would be a lie pip acts on.
//!
//! A host that cannot run the catalogue GNU bundle (every bundle is
//! x86_64-hosted, soldr#2874 — so a native aarch64 Linux host) cannot enforce
//! the floor for a host-target release wheel. soldr refuses rather than
//! falling back: the same rule `target_lifecycle::decide_gnu_bundle` applies to
//! an explicit `.2.17` floor request, where a silent success is worse than a
//! failure because the artifact would read as a promise. The refusal names both
//! remedies (cross-build from x86_64, or `--host-glibc`).
//!
//! musl is deliberately unchanged: a host-target musl wheel still gets `pypi`.
//! The catalogue musl bundle is hosted on `x86_64-unknown-linux-gnu`, which a
//! musl host (the only place a host-target musl build happens) is not
//! guaranteed to be able to execute, so the glibc fix does not carry over
//! (soldr#3435).
//!
//! Note that maturin does **not** paper over a bad claim: with an explicit
//! `--compatibility manylinux_2_17` and an ELF needing `GLIBC_2.39`,
//! `auditwheel_rs` (maturin `src/auditwheel/linux.rs`) returns
//! `VersionedSymbolTooNewError` from the explicit-tag branch and the build
//! fails with "Error ensuring manylinux_2_17 compliance" — it downgrades only
//! when *no* tag was requested. `AuditWheelMode::Repair` is maturin's
//! `#[default]`, so `release-auto.yml`'s explicit `--auditwheel repair`
//! restates the default and its absence here changes nothing.

use crate::core::SoldrError;
use crate::fetch::gnu_linux_toolchain::GNU_LINUX_GLIBC_BASELINE;
use crate::pyo3_detect::PlanMode;

/// Arguments for `soldr wheel`.
///
/// `--target`, `--release` and `--host-glibc` must precede any passthrough
/// arguments, because everything after the first free argument is forwarded
/// to maturin verbatim.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct WheelArgs {
    /// Target triple or friendly alias (for example `linux-arm64`).
    /// Defaults to the host triple.
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,
    /// Build with the release profile. Default is a quick dev-profile wheel,
    /// matching `cargo` and `soldr build`. A release `*-linux-gnu` wheel is
    /// always built against the catalogue glibc 2.17 sysroot and tagged
    /// `manylinux_2_17`, including when the target is the host.
    #[arg(long)]
    pub release: bool,
    /// Link a host-target `*-linux-gnu` wheel against this host's glibc
    /// instead of the catalogue glibc 2.17 sysroot. The wheel then requires
    /// this host's glibc version or newer, and maturin tags it from its bytes
    /// (for example `manylinux_2_39`) rather than `manylinux_2_17`. Refused
    /// with a cross target or a non-glibc target.
    #[arg(long)]
    pub host_glibc: bool,
    /// Extra arguments forwarded verbatim to `maturin build`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

/// The host facts the wheel plan depends on, injected so the policy can be
/// tested from any machine rather than only from the host it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelHost {
    /// The host triple (`pyo3_detect::host_triple`).
    pub triple: String,
    /// Whether the catalogue GNU/Linux bundle's compilers execute here
    /// (`gnu_linux_toolchain::bundle_host_fitness`). The bundle's pinned
    /// sysroot is what enforces the 2.17 floor.
    pub gnu_bundle_runnable: bool,
    /// The running glibc's version, when it can be read cheaply. Only used in
    /// the `--host-glibc` notice.
    pub glibc_version: Option<String>,
}

impl WheelHost {
    /// The facts of the machine soldr is running on.
    pub fn current() -> Self {
        use crate::platform::host::facts;
        Self {
            triple: crate::pyo3_detect::host_triple().to_string(),
            gnu_bundle_runnable: crate::fetch::gnu_linux_toolchain::bundle_host_fitness(
                facts::os(),
                facts::arch(),
            )
            .is_runnable(),
            glibc_version: facts::glibc_version(),
        }
    }
}

/// Everything `soldr wheel` decided before re-entering the maturin path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelPlan {
    /// `maturin build ...`, handed to the ordinary dispatcher.
    pub argv: Vec<String>,
    /// The resolved Rust target triple.
    pub triple: String,
    /// soldr#3432: run target preparation even though the target is the
    /// host, because this is a release `*-linux-gnu` wheel.
    pub prepare_host_target: bool,
    /// The one `info` line printed before the build, if any.
    pub notice: Option<GlibcNotice>,
}

/// The glibc-floor `info` line `soldr wheel` prints before building, so the
/// floor a Linux wheel gets is never a silent choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlibcNotice {
    /// Built against the catalogue glibc 2.17 sysroot.
    Catalogue {
        /// `None` for a host-target build, the triple for a cross build
        /// (where `--host-glibc` has no meaning, so it is not offered).
        cross_target: Option<String>,
        /// The tag soldr asked maturin for; `None` when the caller supplied
        /// their own `--compatibility` / `--manylinux`.
        tag: Option<&'static str>,
    },
    /// `--host-glibc`: built against this host's glibc.
    HostGlibc {
        /// This host's glibc version, when known.
        version: Option<String>,
    },
}

const GREEN: &str = "\x1b[32m";
const RESET: &str = "\x1b[0m";

impl GlibcNotice {
    /// The plain-text line.
    pub fn message(&self) -> String {
        match self {
            Self::Catalogue { cross_target, tag } => {
                let tag = tag.map(|tag| format!(" ({tag})")).unwrap_or_default();
                match cross_target {
                    None => format!(
                        "soldr: info: building release wheel against glibc \
                         {GNU_LINUX_GLIBC_BASELINE}{tag} for maximum Linux compatibility; \
                         pass --host-glibc to link against this host's glibc instead"
                    ),
                    Some(triple) => format!(
                        "soldr: info: building release wheel for {triple} against glibc \
                         {GNU_LINUX_GLIBC_BASELINE}{tag} for maximum Linux compatibility \
                         (catalogue cross toolchain; --host-glibc applies only to a \
                         host-target build)"
                    ),
                }
            }
            Self::HostGlibc { version } => {
                let floor = match version {
                    Some(version) => format!("glibc {version} or newer"),
                    None => "this host's glibc version or newer (version not detected)".to_string(),
                };
                format!(
                    "soldr: info: --host-glibc: building wheel against this host's glibc, \
                     not glibc {GNU_LINUX_GLIBC_BASELINE}; it will require {floor}, and \
                     maturin tags it from its bytes instead of manylinux_2_17"
                )
            }
        }
    }

    /// The line as printed: green when `use_color`, plain otherwise.
    pub fn render(&self, use_color: bool) -> String {
        let message = self.message();
        if use_color {
            format!("{GREEN}{message}{RESET}")
        } else {
            message
        }
    }
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
    } else if is_linux_gnu(triple) {
        "manylinux_2_17"
    } else {
        "pypi"
    }
}

fn is_linux_gnu(triple: &str) -> bool {
    triple.contains("-linux-gnu")
}

fn has_flag(args: &[String], flag: &str) -> bool {
    let prefix = format!("{flag}=");
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == flag || arg.starts_with(&prefix))
}

/// Pure planner: `(args, host) -> WheelPlan`.
///
/// No I/O and no env reads — the host is injected — so this is the piece worth
/// unit-testing, and it is the only place the wheel surface decides anything.
pub fn plan_for_host(args: &WheelArgs, host: &WheelHost) -> Result<WheelPlan, SoldrError> {
    let rest = args.rest.as_slice();
    let requested = args
        .target
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&host.triple);

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
            compatibility_for_target(base, true)
        )));
    }

    let resolved = crate::target_alias::resolve_soldr_target(requested)
        .map_err(|err| err.into_soldr_error(crate::target_alias::TargetSurface::Wheel))?;
    let triple = resolved.rust_triple;
    let host_target = triple == host.triple;
    let gnu = is_linux_gnu(&triple);

    if args.host_glibc && !host_target {
        return Err(SoldrError::Other(format!(
            "soldr wheel: --host-glibc links the wheel against this host's glibc, which \
             only means something for a host-target build. `{triple}` is a cross target \
             (this host is `{}`), so the wheel is built against the catalogue glibc \
             {GNU_LINUX_GLIBC_BASELINE} sysroot for that target. Drop --host-glibc.",
            host.triple
        )));
    }
    if args.host_glibc && !gnu {
        return Err(SoldrError::Other(format!(
            "soldr wheel: --host-glibc only applies to a `*-linux-gnu` wheel; `{triple}` \
             does not link against glibc. Drop --host-glibc."
        )));
    }

    // `--debug` is maturin's spelling for "not --release". A caller who wrote
    // both is asking for two different profiles; say so rather than picking
    // one and building something they did not ask for.
    let release_in_rest = has_flag(rest, "--release");
    let debug_in_rest = has_flag(rest, "--debug");
    if args.release && debug_in_rest {
        return Err(SoldrError::Other(
            "soldr wheel: `--release` and a forwarded `--debug` ask for different profiles. \
             Drop one — `soldr wheel` alone already builds the dev profile."
                .to_string(),
        ));
    }
    let is_release = args.release || release_in_rest;

    // soldr#3432: a release linux-gnu wheel always gets the catalogue 2.17
    // sysroot, host target included, unless the caller opted out.
    let prepare_host_target = is_release && host_target && gnu && !args.host_glibc;
    if prepare_host_target && !host.gnu_bundle_runnable {
        return Err(SoldrError::Other(format!(
            "soldr wheel: a release wheel for `{triple}` is built against the catalogue \
             glibc {GNU_LINUX_GLIBC_BASELINE} sysroot so that its manylinux_2_17 tag is \
             enforced, but that toolchain cannot run on this host: every catalogue \
             GNU/Linux bundle is hosted on `{bundle_host}` (soldr#2874). soldr will not \
             silently fall back to this host's glibc for a release wheel (soldr#3432). \
             Build on an `{bundle_host}` host instead — `soldr wheel --release --target \
             {triple}` cross-builds it at glibc {GNU_LINUX_GLIBC_BASELINE} — or pass \
             --host-glibc to link against this host's glibc and have the wheel tagged \
             from its bytes.",
            bundle_host = crate::fetch::gnu_linux_toolchain::GNU_LINUX_TOOLCHAIN_HOST_TRIPLE,
        )));
    }
    // Cross builds are prepared by the maturin path's own `target != host`
    // gate; host-target ones only when this plan asks for it.
    let floor_backed = is_release && !args.host_glibc && (!host_target || prepare_host_target);

    let mut argv = vec!["maturin".to_string(), "build".to_string()];
    if is_release && !release_in_rest {
        argv.push("--release".to_string());
    }
    let caller_tagged = has_flag(rest, "--compatibility") || has_flag(rest, "--manylinux");
    let compatibility = compatibility_for_target(&triple, floor_backed);
    if !caller_tagged {
        argv.push("--compatibility".to_string());
        argv.push(compatibility.to_string());
    }
    argv.push("--target".to_string());
    argv.push(triple.clone());
    argv.extend(rest.iter().cloned());

    let notice = if args.host_glibc {
        Some(GlibcNotice::HostGlibc {
            version: host.glibc_version.clone(),
        })
    } else if floor_backed && gnu {
        Some(GlibcNotice::Catalogue {
            cross_target: (!host_target).then(|| triple.clone()),
            tag: (!caller_tagged).then_some(compatibility),
        })
    } else {
        None
    };

    Ok(WheelPlan {
        argv,
        triple,
        prepare_host_target,
        notice,
    })
}

/// The target a `soldr wheel` plan asked the maturin path to prepare although
/// it is the host (soldr#3432). `soldr wheel` re-enters the dispatcher
/// in-process, so a process-local slot carries the request without an
/// environment variable that would leak into maturin, cargo and build scripts.
static HOST_TARGET_PREP: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn request_host_target_prep(triple: &str) {
    *HOST_TARGET_PREP
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(triple.to_string());
}

/// Whether the maturin execution path must run
/// `target_lifecycle::prepare_for_invocation` for `target`: always for a
/// cross target, and for the host target when `soldr wheel` planned a release
/// `*-linux-gnu` wheel (soldr#3432).
pub(crate) fn maturin_target_needs_prep(target: &str, host: &str) -> bool {
    target != host
        || HOST_TARGET_PREP
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_deref()
            == Some(target)
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
                 abi3. The first cut of `soldr wheel` is abi3-only (soldr#2139): enable the \
                 `abi3-py310` feature on pyo3 (the fleet-wide policy, docs/API.md \"PyO3 ABI \
                 policy\"), or set SOLDR_PYO3_COMPATIBILITY=sysroot, or \
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
///
/// Side effects, both deliberate and both after every refusal has had its
/// chance: a host-target release linux-gnu plan registers its target with
/// [`maturin_target_needs_prep`], and the plan's [`GlibcNotice`] is printed to
/// stderr (green on a terminal, plain when `NO_COLOR` is set or stderr is
/// redirected).
pub(crate) fn maturin_invocation(
    args: &WheelArgs,
    no_cache: bool,
    trust_inherited_soldr_env: bool,
) -> Result<Vec<String>, SoldrError> {
    let plan = plan_for_host(args, &WheelHost::current())?;

    // The gate only has something to say about cross builds; a host build
    // resolves to `PlanMode::Native` without touching Cargo metadata anyway,
    // so skip the `cargo metadata` round trip entirely.
    if plan.triple != crate::pyo3_detect::host_triple() {
        let workspace_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let pyo3_plan = crate::pyo3_detect::resolve_for_invocation(
            &workspace_root,
            &plan.argv,
            Some(&plan.triple),
        );
        abi3_scope_check(
            pyo3_plan.mode,
            &plan.triple,
            pyo3_plan.diagnostic.as_deref(),
        )?;
    }

    if plan.prepare_host_target {
        request_host_target_prep(&plan.triple);
    }
    if let Some(notice) = &plan.notice {
        eprintln!(
            "{}",
            notice.render(crate::cargo_front_door::stderr_should_use_color())
        );
    }

    let mut argv = Vec::with_capacity(plan.argv.len() + 2);
    if no_cache {
        argv.push("--no-cache".to_string());
    }
    if trust_inherited_soldr_env {
        argv.push("--trust-inherited-soldr-env".to_string());
    }
    argv.extend(plan.argv);
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

    /// An x86_64 Linux host: the catalogue GNU bundle runs here.
    fn x86_64_linux() -> WheelHost {
        WheelHost {
            triple: "x86_64-unknown-linux-gnu".to_string(),
            gnu_bundle_runnable: true,
            glibc_version: Some("2.39".to_string()),
        }
    }

    /// A native aarch64 Linux host: every catalogue GNU bundle is
    /// x86_64-hosted (soldr#2874), so it cannot run here.
    fn aarch64_linux() -> WheelHost {
        WheelHost {
            triple: "aarch64-unknown-linux-gnu".to_string(),
            gnu_bundle_runnable: false,
            glibc_version: Some("2.39".to_string()),
        }
    }

    fn host_named(triple: &str) -> WheelHost {
        WheelHost {
            triple: triple.to_string(),
            gnu_bundle_runnable: triple == "x86_64-unknown-linux-gnu",
            glibc_version: None,
        }
    }

    fn wheel_args(
        target: Option<&str>,
        release: bool,
        host_glibc: bool,
        rest: &[&str],
    ) -> WheelArgs {
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
        let plan =
            plan_for_host(&wheel_args(None, true, false, &[]), &x86_64_linux()).expect("plan");
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
        use crate::cargo_front_door::color_enabled;
        assert!(
            color_enabled(false, true),
            "a terminal with no NO_COLOR is green"
        );
        assert!(!color_enabled(true, true), "NO_COLOR wins over a terminal");
        assert!(!color_enabled(false, false), "a redirected stderr is plain");
        assert!(!color_enabled(true, false));
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
    fn musl_host_target_release_wheel_is_unchanged() {
        // The musl bundle is x86_64-glibc-hosted; a musl host is not
        // guaranteed to run it, so soldr#3432 does not extend to musl.
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
}
