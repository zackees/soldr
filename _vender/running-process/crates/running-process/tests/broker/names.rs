//! Phase 1 of #228 (issue #230) — coverage for
//! `crate::broker::lifecycle::names`.
//!
//! Tests are deliberately platform-aware: every assertion that depends
//! on the Windows or Unix half of [`PipePath`] is gated with `cfg`.

#![cfg(feature = "client")]

#[cfg(target_os = "macos")]
use running_process::broker::lifecycle::names::MACOS_SUN_PATH_MAX;
#[cfg(windows)]
use running_process::broker::lifecycle::names::WINDOWS_MAX_PATH;
use running_process::broker::lifecycle::names::{
    backend_pipe, explicit_instance_pipe, private_broker_pipe, shared_broker_pipe,
    validate_service_name, validate_version, PipePath, PipePathError,
};
use running_process::broker::lifecycle::sid::{hash_to_16_hex, user_sid_hash};

const ALICE: &str = "deadbeefdeadbeef";
const BOB: &str = "feedfacefeedface";

fn pick_one(p: &PipePath) -> String {
    match (&p.windows, &p.unix) {
        (Some(w), None) => w.clone(),
        (None, Some(u)) => u.to_string_lossy().into_owned(),
        _ => panic!("exactly one of windows/unix must be Some"),
    }
}

#[test]
fn same_user_produces_stable_hash() {
    let a = hash_to_16_hex(b"alice:machine-1");
    let b = hash_to_16_hex(b"alice:machine-1");
    assert_eq!(a, b);
    assert_eq!(a.len(), 16);
}

#[test]
fn different_users_produce_different_hashes() {
    let a = hash_to_16_hex(b"alice:machine-1");
    let b = hash_to_16_hex(b"bob:machine-1");
    assert_ne!(a, b);
}

#[test]
fn user_sid_hash_runs_or_explains_why_not() {
    // We do not require this to succeed on every CI box (e.g. minimal
    // containers may lack /etc/machine-id). When it does succeed,
    // assert the 16-char invariant. When it doesn't, the test still
    // passes — the failure mode is informational only.
    match user_sid_hash() {
        Ok(h) => assert_eq!(h.len(), 16),
        Err(e) => eprintln!("user_sid_hash unavailable on this host: {e}"),
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn pipe_names_share_v1_prefix() {
    // macOS folds the canonical name into a 16-char hash to fit
    // sun_path (104 bytes), so neither `rpb-v1-` nor the SID hash
    // appears literally on that platform. See `macos_pipe_paths_*`
    // tests below for the macOS-specific invariants.
    let shared = pick_one(&shared_broker_pipe(ALICE).unwrap());
    let private = pick_one(&private_broker_pipe(ALICE, "zccache").unwrap());
    let instance = pick_one(&explicit_instance_pipe(ALICE, "dev-build").unwrap());
    let backend = pick_one(&backend_pipe(ALICE, &[0u8; 16]).unwrap());
    for s in [&shared, &private, &instance, &backend] {
        assert!(
            s.contains("rpb-v1-"),
            "expected `rpb-v1-` prefix in pipe path {s:?}"
        );
        assert!(
            s.contains(ALICE),
            "expected SID hash {ALICE} embedded in pipe path {s:?}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn macos_pipe_paths_are_hashed_leaves() {
    // On macOS the leaf is always `{16char-hash}.sock` so the canonical
    // `rpb-v1-` prefix and the raw SID hash don't appear in the path
    // string. We still want to assert: (a) it ends with `.sock`, (b)
    // the leaf name is the expected 16-hex-char + `.sock` shape, and
    // (c) different inputs produce different leaves.
    let shared = pick_one(&shared_broker_pipe(ALICE).unwrap());
    let private = pick_one(&private_broker_pipe(ALICE, "zccache").unwrap());
    let instance = pick_one(&explicit_instance_pipe(ALICE, "dev-build").unwrap());
    let backend = pick_one(&backend_pipe(ALICE, &[0u8; 16]).unwrap());
    let mut leaves = std::collections::HashSet::new();
    for s in [&shared, &private, &instance, &backend] {
        assert!(s.ends_with(".sock"), "expected `.sock` suffix in {s:?}");
        let leaf = std::path::Path::new(s)
            .file_name()
            .expect("path must have a leaf")
            .to_string_lossy()
            .into_owned();
        // 16 hex chars + ".sock" = 21 chars
        assert_eq!(leaf.len(), 21, "unexpected leaf length in {s:?}");
        let hex = leaf.trim_end_matches(".sock");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "leaf {leaf:?} must be lowercase hex"
        );
        leaves.insert(leaf);
    }
    assert_eq!(leaves.len(), 4, "each pipe-name input must hash uniquely");
}

#[test]
fn different_users_produce_different_paths() {
    let a = pick_one(&shared_broker_pipe(ALICE).unwrap());
    let b = pick_one(&shared_broker_pipe(BOB).unwrap());
    assert_ne!(a, b);
}

#[test]
fn service_name_validator_accepts_canonical() {
    validate_service_name("zccache").unwrap();
    validate_service_name("a").unwrap();
    validate_service_name("svc-with-dashes").unwrap();
    validate_service_name("0123456789").unwrap();
    validate_service_name(&"a".repeat(64)).unwrap();
}

#[test]
fn service_name_validator_rejects_invalid() {
    assert!(validate_service_name("").is_err());
    assert!(validate_service_name(&"a".repeat(65)).is_err());
    assert!(validate_service_name("UPPER").is_err()); // case-only collision guard
    assert!(validate_service_name("with space").is_err());
    assert!(validate_service_name("dots.are.bad").is_err());
    assert!(validate_service_name("emoji-rocket-🚀").is_err());
}

#[test]
fn case_only_collision_is_rejected_not_silently_merged() {
    // Windows named pipes are case-insensitive. If we accepted both
    // `Zccache` and `zccache` we'd silently merge two distinct
    // services on Windows while keeping them separate on Linux/macOS
    // — a recipe for a hijack-by-misconfiguration class of bug.
    let lower = private_broker_pipe(ALICE, "zccache").unwrap();
    let mixed = private_broker_pipe(ALICE, "Zccache");
    assert!(mixed.is_err());
    // Spot-check that the lowercase form, which IS accepted, contains
    // exactly the canonical service segment. (macOS hashes the leaf,
    // so the literal segment doesn't appear on that platform — we
    // still assert success on Windows/Linux.)
    #[cfg(not(target_os = "macos"))]
    {
        let lower_path = pick_one(&lower);
        assert!(lower_path.contains("-svc-zccache"));
    }
    #[cfg(target_os = "macos")]
    {
        // Just touch the value so the binding isn't unused.
        let _ = pick_one(&lower);
    }
}

#[test]
fn version_validator_accepts_canonical() {
    validate_version("1.0.0").unwrap();
    validate_version("1.11.20").unwrap();
    validate_version("0.0.1-alpha.1").unwrap();
    validate_version("2.3.4-rc.1.beta").unwrap();
}

#[test]
fn version_validator_rejects_invalid() {
    assert!(validate_version("").is_err());
    assert!(validate_version("1.0").is_err());
    assert!(validate_version("1.0.0.0").is_err());
    assert!(validate_version("v1.0.0").is_err());
    assert!(validate_version("1.0.0-").is_err());
    assert!(validate_version("1.0.0-ALPHA").is_err());
}

#[cfg(windows)]
#[test]
fn windows_path_stays_under_max_path_without_long_prefix() {
    // Worst-case input: 64-char service name + 16-char SID hash.
    let long_svc = "a".repeat(64);
    let p = private_broker_pipe(ALICE, &long_svc).unwrap();
    let w = p.windows.expect("Windows form populated on Windows");
    assert!(
        !w.starts_with(r"\\?\"),
        "must not need the long-path prefix"
    );
    assert!(
        w.len() <= WINDOWS_MAX_PATH,
        "Windows pipe {w:?} exceeds MAX_PATH ({})",
        w.len()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_path_stays_under_sun_path() {
    // Worst-case input: 64-char explicit instance name.
    let long_name = "a".repeat(64);
    let p = explicit_instance_pipe(ALICE, &long_name).unwrap();
    let u = p.unix.expect("Unix form populated on macOS");
    let len = u.to_string_lossy().len();
    assert!(
        len < MACOS_SUN_PATH_MAX,
        "macOS pipe path {u:?} too long: {len} >= {MACOS_SUN_PATH_MAX}"
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn backend_pipe_renders_random_as_hex() {
    // macOS hashes the leaf to fit `sun_path`, so the literal hex
    // doesn't appear on that platform — see `macos_pipe_paths_are_hashed_leaves`
    // for the corresponding invariant.
    let bytes = [0xAB_u8; 16];
    let p = backend_pipe(ALICE, &bytes).unwrap();
    let s = pick_one(&p);
    assert!(s.contains("-be-"));
    assert!(s.contains(&"ab".repeat(16)));
}

#[test]
fn invalid_sid_hash_is_rejected() {
    let err = shared_broker_pipe("not-16-chars").unwrap_err();
    match err {
        PipePathError::InvalidName { .. } => {}
        _ => panic!("expected InvalidName, got {err:?}"),
    }
}

#[test]
fn sid_hash_must_be_lowercase_hex() {
    let err = shared_broker_pipe("ZZZZZZZZZZZZZZZZ").unwrap_err();
    match err {
        PipePathError::InvalidName { .. } => {}
        _ => panic!("expected InvalidName, got {err:?}"),
    }
}
