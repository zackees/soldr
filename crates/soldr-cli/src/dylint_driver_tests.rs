//! Unit coverage for the soldr#2349 local-build fallback (tier 2 of
//! `ensure_prebuilt_driver`): the marker schema, its round-trip and
//! mismatch behavior, the skip condition, the two new env-var switches, and
//! the strict-trust-mode decision documented on
//! `refuse_local_build_under_strict_trust`.
//!
//! Deliberately does not exercise `build_and_install_driver_from_source` or
//! `ensure_local_driver_build`'s actual build path — that needs a real
//! pinned nightly toolchain, network access to crates.io, and minutes of
//! compile time, none of which belong in a unit test. What is tested here
//! is everything that does not require a toolchain: the pure marker key,
//! the on-disk marker contract, the lock's exclusivity, the two
//! env-var-driven decisions (fallback on/off, strict trust refusal), and
//! the probe verdict that decides whether tier 2 is even reachable.
//!
//! Env-var mutations use the crate's shared [`crate::TEST_PROCESS_ENV_LOCK`]
//! barrier via [`crate::EnvVarGuard`] rather than a private mutex:
//! `SOLDR_TRUST_MODE` and `SOLDR_ALLOW_DYLINT_DRIVER_BUILD` are read by
//! production code elsewhere in the crate, so a private barrier here would
//! not actually exclude those readers (`env_lock_lint.rs`'s whole point).

use super::*;
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;
use fs2::FileExt;

fn sample_plan() -> DylintToolchainPlan {
    DylintToolchainPlan::identity(
        "nightly-2026-05-28-x86_64-unknown-linux-gnu".to_string(),
        "1.96.0-nightly".to_string(),
        "0123456789abcdef".to_string(),
    )
}

fn sample_marker() -> DylintDriverBuildMarker {
    local_driver_build_marker(&sample_plan(), "6.0.3", "x86_64-unknown-linux-gnu")
}

// ---------------------------------------------------------------------
// Marker round-trip + mismatch.
// ---------------------------------------------------------------------

#[test]
fn marker_round_trips_through_disk() {
    let temp = tempfile::tempdir().unwrap();
    let marker_path = temp.path().join(DRIVER_BUILD_MARKER_NAME);
    let marker = sample_marker();

    assert!(read_driver_build_marker(&marker_path).is_none());

    write_driver_build_marker(&marker_path, &marker).expect("write marker");
    let restored = read_driver_build_marker(&marker_path).expect("marker must be readable back");
    assert_eq!(restored, marker);
}

#[test]
fn marker_write_is_atomic_replace_not_append() {
    // Writing twice must leave exactly the second marker on disk, proving
    // the temp-file + rename replaces rather than corrupts/append the
    // existing file (soldr#2349, mirroring dylint_cook.rs's
    // `replace_marker_file` contract).
    let temp = tempfile::tempdir().unwrap();
    let marker_path = temp.path().join(DRIVER_BUILD_MARKER_NAME);

    let first = local_driver_build_marker(&sample_plan(), "6.0.3", "x86_64-unknown-linux-gnu");
    write_driver_build_marker(&marker_path, &first).expect("first write");

    let mut second_plan = sample_plan();
    second_plan.compiler_commit = "fedcba9876543210".to_string();
    let second = local_driver_build_marker(&second_plan, "6.0.3", "x86_64-unknown-linux-gnu");
    write_driver_build_marker(&marker_path, &second).expect("second write");

    let restored = read_driver_build_marker(&marker_path).expect("marker must be readable");
    assert_eq!(restored, second);
    assert_ne!(restored, first);
}

#[test]
fn marker_mismatches_on_nightly_change() {
    let base = local_driver_build_marker(&sample_plan(), "6.0.3", "x86_64-unknown-linux-gnu");

    let mut other_plan = sample_plan();
    other_plan.channel = "nightly-2026-06-11-x86_64-unknown-linux-gnu".to_string();
    let other = local_driver_build_marker(&other_plan, "6.0.3", "x86_64-unknown-linux-gnu");

    assert_ne!(
        base, other,
        "a different nightly channel must not satisfy the same marker"
    );
}

#[test]
fn marker_mismatches_on_cargo_dylint_version_change() {
    let plan = sample_plan();
    let base = local_driver_build_marker(&plan, "6.0.3", "x86_64-unknown-linux-gnu");
    let other = local_driver_build_marker(&plan, "6.0.4", "x86_64-unknown-linux-gnu");

    assert_ne!(
        base, other,
        "a cargo-dylint version bump must not satisfy a marker built for the old version"
    );
}

#[test]
fn marker_mismatches_on_host_triple_change() {
    let plan = sample_plan();
    let base = local_driver_build_marker(&plan, "6.0.3", "x86_64-unknown-linux-gnu");
    let other = local_driver_build_marker(&plan, "6.0.3", "aarch64-apple-darwin");

    assert_ne!(
        base, other,
        "a marker built for one host triple must not satisfy a different host"
    );
}

#[test]
fn marker_mismatches_on_schema_version_change() {
    // Not one of the four axes the task names explicitly, but the schema
    // field exists precisely to make this case fail closed too: a marker
    // written by an older/newer schema must never be misread as current.
    let mut old = sample_marker();
    old.schema_version = DRIVER_BUILD_MARKER_SCHEMA_VERSION.wrapping_add(1);
    let current = sample_marker();
    assert_ne!(old.schema_version, current.schema_version);
    assert_ne!(old, current);
}

// ---------------------------------------------------------------------
// Skip requires the binary to exist.
// ---------------------------------------------------------------------

/// `driver_build_satisfied` takes an explicit `driver_dir`, but its second
/// half (`require_prebuilt_driver`) independently re-derives its own driver
/// directory from `(paths, plan)`. In production the two always agree
/// because `ensure_local_driver_build` computes `driver_dir` with this exact
/// same formula; tests must do the same or the probe silently looks in an
/// empty, unrelated directory and every assertion below would pass for the
/// wrong reason. `sample_plan()`'s channel is already fully qualified, so
/// this needs no host-triple lookup and is deterministic on any machine.
fn sample_driver_dir(paths: &SoldrPaths, plan: &DylintToolchainPlan) -> PathBuf {
    driver_root(paths).join(qualified_driver_channel(plan).expect("qualified channel"))
}

#[test]
fn skip_is_refused_when_marker_matches_but_binary_is_absent() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _unset = crate::EnvVarGuard::remove("DYLINT_DRIVER_PATH");
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("soldr"));
    let plan = sample_plan();
    let driver_dir = sample_driver_dir(&paths, &plan);
    std::fs::create_dir_all(&driver_dir).unwrap();

    let marker = local_driver_build_marker(&plan, "6.0.3", "x86_64-unknown-linux-gnu");
    write_driver_build_marker(&driver_dir.join(DRIVER_BUILD_MARKER_NAME), &marker)
        .expect("write marker");

    // No `dylint-driver` file was ever created in `driver_dir`: a matching
    // marker alone must never be treated as a skip condition.
    assert!(
        !driver_build_satisfied(&driver_dir, &marker, &plan, &paths),
        "a matching marker next to a missing binary must not skip the build"
    );
}

#[test]
fn skip_is_refused_when_binary_exists_but_fails_the_probe() {
    // A present-but-non-functional file (e.g. left over from an
    // interrupted install, or simply not executable) must not satisfy the
    // skip either -- `require_prebuilt_driver`'s `-V` probe is what
    // ultimately gates it, not file presence alone.
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _unset = crate::EnvVarGuard::remove("DYLINT_DRIVER_PATH");
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("soldr"));
    let plan = sample_plan();
    let driver_dir = sample_driver_dir(&paths, &plan);
    std::fs::create_dir_all(&driver_dir).unwrap();
    std::fs::write(driver_dir.join("dylint-driver"), b"not an executable").unwrap();

    let marker = local_driver_build_marker(&plan, "6.0.3", "x86_64-unknown-linux-gnu");
    write_driver_build_marker(&driver_dir.join(DRIVER_BUILD_MARKER_NAME), &marker)
        .expect("write marker");

    assert!(
        !driver_build_satisfied(&driver_dir, &marker, &plan, &paths),
        "a present but non-executable file must not satisfy the skip condition"
    );
}

#[test]
fn skip_is_refused_when_marker_is_absent_even_if_something_is_on_disk() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _unset = crate::EnvVarGuard::remove("DYLINT_DRIVER_PATH");
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("soldr"));
    let plan = sample_plan();
    let driver_dir = sample_driver_dir(&paths, &plan);
    std::fs::create_dir_all(&driver_dir).unwrap();
    std::fs::write(driver_dir.join("dylint-driver"), b"not an executable").unwrap();

    let marker = local_driver_build_marker(&plan, "6.0.3", "x86_64-unknown-linux-gnu");

    // No marker was ever written.
    assert!(!driver_build_satisfied(&driver_dir, &marker, &plan, &paths));
}

// ---------------------------------------------------------------------
// SOLDR_DYLINT_DRIVER_FALLBACK: default-on opt-out.
// ---------------------------------------------------------------------

#[test]
fn fallback_defaults_on_when_unset() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _unset = crate::EnvVarGuard::remove(DYLINT_DRIVER_FALLBACK_ENV_VAR);
    assert!(local_driver_build_fallback_enabled());
}

#[test]
fn fallback_off_spellings_disable_it() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for value in ["0", "false", "no", "off", "OFF"] {
        let _set = crate::EnvVarGuard::set(DYLINT_DRIVER_FALLBACK_ENV_VAR, value);
        assert!(
            !local_driver_build_fallback_enabled(),
            "{value:?} must disable the fallback"
        );
    }
}

#[test]
fn fallback_unrecognised_values_stay_on() {
    // Default-on switches keep an unrecognised value on -- only an
    // explicit off spelling turns them off (env_flag's `is_off_value`
    // contract; see the doc comment on DYLINT_DRIVER_FALLBACK_ENV_VAR).
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _set = crate::EnvVarGuard::set(DYLINT_DRIVER_FALLBACK_ENV_VAR, "maybe");
    assert!(local_driver_build_fallback_enabled());
}

#[test]
fn fallback_on_spellings_stay_on() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for value in ["1", "true", "yes", "on"] {
        let _set = crate::EnvVarGuard::set(DYLINT_DRIVER_FALLBACK_ENV_VAR, value);
        assert!(
            local_driver_build_fallback_enabled(),
            "{value:?} must stay on"
        );
    }
}

// ---------------------------------------------------------------------
// The strict-trust-mode decision.
// ---------------------------------------------------------------------

#[test]
fn strict_trust_mode_refuses_the_local_build_fallback() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _set = crate::EnvVarGuard::set(crate::fetch::TRUST_MODE_ENV_VAR, "strict");

    let error = refuse_local_build_under_strict_trust()
        .expect_err("strict trust mode must refuse the local-build fallback");
    let message = error.to_string();
    assert!(message.contains("strict"), "unexpected error: {message}");
    assert!(
        message.contains("DYLINT_DRIVER_PATH"),
        "the refusal must point at the escape hatch: {message}"
    );
}

#[test]
fn permissive_trust_mode_permits_the_local_build_fallback() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _set = crate::EnvVarGuard::set(crate::fetch::TRUST_MODE_ENV_VAR, "permissive");
    refuse_local_build_under_strict_trust().expect("permissive mode must permit the fallback");
}

#[test]
fn absent_trust_mode_defaults_to_permitting_the_local_build_fallback() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _unset = crate::EnvVarGuard::remove(crate::fetch::TRUST_MODE_ENV_VAR);
    refuse_local_build_under_strict_trust().expect("default trust mode must permit the fallback");
}

// ---------------------------------------------------------------------
// The advisory lock is actually exclusive.
// ---------------------------------------------------------------------

#[test]
fn driver_build_lock_is_exclusive_within_the_process() {
    let temp = tempfile::tempdir().unwrap();
    let driver_dir = temp.path().join("driver-dir");
    std::fs::create_dir_all(&driver_dir).unwrap();

    let _held = lock_driver_build(&driver_dir).expect("first lock must succeed");

    // A second, independent open + lock attempt on the same lock file must
    // contend rather than silently succeed -- otherwise two racing
    // processes could build the driver concurrently into the same
    // directory.
    let contender_path = driver_dir.join(DRIVER_BUILD_LOCK_NAME);
    let contender = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&contender_path)
        .expect("open contender handle");
    assert!(
        contender.try_lock_exclusive().is_err(),
        "a second handle must not be able to take the lock while the first holds it"
    );

    drop(_held);

    // Once released, a fresh attempt succeeds.
    contender
        .try_lock_exclusive()
        .expect("the lock must be acquirable once released");
}

// ---------------------------------------------------------------------
// Build-recipe fidelity to upstream (pure string checks only -- no build).
// ---------------------------------------------------------------------

#[test]
fn cargo_toml_pins_the_exact_cargo_dylint_version() {
    let manifest = driver_build_cargo_toml("6.0.3");
    assert!(
        manifest.contains("dylint_driver = \"=6.0.3\""),
        "must pin dylint_driver to the exact cargo-dylint version: {manifest}"
    );
    assert!(manifest.contains(DRIVER_BUILD_PACKAGE_NAME));
}

#[test]
fn main_rs_calls_the_upstream_driver_entry_point() {
    assert!(DRIVER_BUILD_MAIN_RS.contains("dylint_driver::dylint_driver(&args)"));
    assert!(DRIVER_BUILD_MAIN_RS.contains("#![feature(rustc_private)]"));
}

#[test]
fn required_components_match_upstream_driver_builder() {
    assert_eq!(
        DRIVER_BUILD_COMPONENTS,
        &["rustc-dev", "llvm-tools-preview"]
    );
}

#[test]
fn loader_wrapper_script_execs_the_real_binary_for_the_named_channel() {
    let script = local_driver_loader_wrapper_script("nightly-2026-05-28-x86_64-unknown-linux-gnu");
    assert!(script.starts_with("#!/bin/sh"));
    assert!(script.contains("nightly-2026-05-28-x86_64-unknown-linux-gnu/lib"));
    assert!(script.contains("exec \"$dir/dylint-driver-real\" \"$@\""));
}

// ---------------------------------------------------------------------
// soldr#2436 vs. soldr#2349: the probe verdict decides whether tier 2
// may run at all.
//
// A missing (or wrong-identity) driver is what the fetch and local-build
// tiers exist to cure. A driver that blew the bounded `-V` deadline is
// not: the binary is there and was executing, and building another one
// cannot make an unresponsive host answer -- it only turns soldr#2436's
// deliberately bounded failure into a `rustup component add` plus a full
// `rustc_private` compile before reporting the same verdict.
// ---------------------------------------------------------------------

#[test]
fn missing_driver_verdict_permits_the_local_build_fallback() {
    assert!(local_build_fallback_applies(
        DriverProbeVerdict::NoUsableDriver
    ));
}

#[test]
fn timed_out_probe_verdict_refuses_the_local_build_fallback() {
    assert!(!local_build_fallback_applies(
        DriverProbeVerdict::ProbeTimedOut
    ));
}

#[test]
fn absent_driver_probes_as_no_usable_driver() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _unset = crate::EnvVarGuard::remove("DYLINT_DRIVER_PATH");
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("soldr"));
    let plan = sample_plan();

    let (verdict, _error) =
        probe_prebuilt_driver(&plan, &paths).expect_err("no driver exists on disk");
    assert_eq!(verdict, DriverProbeVerdict::NoUsableDriver);
    assert!(
        local_build_fallback_applies(verdict),
        "a genuinely missing driver must still reach the local-build fallback"
    );
}

#[test]
fn hanging_driver_probes_as_timed_out_and_stays_bounded() {
    // The fixture is a `sleep` shell script, so this is Unix-only; the
    // Windows probe path is covered by the shared verdict tests above.
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _unset = crate::EnvVarGuard::remove("DYLINT_DRIVER_PATH");
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("soldr"));
    let plan = sample_plan();

    // `apply_driver_runtime_environment` resolves the plan's rustc to
    // derive the driver's loader path; point it at a stand-in so the probe
    // itself -- not toolchain resolution -- is what this test measures.
    let fake_rustc = temp.path().join("toolchain").join("bin").join("rustc");
    std::fs::create_dir_all(fake_rustc.parent().unwrap()).unwrap();
    std::fs::write(&fake_rustc, b"").unwrap();
    let _rustc = crate::EnvVarGuard::set(crate::TEST_RUSTC_BIN_ENV_VAR, &fake_rustc);

    let driver_dir = sample_driver_dir(&paths, &plan);
    std::fs::create_dir_all(&driver_dir).unwrap();
    let driver = driver_dir.join("dylint-driver");
    std::fs::write(&driver, b"#!/bin/sh\nsleep 60\n").unwrap();
    crate::platform::fs::permissions::make_executable(&driver).expect("chmod hung driver");

    let started = std::time::Instant::now();
    let (verdict, error) =
        probe_prebuilt_driver(&plan, &paths).expect_err("a hung driver must not probe clean");
    let elapsed = started.elapsed();

    assert_eq!(verdict, DriverProbeVerdict::ProbeTimedOut);
    assert!(
        !local_build_fallback_applies(verdict),
        "a hung probe must not escalate to a local driver build"
    );
    assert!(
        error
            .to_string()
            .contains("version probe exceeded the 2-second deadline"),
        "unexpected probe error: {error}"
    );
    // The probe deadline is 2s; 30s leaves a wide scheduler margin while
    // still proving the classification did not wait out the 60s sleep.
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "probe classification must stay bounded, took {elapsed:?}"
    );
}
