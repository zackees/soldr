//! Unit coverage split from `linker.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;

const LINUX: &str = "x86_64-unknown-linux-gnu";
const LINUX_MUSL: &str = "x86_64-unknown-linux-musl";
const MAC_X64: &str = "x86_64-apple-darwin";
const MAC_ARM: &str = "aarch64-apple-darwin";
const WIN_MSVC: &str = "x86_64-pc-windows-msvc";
const WIN_GNU: &str = "x86_64-pc-windows-gnu";

fn always_false() -> bool {
    false
}

fn assert_apple_fast_linker(injection: &LinkerInjection, triple: &str) {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::MacOs {
        assert!(injection.linker.is_none(), "{triple}");
        assert!(injection.rustflags.is_none(), "{triple}");
    } else {
        assert_eq!(injection.linker.as_deref(), Some("clang"), "{triple}");
        assert_eq!(
            injection.rustflags.as_deref(),
            Some("-C link-arg=-fuse-ld=lld"),
            "{triple}"
        );
    }
}

fn always_true() -> bool {
    true
}

#[test]
fn parses_known_values_case_insensitively() {
    assert_eq!(
        LinkerChoice::from_str("default").unwrap(),
        LinkerChoice::Default
    );
    assert_eq!(LinkerChoice::from_str("LD").unwrap(), LinkerChoice::Ld);
    assert_eq!(LinkerChoice::from_str("Mold").unwrap(), LinkerChoice::Mold);
    assert_eq!(
        LinkerChoice::from_str("rust-lld").unwrap(),
        LinkerChoice::RustLld
    );
    assert_eq!(
        LinkerChoice::from_str("RUST-LLD").unwrap(),
        LinkerChoice::RustLld
    );
    assert_eq!(LinkerChoice::from_str("fast").unwrap(), LinkerChoice::Fast);
}

#[test]
fn empty_parses_as_default() {
    assert_eq!(LinkerChoice::from_str("").unwrap(), LinkerChoice::Default);
    assert_eq!(
        LinkerChoice::from_str("   ").unwrap(),
        LinkerChoice::Default
    );
}

#[test]
fn unknown_value_is_clear_error() {
    let err = LinkerChoice::from_str("gold").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("invalid SOLDR_LINKER value"),
        "unexpected error message: {msg}"
    );
    assert!(msg.contains("gold"), "should echo the bad value: {msg}");
    assert!(
        msg.contains("default") && msg.contains("mold") && msg.contains("rust-lld"),
        "should list valid choices: {msg}"
    );
}

#[test]
fn env_wins_over_config() {
    let choice = from_env_and_config(Some(OsStr::new("mold")), Some("rust-lld")).unwrap();
    assert_eq!(choice, LinkerChoice::Mold);
}

#[test]
fn config_fallback_when_env_unset() {
    let choice = from_env_and_config(None, Some("rust-lld")).unwrap();
    assert_eq!(choice, LinkerChoice::RustLld);
}

#[test]
fn nothing_falls_back_to_default() {
    let choice = from_env_and_config(None, None).unwrap();
    assert_eq!(choice, LinkerChoice::Default);
}

#[test]
fn empty_env_string_falls_back_to_default() {
    let choice = from_env_and_config(Some(OsStr::new("")), Some("mold")).unwrap();
    // Empty env string is treated as "no explicit choice" -> Default.
    assert_eq!(choice, LinkerChoice::Default);
}

#[test]
fn default_and_ld_inject_nothing_on_every_target() {
    for triple in [LINUX, LINUX_MUSL, MAC_X64, MAC_ARM, WIN_MSVC, WIN_GNU] {
        let i =
            resolve_for_target_with_probe(LinkerChoice::Default, triple, &always_false).unwrap();
        assert_eq!(i, LinkerInjection::default(), "default/{triple}");
        let i = resolve_for_target_with_probe(LinkerChoice::Ld, triple, &always_false).unwrap();
        assert_eq!(i, LinkerInjection::default(), "ld/{triple}");
    }
}

#[test]
fn mold_on_linux_uses_clang_with_fuse_mold() {
    let i = resolve_for_target_with_probe(LinkerChoice::Mold, LINUX, &always_false).unwrap();
    assert_eq!(i.linker.as_deref(), Some("clang"));
    assert_eq!(i.rustflags.as_deref(), Some("-C link-arg=-fuse-ld=mold"));
}

#[test]
fn mold_on_macos_returns_clear_error() {
    let err =
        resolve_for_target_with_probe(LinkerChoice::Mold, MAC_X64, &always_false).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("mold is not supported"),
        "unexpected message: {msg}"
    );
    assert!(msg.contains(MAC_X64), "error should name the target: {msg}");
    assert!(msg.contains("fast"), "error should hint at fast: {msg}");
}

#[test]
fn mold_on_windows_returns_clear_error() {
    let err =
        resolve_for_target_with_probe(LinkerChoice::Mold, WIN_MSVC, &always_false).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("mold is not supported"),
        "unexpected message: {msg}"
    );
    assert!(msg.contains(WIN_MSVC), "error should name target: {msg}");
}

#[test]
fn rust_lld_on_msvc_uses_rust_lld_directly() {
    let i = resolve_for_target_with_probe(LinkerChoice::RustLld, WIN_MSVC, &always_false).unwrap();
    assert_eq!(i.linker.as_deref(), Some("rust-lld"));
    assert!(i.rustflags.is_none());
}

#[test]
fn rust_lld_on_non_msvc_non_apple_uses_clang_with_fuse_lld() {
    for triple in [LINUX, LINUX_MUSL, WIN_GNU] {
        let i =
            resolve_for_target_with_probe(LinkerChoice::RustLld, triple, &always_false).unwrap();
        assert_eq!(i.linker.as_deref(), Some("clang"), "{triple}");
        assert_eq!(
            i.rustflags.as_deref(),
            Some("-C link-arg=-fuse-ld=lld"),
            "{triple}"
        );
    }
}

/// Issue #509: Apple clang rejects `-fuse-ld=lld` (it expects
/// `ld64.lld`, which stock macOS toolchains do not ship). `RustLld`
/// on Apple targets must therefore inject nothing and fall back to
/// the platform default linker. This test is host-agnostic because
/// `target_kind` is driven purely by the triple string.
#[test]
fn rust_lld_on_apple_uses_a_macho_capable_linker() {
    for triple in [MAC_X64, MAC_ARM] {
        let i =
            resolve_for_target_with_probe(LinkerChoice::RustLld, triple, &always_false).unwrap();
        assert_apple_fast_linker(&i, triple);
    }
}

#[test]
fn fast_on_linux_prefers_mold_when_present() {
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, LINUX, &always_true).unwrap();
    assert_eq!(i.linker.as_deref(), Some("clang"));
    assert_eq!(i.rustflags.as_deref(), Some("-C link-arg=-fuse-ld=mold"));
}

#[test]
fn fast_on_linux_falls_back_to_rust_lld_when_mold_absent() {
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, LINUX, &always_false).unwrap();
    assert_eq!(i.linker.as_deref(), Some("clang"));
    assert_eq!(i.rustflags.as_deref(), Some("-C link-arg=-fuse-ld=lld"));
}

/// Issue #509: `SOLDR_LINKER=fast` on macOS used to inject
/// `-fuse-ld=lld`, which breaks Apple-clang-driven `cc-rs` build
/// scripts ("invalid linker name in argument '-fuse-ld=lld'"). The
/// fast mode must now silently fall back to the platform default
/// linker on every Apple target, regardless of the host that ran the
/// resolver — so this test covers the bug whether it executes on
/// Linux, macOS, or Windows.
#[test]
fn fast_on_apple_uses_a_macho_capable_linker() {
    for triple in [MAC_X64, MAC_ARM] {
        let i = resolve_for_target_with_probe(LinkerChoice::Fast, triple, &always_false).unwrap();
        assert_apple_fast_linker(&i, triple);
        // Also exercise the mold-present branch — mold is irrelevant
        // on Apple targets and must not change the outcome.
        let i = resolve_for_target_with_probe(LinkerChoice::Fast, triple, &always_true).unwrap();
        assert_apple_fast_linker(&i, triple);
    }
}

#[test]
fn fast_on_windows_msvc_uses_rust_lld_directly() {
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, WIN_MSVC, &always_false).unwrap();
    assert_eq!(i.linker.as_deref(), Some("rust-lld"));
    assert!(i.rustflags.is_none());
}

#[test]
fn fast_on_windows_gnu_keeps_the_managed_gcc_linker() {
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, WIN_GNU, &always_false).unwrap();
    assert_eq!(i, LinkerInjection::default());
}

// soldr#1992 / soldr#1999 rule 1. When the standard-linker retry also
// fails, the user's last screen is that second build's output -- carrying
// rustc's "the Visual Studio build tools may need to be repaired" note.
// The retry warning that would explain it scrolled past a whole build ago.
// These assert the note does the one job that matters: contradicting the
// false lead at the point where the reader is looking.
#[test]
fn the_fallback_failure_note_clears_the_fast_linker_and_the_false_lead() {
    let note = fallback_also_failed_note("rust-lld");
    assert!(
        note.contains("rust-lld"),
        "must name what was ruled out: {note}"
    );
    assert!(
        note.contains("was not the cause"),
        "must exonerate the fast linker explicitly: {note}"
    );
    assert!(
        note.contains("repair your build tools"),
        "must quote the misleading advice it is rebutting: {note}"
    );
    assert!(
        note.contains("second attempt"),
        "must say which build the errors came from: {note}"
    );
}

// A successful fallback must not print the failure note -- telling a user
// their build failed when it succeeded is worse than saying nothing.
#[test]
fn a_successful_fallback_reports_success_not_failure() {
    let note = fallback_also_failed_note("rust-lld");
    assert!(
        !note.contains("succeeded"),
        "the failure note must never read as success: {note}"
    );
}

const MSVC: &str = "x86_64-pc-windows-msvc";

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

// soldr#1992: the failing shape, exactly as cargo emits it.
#[test]
fn proc_macro_on_msvc_loses_the_injected_rust_lld() {
    let args = argv(&[
        "rustc",
        "--crate-name",
        "serde_derive",
        "--crate-type",
        "proc-macro",
        "-C",
        "prefer-dynamic",
        "-C",
        "linker=rust-lld",
    ]);
    let out = strip_fast_linker_for_proc_macro(&args, MSVC);
    assert!(!out.iter().any(|a| a == "linker=rust-lld"), "{out:?}");
    assert!(
        out.iter().any(|a| a == "prefer-dynamic"),
        "must touch only the linker: {out:?}"
    );
    assert!(out.iter().any(|a| a == "serde_derive"), "{out:?}");
}

#[test]
fn the_joined_spelling_is_also_removed() {
    let args = argv(&["rustc", "--crate-type=proc-macro", "-Clinker=rust-lld"]);
    let out = strip_fast_linker_for_proc_macro(&args, MSVC);
    assert!(!out.iter().any(|a| a.contains("rust-lld")), "{out:?}");
}

// Ordinary crates keep the fast linker -- that is the whole point of the
// feature, and rlib compiles were never the failing case.
#[test]
fn a_non_proc_macro_crate_keeps_rust_lld() {
    let args = argv(&["rustc", "--crate-type", "lib", "-C", "linker=rust-lld"]);
    let out = strip_fast_linker_for_proc_macro(&args, MSVC);
    assert_eq!(out.as_ref(), args.as_slice());
}

// rust-lld links proc-macro dylibs fine off MSVC; stripping there would
// silently forfeit the fast linker for every derive crate.
#[test]
fn a_proc_macro_off_msvc_keeps_rust_lld() {
    let args = argv(&[
        "rustc",
        "--crate-type",
        "proc-macro",
        "-C",
        "linker=rust-lld",
    ]);
    let out = strip_fast_linker_for_proc_macro(&args, LINUX);
    assert_eq!(out.as_ref(), args.as_slice());
}

// An explicit --target decides, not the host.
#[test]
fn an_explicit_msvc_target_is_honoured_from_a_non_msvc_host() {
    let args = argv(&[
        "rustc",
        "--crate-type",
        "proc-macro",
        "--target",
        MSVC,
        "-C",
        "linker=rust-lld",
    ]);
    let out = strip_fast_linker_for_proc_macro(&args, LINUX);
    assert!(!out.iter().any(|a| a == "linker=rust-lld"), "{out:?}");
}

// A different linker is not ours to remove.
#[test]
fn another_linker_is_left_alone() {
    let args = argv(&[
        "rustc",
        "--crate-type",
        "proc-macro",
        "-C",
        "linker=lld-link",
    ]);
    let out = strip_fast_linker_for_proc_macro(&args, MSVC);
    assert_eq!(out.as_ref(), args.as_slice());
}

#[test]
fn linker_failure_classifier_ignores_non_linker_failures() {
    assert!(!looks_like_linker_failure_text(
        "error: failed to parse source file"
    ));
    assert!(looks_like_linker_failure_text(
        "error: linking with `clang` failed: mold not found"
    ));
}

#[test]
fn fallback_record_is_idempotent_and_corruption_tolerant() {
    let root = tempfile::tempdir().expect("temporary soldr root");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());
    record_pep517_fallback(&paths, Some("key-a")).expect("record fallback");
    record_pep517_fallback(&paths, Some("key-a")).expect("record duplicate fallback");
    record_pep517_fallback(&paths, Some("key-b")).expect("record second fallback");
    let contents = std::fs::read_to_string(fallback_cache_path(&paths)).unwrap();
    assert_eq!(contents.lines().collect::<Vec<_>>(), ["key-a", "key-b"]);
    assert!(!fallback_cache_contains(&paths, "key-corrupt"));
}

#[test]
fn cargo_target_env_prefix_uppercases_and_replaces_hyphens() {
    assert_eq!(
        cargo_target_env_prefix("x86_64-unknown-linux-gnu"),
        "X86_64_UNKNOWN_LINUX_GNU"
    );
    assert_eq!(
        cargo_target_env_prefix("aarch64-apple-darwin"),
        "AARCH64_APPLE_DARWIN"
    );
    assert_eq!(
        cargo_target_env_prefix("x86_64-pc-windows-msvc"),
        "X86_64_PC_WINDOWS_MSVC"
    );
}
