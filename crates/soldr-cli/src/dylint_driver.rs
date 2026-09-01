//! The Dylint *driver* half of what used to be one `dylint_toolchain.rs`
//! (soldr#2945).
//!
//! `dylint_toolchain.rs` answers a single question — *which nightly does this
//! workspace's Dylint run against, and who decided that?* — and hands back a
//! [`DylintToolchainPlan`]. Everything in this file starts where that answer
//! ends: given a settled plan, it locates the matching `dylint-driver` binary
//! on disk, proves the binary is the one the pinned cargo-dylint expects,
//! fetches it from the soldr-toolchain catalogue when it is absent, and builds
//! the runtime environment (`PATH` / `LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH`)
//! the driver needs to load the nightly's `rustc_private` shared libraries.
//! The two halves share only the plan type and the "is this a fully-qualified
//! nightly name?" predicate, which is what made the seam obvious once the file
//! had to be cut.
//!
//! The cut itself was forced: `.github/scripts/loc_ceiling.py` enforces a
//! **hard** 1,000-physical-line ceiling on every production source under
//! `crates/*/src/`, and unlike `loc_ratchet.py` there is no grandfathering and
//! no "it was already over" escape — a violating file fails the `Lint` job
//! whether or not the PR touched it. soldr#2945 took `dylint_toolchain.rs` from
//! 987 lines to 1,265 by threading `ChannelProvenance` through the resolver
//! and rewriting the driver-gate diagnostic, which put it 265 lines over. This
//! module is that overage, moved rather than trimmed: nothing here changed
//! behaviour on the way across.
//!
//! Three policies live here and are easy to break by accident, so they are
//! stated once, loudly:
//!
//! * **Binary-or-exit, amended by a cached local build (soldr#2432/#2484,
//!   soldr#2349).** Upstream cargo-dylint silently builds a missing or stale
//!   driver from source, uncached, every time. Soldr's original policy was to
//!   refuse outright; [`require_prebuilt_driver`] preflights the *same*
//!   version signal cargo-dylint would have checked so the refusal happens at
//!   Soldr's front door rather than several minutes into a nested build.
//!   soldr#2349 amends "refuse" to "resolve in two tiers": the catalogue
//!   prebuilt stays tier 1, unchanged; when it has no asset for this
//!   (nightly, host) pair — routine on macOS and Windows, since the catalogue
//!   does not backfill every dated nightly for every triple —
//!   [`ensure_prebuilt_driver`] now builds the driver itself as tier 2,
//!   *once*, into the same `driver_root` layout, behind a schema-versioned
//!   marker keyed on the exact compiler + cargo-dylint + host identity (see
//!   [`DylintDriverBuildMarker`]) so every later resolution with a matching
//!   marker and a still-probe-verified binary skips the build entirely. This
//!   is on by default — `SOLDR_DYLINT_DRIVER_FALLBACK=off` restores the
//!   original strict binary-or-exit behavior for hosts that must stay
//!   prebuilt-only. `wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR` is
//!   unchanged and now tier 3: the pre-existing escape that lets
//!   cargo-dylint build the driver inline, uncached, the moment it runs —
//!   reached only when tier 2 declined or failed. It is deliberately the same
//!   variable the nested-build guard in `wrapper.rs` honours, so a user who
//!   turns that policy off is not stopped a second time by a guard that
//!   disagrees about which switch turns it off.
//! * **`DYLINT_DRIVER_PATH` belongs to the caller when the caller sets it.**
//!   Every path decision in this file consults it first and never clobbers it.
//! * **A local build's trust is `unverified`, and `SOLDR_TRUST_MODE=strict`
//!   refuses it (soldr#2349).** See the doc comment on
//!   [`refuse_local_build_under_strict_trust`] for the full rationale — in
//!   short, strict's contract is "matches a pinned sha256 of the installed
//!   bytes", and a local build's output has no such pin even though its
//!   top-level input version does.
//!
//! Unit coverage for this module lives in `dylint_toolchain_tests.rs` beside
//! the resolver tests it shares its `EnvVarGuard` / sample-plan scaffolding
//! with, and reaches these items through `crate::dylint_driver::…`. The
//! soldr#2349 local-build-fallback additions have their own coverage in the
//! sibling `dylint_driver_tests.rs`, included at the bottom of this file.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use wait_timeout::ChildExt;

use crate::core::{suppress_windows_console_window, SoldrError, SoldrPaths, TargetTriple};
use crate::dylint_toolchain::{is_fully_qualified_nightly, DylintToolchainPlan};
use crate::resolve_toolchain_binary_for_channel;

/// Refuse to launch cargo-dylint unless its exact toolchain driver already
/// exists and answers the bounded version probe. Upstream cargo-dylint builds
/// this driver automatically when it is absent or stale; preflighting the same
/// version signal is what makes Soldr's binary-or-exit policy cover the driver,
/// not just the two CLI executables.
pub(crate) fn require_prebuilt_driver(
    plan: &DylintToolchainPlan,
    paths: &SoldrPaths,
) -> Result<PathBuf, SoldrError> {
    let driver_dir = driver_root(paths).join(qualified_driver_channel(plan)?);
    let driver = ["dylint-driver", "dylint-driver.exe", "dylint-driver.cmd"]
        .into_iter()
        .map(|name| driver_dir.join(name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            unavailable_driver_error(plan, &driver_dir, "no driver binary at that path")
        })?;

    let mut command = std::process::Command::new(&driver);
    command
        .arg("-V")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_driver_runtime_environment(&mut command, plan)?;
    suppress_windows_console_window(&mut command);
    // soldr#3098: spawns share, staged writes exclude.
    let spawned = {
        let _spawn = crate::core::spawn_exclusion::spawn_shared();
        command.spawn()
    };
    let mut child = spawned.map_err(|error| {
        unavailable_driver_error(
            plan,
            &driver_dir,
            &format!("version probe could not start: {error}"),
        )
    })?;
    let status = match child.wait_timeout(Duration::from_secs(2)) {
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(unavailable_driver_error(
                plan,
                &driver_dir,
                &format!("version probe failed: {error}"),
            ));
        }
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(unavailable_driver_error(
                plan,
                &driver_dir,
                "version probe exceeded the 2-second deadline",
            ));
        }
    };
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout).map_err(|error| {
            unavailable_driver_error(
                plan,
                &driver_dir,
                &format!("version probe output failed: {error}"),
            )
        })?;
    }
    if !status.success() {
        return Err(unavailable_driver_error(
            plan,
            &driver_dir,
            &format!("version probe exited with {status}"),
        ));
    }
    let expected = crate::fetch::known_tools::lookup_by_crate("cargo-dylint")
        .and_then(|spec| spec.pinned_version)
        .ok_or_else(|| SoldrError::Other("cargo-dylint must have a registry pin".into()))?;
    let actual = dylint_driver_version(&stdout);
    if actual != Some(expected) {
        return Err(unavailable_driver_error(
            plan,
            &driver_dir,
            &format!("driver version is {actual:?}, expected {expected:?}"),
        ));
    }
    Ok(driver)
}

/// Where cargo-dylint keeps its per-toolchain drivers. An explicit
/// `DYLINT_DRIVER_PATH` always wins; otherwise soldr owns the location.
fn driver_root(paths: &SoldrPaths) -> PathBuf {
    std::env::var_os("DYLINT_DRIVER_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.root.join("dylint").join("drivers"))
}

/// cargo-dylint's driver directory name: `<nightly>-<host triple>`.
fn qualified_driver_channel(plan: &DylintToolchainPlan) -> Result<String, SoldrError> {
    if is_fully_qualified_nightly(&plan.channel) {
        return Ok(plan.channel.clone());
    }
    Ok(format!(
        "{}-{}",
        plan.channel,
        TargetTriple::host()?.triple()
    ))
}

fn apply_driver_runtime_environment(
    command: &mut std::process::Command,
    plan: &DylintToolchainPlan,
) -> Result<(), SoldrError> {
    apply_driver_runtime_environment_impl(command, plan)
}

fn dylint_toolchain_dirs(plan: &DylintToolchainPlan) -> Result<(PathBuf, PathBuf), SoldrError> {
    let rustc = resolve_toolchain_binary_for_channel("rustc", Some(&plan.channel))?;
    let bin_dir = rustc
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| SoldrError::Other("Dylint rustc has no parent directory".into()))?;
    let toolchain_root = bin_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| SoldrError::Other("Dylint rustc has no toolchain root".into()))?;
    Ok((bin_dir, toolchain_root))
}

fn apply_driver_runtime_environment_impl(
    command: &mut std::process::Command,
    plan: &DylintToolchainPlan,
) -> Result<(), SoldrError> {
    match crate::platform::host::facts::os() {
        crate::platform::host::facts::HostOs::Windows => {
            let (bin_dir, _) = dylint_toolchain_dirs(plan)?;
            prepend_command_path(command, "PATH", &bin_dir)
        }
        crate::platform::host::facts::HostOs::Linux => {
            let (_, toolchain_root) = dylint_toolchain_dirs(plan)?;
            prepend_command_path(command, "LD_LIBRARY_PATH", &toolchain_root.join("lib"))?;
            Ok(())
        }
        crate::platform::host::facts::HostOs::MacOs => {
            let (_, toolchain_root) = dylint_toolchain_dirs(plan)?;
            prepend_command_path(command, "DYLD_LIBRARY_PATH", &toolchain_root.join("lib"))?;
            Ok(())
        }
    }
}

fn prepend_command_path(
    command: &mut std::process::Command,
    key: &str,
    directory: &Path,
) -> Result<(), SoldrError> {
    let existing = std::env::var_os(key);
    let paths = std::iter::once(directory.to_path_buf()).chain(
        existing
            .as_deref()
            .map(std::env::split_paths)
            .into_iter()
            .flatten(),
    );
    let joined = std::env::join_paths(paths)
        .map_err(|error| SoldrError::Other(format!("failed to construct Dylint {key}: {error}")))?;
    command.env(key, joined);
    Ok(())
}

/// Resolve a missing driver from soldr-toolchain before cargo-dylint can
/// trigger its implicit source-build fallback.
///
/// Binary-or-exit stays the default (soldr#2432/#2484). soldr#2945 adds the
/// escape hatch the nested wrapper guard already honoured — the *same*
/// variable, so a user who sets it once is not stopped twice by two guards
/// that disagree about which switch turns them off.
pub(crate) async fn ensure_prebuilt_driver(
    plan: &DylintToolchainPlan,
    paths: &SoldrPaths,
) -> Result<(), SoldrError> {
    if require_prebuilt_driver(plan, paths).is_ok() {
        return Ok(());
    }
    let version = crate::fetch::known_tools::lookup_by_crate("cargo-dylint")
        .and_then(|spec| spec.pinned_version)
        .ok_or_else(|| SoldrError::Other("cargo-dylint must have a registry pin".into()))?;
    let root = driver_root(paths);
    let fetched = crate::fetch::ensure_dylint_driver(paths, version, &plan.channel, &root).await;
    let catalogue_outcome = match fetched {
        Ok(_) => require_prebuilt_driver(plan, paths).map(|_| ()),
        // A catalogue/transport failure is just as much "no driver" as a
        // catalogue miss, and both fall through to the local-build fallback
        // below the same way they used to fall straight through to the
        // opt-in.
        Err(error) => Err(error),
    };
    let catalogue_error = match catalogue_outcome {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    // Tier 2 (soldr#2349): the catalogue has no asset for this nightly +
    // host — routine on macOS/Windows, where the catalogue does not
    // backfill every dated nightly for every triple. Build the driver once,
    // cache it behind a marker keyed on the exact inputs, and reuse it on
    // every later resolution instead of dead-ending, or (tier 3) paying an
    // uncached inline build on every single invocation.
    match ensure_local_driver_build(plan, paths, version) {
        Ok(()) => return Ok(()),
        Err(error) => {
            eprintln!(
                "soldr: local Dylint driver build fallback did not produce a driver: {error}"
            );
        }
    }

    // Tier 3: the pre-existing opt-in (soldr#2432/#2484), unchanged.
    // cargo-dylint builds the driver itself, inline, the moment it runs,
    // uncached. Reached only when tier 2 declined (fallback disabled,
    // refused under strict trust) or genuinely failed (no network, missing
    // rustc-dev, a build error).
    if crate::wrapper::allow_dylint_driver_build() {
        eprintln!(
            "{}",
            driver_source_build_warning(plan, version, &catalogue_error)
        );
        Ok(())
    } else {
        Err(catalogue_error)
    }
}

pub(crate) fn dylint_driver_version(stdout: &str) -> Option<&str> {
    stdout
        .split_whitespace()
        .last()
        .filter(|value| !value.is_empty())
}

fn host_triple_or_unknown() -> String {
    TargetTriple::host()
        .map(|target| target.triple())
        .unwrap_or_else(|_| "unknown-host".to_string())
}

/// The catalogue asset that would satisfy this plan:
/// `dylint-driver <version>-<dated nightly>` for the host triple.
fn missing_driver_asset(plan: &DylintToolchainPlan, version: &str) -> String {
    format!(
        "dylint-driver {version}-{} for {}",
        crate::dylint_libraries::canonical_channel(&plan.channel),
        host_triple_or_unknown()
    )
}

/// soldr#2945: this diagnostic used to open with "Dylint v6.0.3 is not built
/// for this machine" and close by advising the reader to "select a Dylint
/// version that provides <host> prebuilts". Both were usually false. Dylint
/// 6.0.3 publishes driver assets for all eight supported triples; what is
/// actually missing is a driver for the *nightly* that was selected, and the
/// fix depends entirely on which tier selected it. So the message leads with
/// the channel and its provenance, names the exact asset it looked for and the
/// path it looked at, and blames neither the host nor the Dylint version.
pub(crate) fn unavailable_driver_error(
    plan: &DylintToolchainPlan,
    driver_dir: &Path,
    reason: &str,
) -> SoldrError {
    let version = crate::fetch::known_tools::lookup_by_crate("cargo-dylint")
        .and_then(|spec| spec.pinned_version)
        .unwrap_or("unknown");
    SoldrError::Other(format!(
        "no usable Dylint driver for {channel} (channel selected from {provenance}). \
         Cause: {reason}. Missing asset: {asset}; expected on disk under {directory}. \
         Soldr will not build the driver from source unless asked (binary-or-exit, \
         soldr#2432/#2484). Corrective action: select a nightly the catalogue publishes a \
         Dylint v{version} driver for — when the channel came from the lint libraries that \
         means re-pinning every `rust-toolchain.toml` under \
         workspace.metadata.dylint.libraries — or point DYLINT_DRIVER_PATH at a directory \
         that already holds a v{version} driver for {channel}, or set \
         {opt_in}=1 to permit a one-time driver source build.",
        channel = plan.channel,
        provenance = plan.provenance.describe(),
        asset = missing_driver_asset(plan, version),
        directory = driver_dir.display(),
        opt_in = crate::wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR,
    ))
}

/// The loud counterpart to [`unavailable_driver_error`] for the opt-in path.
/// A driver source build is not a small thing — it installs `rustc-dev` for
/// the nightly and takes minutes — so proceeding silently would be worse than
/// the refusal it replaces.
pub(crate) fn driver_source_build_warning(
    plan: &DylintToolchainPlan,
    version: &str,
    reason: &SoldrError,
) -> String {
    format!(
        "soldr: WARNING: no prebuilt Dylint driver for {channel} (channel selected from \
         {provenance}); proceeding because {opt_in} is set. Missing asset: {asset} ({reason}). \
         cargo-dylint will now build the driver from source: that installs the rustc-dev \
         component for {channel} and takes minutes, and Soldr does not cache the result. \
         Unset {opt_in} to restore the binary-or-exit default (soldr#2432/#2484).",
        channel = plan.channel,
        provenance = plan.provenance.describe(),
        opt_in = crate::wrapper::ALLOW_DYLINT_DRIVER_BUILD_ENV_VAR,
        asset = missing_driver_asset(plan, version),
    )
}

/// Give the dylint driver cargo-dylint builds a stable soldr-owned
/// home instead of the tool's own unmanaged default (normally
/// `~/.dylint_drivers` or wherever `$DYLINT_DRIVER_PATH` happens to
/// point). A fixed path means warm runs reuse the already-built
/// driver and CI caches have something deterministic to restore.
/// Respects an explicit caller-set `DYLINT_DRIVER_PATH` — soldr never
/// clobbers a user override.
pub(crate) fn apply_dylint_driver_path(command: &mut std::process::Command) {
    if std::env::var_os("DYLINT_DRIVER_PATH").is_some() {
        return;
    }
    let Ok(paths) = crate::core::SoldrPaths::new() else {
        return;
    };
    let driver_dir = paths.root.join("dylint").join("drivers");
    if std::fs::create_dir_all(&driver_dir).is_err() {
        // Best-effort: if the directory cannot be created, fall back
        // to the tool's own default rather than pointing at a
        // nonexistent path.
        return;
    }
    command.env("DYLINT_DRIVER_PATH", driver_dir);
}

// ---------------------------------------------------------------------
// Tier 2: cached local driver build (soldr#2349).
//
// Resolution order implemented by `ensure_prebuilt_driver`:
//   1. catalogue prebuilt asset (unchanged, sha256-verified fetch + `-V`
//      identity probe);
//   2. this section — a cached local build, attempted at most once per
//      (nightly identity, cargo-dylint version, host triple);
//   3. the pre-existing `SOLDR_ALLOW_DYLINT_DRIVER_BUILD` opt-in, unchanged.
// ---------------------------------------------------------------------

/// The exact input set that produced an installed local-build driver.
///
/// Schema-versioned like `dylint_cook.rs`'s `DylintCookMarker`, for the same
/// reason: a future change to the key set (or the install layout) must be
/// able to tell an old marker from a new one instead of misreading it.
/// `schema_version` participates in equality (via `derive(PartialEq)`) so a
/// stale-schema marker on disk never satisfies a fresh key by accident.
///
/// The key set is deliberately everything `require_prebuilt_driver`'s `-V`
/// probe cannot itself observe cheaply: `channel` + `compiler_release` +
/// `compiler_commit` identify the exact nightly (mirroring
/// `DylintToolchainPlan`'s own `PartialEq`), `cargo_dylint_version` guards a
/// cargo-dylint version bump landing while `driver_root`'s per-channel
/// directory name stays the same, and `host_triple` guards a marker
/// surviving a copy (or a shared, non-host-scoped `DYLINT_DRIVER_PATH`)
/// onto a different machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DylintDriverBuildMarker {
    schema_version: u32,
    channel: String,
    compiler_release: String,
    compiler_commit: String,
    cargo_dylint_version: String,
    host_triple: String,
}

const DRIVER_BUILD_MARKER_SCHEMA_VERSION: u32 = 1;
const DRIVER_BUILD_MARKER_NAME: &str = ".soldr-dylint-driver-build-v1.json";
const DRIVER_BUILD_LOCK_NAME: &str = ".soldr-dylint-driver-build.lock";
const DRIVER_BUILD_TIMEOUT_ENV_VAR: &str = "SOLDR_DYLINT_DRIVER_BUILD_TIMEOUT_SECS";

/// Opt-out for the local-build fallback (soldr#2349). Owned, default **on**:
/// this env var absent, or set to anything but an explicit off spelling,
/// permits the fallback. An explicit off spelling (`0`/`false`/`no`/`off`,
/// [`crate::core::is_off_value`]) disables it — for a locked-down host that
/// must stay prebuilt-only (no network access to crates.io, or a policy
/// against on-host compilation of `rustc_private` code), which then falls
/// straight through to tier 3's existing refusal/opt-in.
///
/// This is a *default-on* switch, so it is read via `is_off_value` rather
/// than [`crate::core::flag`]: `flag`'s allowlist defaults an unrecognised
/// value to *off*, which would invert this switch's intended default (the
/// same distinction `env_flag`'s module docs draw between an owned
/// default-off switch and an owned default-on one).
pub(crate) const DYLINT_DRIVER_FALLBACK_ENV_VAR: &str = "SOLDR_DYLINT_DRIVER_FALLBACK";

fn local_driver_build_fallback_enabled() -> bool {
    match std::env::var(DYLINT_DRIVER_FALLBACK_ENV_VAR) {
        Ok(value) => !crate::core::is_off_value(&value),
        Err(_) => true,
    }
}

/// Whether `SOLDR_TRUST_MODE=strict` permits the local-build fallback, and
/// why it does not.
///
/// **Decision: strict refuses it.** `crates/soldr-fetch/src/fetch/trust.rs`
/// defines strict as "every installed artifact matches a pinned sha256" — a
/// promise about the *output bytes*, not the inputs that produced them. A
/// catalogue driver satisfies that today: its sha256 is computed over the
/// exact bytes soldr-toolchain published, and a pin (or the catalogue's own
/// manifest verification) can hold that hash accountable indefinitely.
///
/// A locally built driver cannot make the same promise, even though its
/// *top-level* input is pinned exactly as tightly as the catalogue's:
/// `dylint_driver = "={version}"` in [`driver_build_cargo_toml`] is the same
/// exact-match spec upstream cargo-dylint's own `driver_builder` uses. What
/// is unpinned is everything downstream of that one line — the transitive
/// dependency graph resolves at build time with no lockfile soldr records or
/// verifies, and the resulting binary's bytes vary with the host toolchain
/// build, timestamps, and embedded rpath. There is no stable hash to pin, so
/// calling it "verified" would silently redefine what strict means for this
/// one code path — from "matches a recorded hash" to "resolved from a pinned
/// top-level version". A caller who set `SOLDR_TRUST_MODE=strict`
/// specifically to require hash-verified artifacts should get exactly that
/// guarantee everywhere it applies, or a loud, named refusal on the one path
/// that cannot provide it — not a quietly weaker guarantee wearing the same
/// label.
///
/// Strict users are not stuck: `DYLINT_DRIVER_PATH` (this file's other
/// standing policy — "belongs to the caller when the caller sets it") still
/// accepts a driver placed there by whatever verified-supply-chain process
/// the strict policy trusts, and the catalogue tier is completely unaffected
/// by this decision.
fn refuse_local_build_under_strict_trust() -> Result<(), SoldrError> {
    if crate::fetch::TrustMode::from_env() == crate::fetch::TrustMode::Strict {
        return Err(SoldrError::Other(format!(
            "local Dylint driver build fallback refused under {trust_var}=strict: a local \
             build's output cannot be sha256-pinned the way a catalogue asset can (see the doc \
             comment on `refuse_local_build_under_strict_trust` in dylint_driver.rs for the \
             full rationale). Point DYLINT_DRIVER_PATH at a driver from a verified supply \
             chain, or drop {trust_var}=strict for this run.",
            trust_var = crate::fetch::TRUST_MODE_ENV_VAR,
        )));
    }
    Ok(())
}

/// Components the temporary driver-build package needs for the pinned
/// nightly. Mirrors upstream cargo-dylint's own recipe (trailofbits/dylint
/// v6.0.3, `dylint/src/driver_builder.rs`): that crate's own temporary
/// package carries a `rust-toolchain` file declaring these same two
/// components and lets rustup's proxy auto-install them lazily. Soldr
/// instead invokes the resolved toolchain's `cargo` binary directly
/// (bypassing the rustup proxy, consistent with [`dylint_toolchain_dirs`]),
/// so that auto-install never fires; ensuring the components explicitly
/// first is the direct replacement for it.
const DRIVER_BUILD_COMPONENTS: &[&str] = &["rustc-dev", "llvm-tools-preview"];

/// Package (and `[[bin]]`) name for the temporary crate soldr builds
/// `dylint_driver` inside, chosen so the compiled output has a predictable
/// path regardless of hyphen/underscore mangling.
const DRIVER_BUILD_PACKAGE_NAME: &str = "soldr-dylint-driver-build";

/// The temporary package's `Cargo.toml`. Structurally mirrors upstream
/// cargo-dylint's own `driver_builder::cargo_toml` template — same
/// dependency shape, same exact-version pin on `dylint_driver` — because the
/// binary this produces is upstream's own published driver crate, not a
/// soldr reimplementation of it.
fn driver_build_cargo_toml(cargo_dylint_version: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2018\"\npublish = false\n\n\
         [[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\n\n\
         [dependencies]\nanyhow = \"1.0\"\nenv_logger = \"0.11\"\ndylint_driver = \"={version}\"\n",
        name = DRIVER_BUILD_PACKAGE_NAME,
        version = cargo_dylint_version,
    )
}

/// Verbatim (structurally) match of upstream cargo-dylint's own driver
/// `main.rs` (trailofbits/dylint v6.0.3, `dylint/src/driver_builder.rs`):
/// the published `dylint_driver` crate's entry point is
/// `dylint_driver::dylint_driver(&args)`, so the binary this builds is
/// upstream's own driver, not a soldr reimplementation of it.
const DRIVER_BUILD_MAIN_RS: &str = r#"#![feature(rustc_private)]

use anyhow::Result;
use std::env;

pub fn main() -> Result<()> {
    env_logger::init();
    let args: Vec<_> = env::args_os().collect();
    dylint_driver::dylint_driver(&args)
}
"#;

/// Compile the pinned `dylint_driver` crate from source into a temporary
/// package, mirroring upstream cargo-dylint's own build recipe (see
/// [`DRIVER_BUILD_COMPONENTS`] and [`DRIVER_BUILD_MAIN_RS`]), then install
/// the result into `driver_dir` and return the installed path. The build
/// keeps the temporary package directory alive for its whole duration so the
/// freshly built binary can be copied out before the directory is dropped.
fn build_and_install_driver_from_source(
    plan: &DylintToolchainPlan,
    driver_dir: &Path,
    qualified_channel: &str,
    cargo_dylint_version: &str,
) -> Result<PathBuf, SoldrError> {
    for component in DRIVER_BUILD_COMPONENTS {
        let code = crate::toolchain::rustup_component_add(&plan.channel, component)?;
        if code != 0 {
            return Err(SoldrError::Other(format!(
                "local Dylint driver build: `rustup component add --toolchain {} {component}` \
                 exited with {code}",
                plan.channel,
            )));
        }
    }

    let tempdir = tempfile::tempdir()
        .map_err(|error| SoldrError::Other(format!("driver build tempdir failed: {error}")))?;
    let package = tempdir.path();
    std::fs::write(
        package.join("Cargo.toml"),
        driver_build_cargo_toml(cargo_dylint_version),
    )?;
    let src = package.join("src");
    std::fs::create_dir_all(&src)?;
    std::fs::write(src.join("main.rs"), DRIVER_BUILD_MAIN_RS)?;

    let cargo = resolve_toolchain_binary_for_channel("cargo", Some(&plan.channel))?;
    let (_, toolchain_root) = dylint_toolchain_dirs(plan)?;

    let mut command = std::process::Command::new(&cargo);
    command.arg("build").current_dir(package);
    crate::binaries::apply_resolved_toolchain_homes(&mut command, &cargo);
    crate::binaries::apply_managed_cargo_home_if_available(&mut command);
    // This build must never re-enter Soldr's own rustc wrapper: a wrapper
    // inherited from an ambient `soldr cargo` session would route this
    // build's rustc invocations back through `wrapper.rs`, and
    // `nested_dylint_driver_build` refuses exactly this crate name unless
    // the tier-3 opt-in is set (soldr#2432/#2484). This build *is* the
    // sanctioned, cached, one-time source build that opt-in exists to gate
    // — it does not need to pass through that guard a second time, so it
    // runs outside the wrapper entirely rather than setting the opt-in
    // globally, which would also loosen unrelated nested builds elsewhere
    // in the same process.
    command
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    // Unix only: bake the *local* toolchain's lib directory into the
    // driver's rpath, mirroring upstream's own `-C link-args=-Wl,-rpath,...`
    // RUSTFLAGS. Unlike the catalogue-fetched driver — built on a CI machine
    // whose rustup layout the install host does not share (soldr#2634) —
    // this binary is built and run on the same host, so the rpath is
    // correct by construction. `install_local_driver` additionally wraps it
    // in the same loader-env script the catalogue path uses, so a
    // `RUSTUP_HOME` relocated between build and run is still covered.
    // Skipped on Windows: DLL resolution there is PATH-based rather than
    // rpath-based, and MSVC's linker does not accept GNU `-Wl,` syntax.
    command.env_remove("RUSTFLAGS");
    if crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows {
        let ambient = std::env::var("RUSTFLAGS").unwrap_or_default();
        let rustflags = format!(
            "{ambient} -C link-args=-Wl,-rpath,{}",
            toolchain_root.join("lib").display()
        );
        command.env("RUSTFLAGS", rustflags);
    }
    suppress_windows_console_window(&mut command);

    let status = crate::exit_guard::run_child_command(
        &mut command,
        &format!("dylint driver build for {}", plan.channel),
        "dylint-driver-build",
        crate::core::InstallerWatchdogConfig::from_env(DRIVER_BUILD_TIMEOUT_ENV_VAR),
    )?;
    if !status.success() {
        return Err(SoldrError::Other(format!(
            "local Dylint driver build failed for {} ({status})",
            plan.channel
        )));
    }

    let built = package.join("target").join("debug").join(format!(
        "{DRIVER_BUILD_PACKAGE_NAME}{}",
        std::env::consts::EXE_SUFFIX
    ));
    if !built.is_file() {
        return Err(SoldrError::Other(format!(
            "local Dylint driver build exited 0 but no binary at {}",
            built.display()
        )));
    }
    install_local_driver(&built, driver_dir, qualified_channel)
}

/// Install a freshly built driver into `driver_dir`, using the exact same
/// extensionless-name convention and Unix loader-env wrapper the catalogue
/// path installs under (`toolchain_packaged::install_extensionless_driver`,
/// soldr#2634) — replicated here rather than called because that function is
/// private to the soldr-fetch crate. Keeping the on-disk shape identical
/// means whatever already knows how to exec a catalogue-fetched driver execs
/// a locally built one exactly the same way.
fn install_local_driver(
    built_binary: &Path,
    driver_dir: &Path,
    qualified_channel: &str,
) -> Result<PathBuf, SoldrError> {
    std::fs::create_dir_all(driver_dir)?;
    let destination = driver_dir.join("dylint-driver");
    let payload_destination =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            destination.clone()
        } else {
            driver_dir.join("dylint-driver-real")
        };
    install_driver_file_atomically(driver_dir, &payload_destination, |tmp| {
        std::fs::copy(built_binary, tmp).map(|_| ())
    })?;
    if payload_destination != destination {
        let script = local_driver_loader_wrapper_script(qualified_channel);
        install_driver_file_atomically(driver_dir, &destination, |tmp| {
            std::fs::write(tmp, &script)
        })?;
    }
    Ok(destination)
}

/// Write one staged driver file through a part-file + rename so a concurrent
/// reader never observes a torn executable. Mirrors
/// `toolchain_packaged::install_driver_file_atomically`.
fn install_driver_file_atomically(
    driver_dir: &Path,
    destination: &Path,
    materialize: impl Fn(&Path) -> std::io::Result<()>,
) -> Result<(), SoldrError> {
    let file_name = destination
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("dylint-driver");
    let temporary = driver_dir.join(format!(".{file_name}.part-{}", std::process::id()));
    materialize(&temporary)?;
    crate::platform::fs::permissions::make_executable(&temporary)?;
    if destination.is_file() {
        std::fs::remove_file(destination)?;
    }
    if let Err(error) = std::fs::rename(&temporary, destination) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// The Unix `dylint-driver` slot for a locally built driver: export the
/// toolchain's shared-library directory for the loader, then exec the real
/// binary. Byte-for-byte the same script `toolchain_packaged::
/// driver_loader_wrapper_script` writes for the catalogue path (soldr#2634)
/// — duplicated rather than shared because that function is private to the
/// soldr-fetch crate.
fn local_driver_loader_wrapper_script(qualified_channel: &str) -> String {
    format!(
        r#"#!/bin/sh
# soldr: loader-env wrapper for a locally built Dylint driver (soldr#2349,
# mirroring soldr#2634's catalogue wrapper).
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
lib="${{RUSTUP_HOME:-$HOME/.rustup}}/toolchains/{qualified_channel}/lib"
if [ -d "$lib" ]; then
  if [ "$(uname)" = "Darwin" ]; then
    DYLD_FALLBACK_LIBRARY_PATH="$lib${{DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}}"
    export DYLD_FALLBACK_LIBRARY_PATH
  else
    LD_LIBRARY_PATH="$lib${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}"
    export LD_LIBRARY_PATH
  fi
fi
exec "$dir/dylint-driver-real" "$@"
"#
    )
}

/// The marker this exact `(plan, cargo_dylint_version, host)` combination
/// must match for a cached build to be reused.
fn local_driver_build_marker(
    plan: &DylintToolchainPlan,
    cargo_dylint_version: &str,
    host_triple: &str,
) -> DylintDriverBuildMarker {
    DylintDriverBuildMarker {
        schema_version: DRIVER_BUILD_MARKER_SCHEMA_VERSION,
        channel: plan.channel.clone(),
        compiler_release: plan.compiler_release.clone(),
        compiler_commit: plan.compiler_commit.clone(),
        cargo_dylint_version: cargo_dylint_version.to_string(),
        host_triple: host_triple.to_string(),
    }
}

fn read_driver_build_marker(path: &Path) -> Option<DylintDriverBuildMarker> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_driver_build_marker(
    path: &Path,
    marker: &DylintDriverBuildMarker,
) -> Result<(), SoldrError> {
    let bytes = serde_json::to_vec(marker).map_err(|error| {
        SoldrError::Other(format!("driver build marker encoding failed: {error}"))
    })?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    replace_driver_build_marker_file(&temporary, path, |from, to| std::fs::rename(from, to))?;
    Ok(())
}

/// Mirrors `dylint_cook.rs`'s `replace_marker_file`: Windows `rename` does
/// not replace an existing destination, so fall back to remove-then-rename.
/// The per-driver-dir build lock makes this safe — interruption can only
/// omit the marker and force a rebuild, never corrupt one.
fn replace_driver_build_marker_file(
    temporary: &Path,
    path: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Err(error) = rename(temporary, path) {
        if path.exists() {
            std::fs::remove_file(path)?;
            rename(temporary, path)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

/// Advisory lock guarding a build of the driver at `driver_dir`, mirroring
/// `dylint_cook.rs`'s `lock_target` (`fs2::FileExt::lock_exclusive`, held for
/// the caller's guard lifetime). Two soldr processes racing to build the
/// same (nightly, cargo-dylint version, host) driver serialize here instead
/// of both compiling at once.
fn lock_driver_build(driver_dir: &Path) -> Result<File, SoldrError> {
    let path = driver_dir.join(DRIVER_BUILD_LOCK_NAME);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    file.lock_exclusive().map_err(|error| {
        SoldrError::Other(format!("failed to lock {}: {error}", path.display()))
    })?;
    Ok(file)
}

/// Skip condition for the local-build fallback: the on-disk marker must
/// match this exact `(plan, cargo_dylint_version, host)` key, *and* the
/// binary it names must still exist and answer the same `-V` identity probe
/// [`require_prebuilt_driver`] uses for the catalogue path. A matching
/// marker next to a missing or now-broken binary is not a skip condition —
/// it triggers a rebuild.
fn driver_build_satisfied(
    driver_dir: &Path,
    marker: &DylintDriverBuildMarker,
    plan: &DylintToolchainPlan,
    paths: &SoldrPaths,
) -> bool {
    read_driver_build_marker(&driver_dir.join(DRIVER_BUILD_MARKER_NAME)).as_ref() == Some(marker)
        && require_prebuilt_driver(plan, paths).is_ok()
}

/// Attempt tier 2 (soldr#2349): a cached local build of the driver.
///
/// Returns `Ok(())` only when a probe-verified driver is installed at the
/// path [`require_prebuilt_driver`] expects — whether by a marker-satisfied
/// skip, or by this call actually building and installing it. Every reason
/// the fallback did not happen (declined by env var, refused under strict
/// trust, or a genuine build failure) comes back as `Err` with the reason in
/// the message; the caller logs it and falls through to tier 3.
fn ensure_local_driver_build(
    plan: &DylintToolchainPlan,
    paths: &SoldrPaths,
    cargo_dylint_version: &str,
) -> Result<(), SoldrError> {
    if !local_driver_build_fallback_enabled() {
        return Err(SoldrError::Other(format!(
            "declined: {DYLINT_DRIVER_FALLBACK_ENV_VAR} is off"
        )));
    }
    refuse_local_build_under_strict_trust()?;

    let qualified_channel = qualified_driver_channel(plan)?;
    let driver_dir = driver_root(paths).join(&qualified_channel);
    let host_triple = TargetTriple::host()?.triple();
    let marker = local_driver_build_marker(plan, cargo_dylint_version, &host_triple);

    if driver_build_satisfied(&driver_dir, &marker, plan, paths) {
        return Ok(());
    }

    std::fs::create_dir_all(&driver_dir)?;
    let _lock = lock_driver_build(&driver_dir)?;

    // A racing process may have finished the build (and written the marker)
    // while this one waited for the lock — re-check before building again.
    if driver_build_satisfied(&driver_dir, &marker, plan, paths) {
        return Ok(());
    }

    eprintln!(
        "soldr: no catalogued Dylint driver for {qualified_channel}; building dylint_driver \
         v{cargo_dylint_version} locally for {} (installs rustc-dev; happens at most once per \
         nightly + cargo-dylint version + host)",
        plan.channel
    );

    build_and_install_driver_from_source(
        plan,
        &driver_dir,
        &qualified_channel,
        cargo_dylint_version,
    )?;

    // Verify with the exact same identity probe the catalogue path uses — a
    // build that exits 0 but produces a driver cargo-dylint would refuse
    // must not be recorded as satisfied.
    require_prebuilt_driver(plan, paths)?;

    write_driver_build_marker(&driver_dir.join(DRIVER_BUILD_MARKER_NAME), &marker)?;

    let payload =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            driver_dir.join("dylint-driver")
        } else {
            driver_dir.join("dylint-driver-real")
        };
    match std::fs::read(&payload) {
        Ok(bytes) => eprintln!(
            "soldr: trust: unverified dylint-driver v{cargo_dylint_version} local-build \
             {qualified_channel} sha256={sha256} (built locally from pinned sources; the \
             catalogue publishes no asset for this nightly+host; run with {trust_var}=strict \
             to refuse this fallback)",
            sha256 = crate::fetch::sha256_of(&bytes),
            trust_var = crate::fetch::TRUST_MODE_ENV_VAR,
        ),
        Err(error) => eprintln!(
            "soldr: trust: unverified dylint-driver v{cargo_dylint_version} local-build \
             {qualified_channel} (built locally from pinned sources; sha256 of the installed \
             binary could not be computed: {error})"
        ),
    }

    Ok(())
}

#[cfg(test)]
#[path = "dylint_driver_tests.rs"]
mod tests;
