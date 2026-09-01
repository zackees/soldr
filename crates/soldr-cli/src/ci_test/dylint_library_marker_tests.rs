//! Unit tests for [`super`].

use super::*;

fn toolchain(channel: &str, release: &str, commit: &str) -> DylintToolchainPlan {
    DylintToolchainPlan::identity(channel.into(), release.into(), commit.into())
}

fn write_lint_fixture(root: &Path, lint: &str) {
    let lint_dir = root.join("dylints").join(lint);
    std::fs::create_dir_all(lint_dir.join("src")).unwrap();
    std::fs::create_dir_all(lint_dir.join(".cargo")).unwrap();
    std::fs::create_dir_all(lint_dir.join("ui")).unwrap();
    std::fs::write(lint_dir.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
    std::fs::write(
        lint_dir.join("Cargo.toml"),
        "[package]\nname='demo'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(lint_dir.join("Cargo.lock"), "version = 3\n").unwrap();
    std::fs::write(
        lint_dir.join("rust-toolchain.toml"),
        "[toolchain]\nchannel='nightly-2026-05-28'\n",
    )
    .unwrap();
    std::fs::write(
        lint_dir.join(".cargo/config.toml"),
        "[build]\nrustflags=['-C', 'linker=dylint-link']\n",
    )
    .unwrap();
    std::fs::write(lint_dir.join("ui/fixture.rs"), "fn main() {}\n").unwrap();
}

fn library_inputs(root: &Path, target_directory: &Path) -> LibraryMarkerInputs {
    LibraryMarkerInputs {
        root: root.to_path_buf(),
        target_directory: target_directory.to_path_buf(),
        toolchain: toolchain("nightly-2026-05-28", "1.99.0-nightly", "0123456789abcdef"),
        driver_identity: None,
    }
}

/// The stage skip is OPT-IN (soldr#3038): every test below that asserts on
/// marker *equality* opts in explicitly, so those assertions keep testing the
/// key, not the default. `default_is_off_even_on_an_exact_marker_hit` is the
/// one test that goes through [`evaluate`] to pin the default itself.
fn evaluate_opted_in(inputs: LibraryMarkerInputs) -> LibraryMarkerDecision {
    evaluate_with_skip_enabled(inputs, true).unwrap()
}

fn populate_release_payload(target_directory: &Path) {
    std::fs::create_dir_all(target_directory.join("release")).unwrap();
    std::fs::write(
        target_directory.join("release/libdylint_lint_a.so"),
        b"cdylib",
    )
    .unwrap();
}

#[test]
fn marker_round_trips_through_record_and_evaluate() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let target = temp.path().join("target-tree");
    write_lint_fixture(&root, "lint-a");
    populate_release_payload(&target);

    let first = evaluate_opted_in(library_inputs(&root, &target));
    assert!(!first.skip, "no marker has been written yet");
    record(&first).unwrap();

    let second = evaluate_opted_in(library_inputs(&root, &target));
    assert!(
        second.skip,
        "a marker just recorded for these exact inputs must read as a hit"
    );
}

#[test]
fn hash_changes_when_a_lint_src_file_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_lint_fixture(root, "lint-a");
    let first = semantic_input_hash(root).unwrap();
    std::fs::write(root.join("dylints/lint-a/src/lib.rs"), "pub fn two() {}\n").unwrap();
    assert_ne!(first, semantic_input_hash(root).unwrap());
}

#[test]
fn hash_changes_when_a_lint_cargo_lock_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_lint_fixture(root, "lint-a");
    let first = semantic_input_hash(root).unwrap();
    std::fs::write(root.join("dylints/lint-a/Cargo.lock"), "version = 4\n").unwrap();
    assert_ne!(first, semantic_input_hash(root).unwrap());
}

#[test]
fn hash_changes_when_cargo_config_toml_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_lint_fixture(root, "lint-a");
    let first = semantic_input_hash(root).unwrap();
    std::fs::write(
        root.join("dylints/lint-a/.cargo/config.toml"),
        "[build]\nrustflags=['-C', 'linker=dylint-link', '-C', 'opt-level=1']\n",
    )
    .unwrap();
    assert_ne!(first, semantic_input_hash(root).unwrap());
}

#[test]
fn hash_ignores_ui_fixture_changes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_lint_fixture(root, "lint-a");
    let first = semantic_input_hash(root).unwrap();
    std::fs::write(
        root.join("dylints/lint-a/ui/fixture.rs"),
        "fn changed() {}\n",
    )
    .unwrap();
    assert_eq!(
        first,
        semantic_input_hash(root).unwrap(),
        "dylints/*/ui/** fixtures do not affect the built cdylib"
    );
}

#[test]
fn no_skip_without_target_tree_payload() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let target = temp.path().join("target-tree");
    write_lint_fixture(&root, "lint-a");
    std::fs::create_dir_all(&target).unwrap();

    let first = evaluate_opted_in(library_inputs(&root, &target));
    record(&first).unwrap();

    let second = evaluate_opted_in(library_inputs(&root, &target));
    assert!(
        !second.skip,
        "a matching marker with no built library payload must still rebuild"
    );
}

#[test]
fn environment_hash_changes_when_rustflags_present() {
    // soldr#2349 finding 1: RUSTFLAGS (and friends) must be part of the
    // marker's cache key, or a rebuild triggered by a flag change reads as
    // a stale-clean hit. Threaded through as a parameter rather than
    // mutating `std::env` so this test stays single-threaded-safe.
    let baseline = environment_input_hash(std::iter::empty()).unwrap();
    let with_rustflags = environment_input_hash(std::iter::once((
        "RUSTFLAGS".to_string(),
        "-C opt-level=1".to_string(),
    )))
    .unwrap();
    assert_ne!(
        baseline, with_rustflags,
        "a changed RUSTFLAGS must change the marker's environment hash"
    );
}

#[test]
fn environment_hash_ignores_irrelevant_keys() {
    let baseline = environment_input_hash(std::iter::empty()).unwrap();
    let with_path = environment_input_hash(std::iter::once((
        "PATH".to_string(),
        "/usr/bin".to_string(),
    )))
    .unwrap();
    assert_eq!(
        baseline, with_path,
        "keys outside the RUSTFLAGS/CARGO_PROFILE_*/CARGO_TARGET_* predicate must not perturb the hash"
    );
}

#[test]
fn driver_identity_ignores_mtime_but_tracks_content() {
    // soldr#2349 finding 2: the CI workflow restores the driver from an
    // actions/cache tar (or re-fetches it), changing mtime while the bytes
    // stay identical. The identity must survive that and only change when
    // the bytes actually do.
    let temp = tempfile::tempdir().unwrap();
    let driver_path = temp.path().join("dylint-driver");
    std::fs::write(&driver_path, b"same-length-a").unwrap();
    let first = hash_driver_file(&driver_path).unwrap();

    filetime::set_file_mtime(
        &driver_path,
        filetime::FileTime::from_unix_time(4_102_444_800, 0),
    )
    .unwrap();
    let mtime_changed = hash_driver_file(&driver_path).unwrap();
    assert_eq!(
        first, mtime_changed,
        "driver identity must not depend on mtime"
    );

    std::fs::write(&driver_path, b"same-length-b").unwrap();
    let content_changed = hash_driver_file(&driver_path).unwrap();
    assert_ne!(
        first, content_changed,
        "a same-length content change must still change the identity"
    );
}

#[test]
fn no_skip_when_compiler_identity_differs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let target = temp.path().join("target-tree");
    write_lint_fixture(&root, "lint-a");
    populate_release_payload(&target);

    let first = evaluate_opted_in(library_inputs(&root, &target));
    record(&first).unwrap();

    let mut changed = library_inputs(&root, &target);
    changed.toolchain = toolchain("nightly-2026-05-28", "1.99.0-nightly", "fedcba9876543210");
    let second = evaluate_opted_in(changed);
    assert!(
        !second.skip,
        "a different compiler commit must not read as a hit"
    );
}

#[test]
fn default_is_off_even_on_an_exact_marker_hit() {
    // soldr#3038: the skip ships OPT-IN. A run whose inputs match the
    // recorded marker exactly, with a real payload in the tree -- the case
    // that would otherwise skip -- must still rebuild when
    // SOLDR_DYLINT_LIBRARY_SKIP is unset. Expressed through the pure
    // `skip_enabled` + `evaluate_with_skip_enabled` pair rather than
    // `std::env::set_var`, which is `unsafe` and would race the other tests
    // in this binary.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let target = temp.path().join("target-tree");
    write_lint_fixture(&root, "lint-a");
    populate_release_payload(&target);

    let first = evaluate_opted_in(library_inputs(&root, &target));
    record(&first).unwrap();

    let opted_in = evaluate_opted_in(library_inputs(&root, &target));
    assert!(opted_in.skip, "opted in, this is an exact marker hit");

    let default_off =
        evaluate_with_skip_enabled(library_inputs(&root, &target), skip_enabled(None)).unwrap();
    assert!(
        !default_off.skip,
        "an unset SOLDR_DYLINT_LIBRARY_SKIP must not skip the six library stages"
    );
}

#[test]
fn only_an_explicit_on_spelling_enables_the_skip() {
    // The switch is soldr-owned and defaults off, so an unrecognised value
    // must read as off (soldr#2740): `=maybe` enabling a stage skip is the
    // failure mode that rule exists to prevent.
    for value in ["1", "true", "TRUE", "yes", "on", " on "] {
        assert!(skip_enabled(Some(value)), "{value:?} must enable the skip");
    }
    for value in ["", "0", "false", "no", "off", "maybe", "enabled", "2"] {
        assert!(
            !skip_enabled(Some(value)),
            "{value:?} must NOT enable the skip"
        );
    }
    assert!(!skip_enabled(None), "absent must not enable the skip");
}
