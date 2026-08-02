//! Precedence contract for the flags `soldr prepare --github-env` exports.
//!
//! Lives in its own module rather than in `prepare_cmd`'s test block because
//! that file is already over the LOC ratchet's ceiling and may not grow.
//!
//! ## What is being pinned
//!
//! `apply_blessed_prep_env` exports `CARGO_ENCODED_RUSTFLAGS`, which outranks
//! both `RUSTFLAGS` and `CARGO_TARGET_<triple>_RUSTFLAGS` in Cargo's
//! precedence order. Whatever it writes there is therefore the *only* thing
//! that takes effect, so anything the caller had already put in the
//! lower-precedence variables has to be folded in rather than shadowed.
//!
//! `apply_to_process` has covered the in-process half of this since
//! `applying_target_flags_consumes_higher_precedence_globals`
//! (`target_lifecycle`). The `--github-env` half — the one CI actually runs —
//! had no equivalent.
//!
//! zackees/clud#732 is why this is worth a test: a bump that moved the MSVC
//! link configuration into the encoded variable cost that consumer a CI cycle,
//! because the precedence rule was not written down and its guard assumed the
//! target-scoped key still won.

use crate::blessed_build::BlessedPrep;
use crate::prepare_cmd::apply_blessed_prep_env;
use crate::{EnvVarGuard, TEST_PROCESS_ENV_LOCK};

crate::timed_test!(exported_encoded_rustflags_keep_caller_target_flags, {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let target_key = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS";
    let _target = EnvVarGuard::set(target_key, "-C link-arg=advapi32.lib");
    let _global = EnvVarGuard::set("RUSTFLAGS", "-Dwarnings");
    let _encoded = EnvVarGuard::remove("CARGO_ENCODED_RUSTFLAGS");

    let mut prep = BlessedPrep::default();
    prep.env.push((
        target_key.to_string(),
        "-C link-arg=/LIBPATH:/soldr/sdk".to_string(),
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let github_env = dir.path().join("github.env");
    apply_blessed_prep_env(Some(&github_env), &prep).expect("apply prep env");

    let exported = std::fs::read_to_string(&github_env).expect("read github env");
    let encoded_line = exported
        .lines()
        .find_map(|line| line.strip_prefix("CARGO_ENCODED_RUSTFLAGS="))
        .expect("CARGO_ENCODED_RUSTFLAGS was not exported");
    let tokens: Vec<&str> = encoded_line.split('\u{1f}').collect();

    // soldr's own required SDK flags.
    assert!(
        tokens.contains(&"link-arg=/LIBPATH:/soldr/sdk"),
        "required SDK flag missing from {tokens:?}"
    );
    // The caller's target-scoped flag, which the encoded variable would
    // otherwise shadow into oblivion.
    assert!(
        tokens.contains(&"link-arg=advapi32.lib"),
        "caller's target-scoped flag was dropped from {tokens:?}"
    );
    // And the lower-precedence global.
    assert!(
        tokens.contains(&"-Dwarnings"),
        "caller's global RUSTFLAGS was dropped from {tokens:?}"
    );
});
