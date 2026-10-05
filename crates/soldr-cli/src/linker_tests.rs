//! Unit coverage split from `linker.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;
use crate::linker_shim::render_windows_linker_driver_shim;

const LINUX: &str = "x86_64-unknown-linux-gnu";
const LINUX_MUSL: &str = "x86_64-unknown-linux-musl";
const MAC_X64: &str = "x86_64-apple-darwin";
const MAC_ARM: &str = "aarch64-apple-darwin";
const WIN_MSVC: &str = "x86_64-pc-windows-msvc";
const WIN_GNU: &str = "x86_64-pc-windows-gnu";
fn fake_clang() -> &'static Path {
    Path::new("/managed/llvm/bin/clang")
}

/// soldr#3430: the shim execs the resolved absolute clang and never a bare
/// `clang` looked up from whatever search path the link happens to run under.
#[test]
fn linux_driver_shim_execs_an_absolute_clang_never_a_bare_one() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().to_path_buf());
    let mut injection = LinkerInjection::clang_with_fuse("lld");
    materialize_linker_driver_shim(&paths, LINUX, &mut injection, Some(fake_clang())).unwrap();
    let body = std::fs::read_to_string(injection.linker.unwrap()).unwrap();
    assert!(body.contains("exec '/managed/llvm/bin/clang' "), "{body}");
    assert!(!body.contains("exec clang"), "{body}");

    let mut unresolved = LinkerInjection::clang_with_fuse("lld");
    let error = materialize_linker_driver_shim(&paths, LINUX, &mut unresolved, None)
        .expect_err("a Unix shim without a resolved clang must not render");
    assert!(error.to_string().contains("resolved clang"), "{error}");
}

#[test]
fn missing_clang_error_names_clang_the_asset_and_the_override() {
    let text = crate::linker_shim::missing_clang_error_text("offline");
    assert!(text.contains("clang") && text.contains("llvm-"), "{text}");
    assert!(
        text.contains("SOLDR_LLVM_DIR") && text.contains("offline"),
        "{text}"
    );
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
    assert_eq!(LinkerChoice::from_str("reld").unwrap(), LinkerChoice::Reld);
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
fn nothing_falls_back_to_fast() {
    // soldr#3262: reld is the default linker.
    let choice = from_env_and_config(None, None).unwrap();
    assert_eq!(choice, LinkerChoice::Fast);
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
        let i = resolve_for_target(LinkerChoice::Default, triple).unwrap();
        assert_eq!(i, LinkerInjection::default(), "default/{triple}");
        let i = resolve_for_target(LinkerChoice::Ld, triple).unwrap();
        assert_eq!(i, LinkerInjection::default(), "ld/{triple}");
    }
}

#[test]
fn mold_on_linux_uses_clang_with_fuse_mold() {
    let i = resolve_for_target(LinkerChoice::Mold, LINUX).unwrap();
    assert_eq!(i.linker.as_deref(), Some("clang"));
    assert_eq!(i.rustflags.as_deref(), Some("-C link-arg=-fuse-ld=mold"));
}

#[test]
fn mold_on_macos_returns_clear_error() {
    let err = resolve_for_target(LinkerChoice::Mold, MAC_X64).unwrap_err();
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
    let err = resolve_for_target(LinkerChoice::Mold, WIN_MSVC).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("mold is not supported"),
        "unexpected message: {msg}"
    );
    assert!(msg.contains(WIN_MSVC), "error should name target: {msg}");
}

#[test]
fn rust_lld_on_msvc_uses_rust_lld_directly() {
    let i = resolve_for_target(LinkerChoice::RustLld, WIN_MSVC).unwrap();
    assert_eq!(i.linker.as_deref(), Some("rust-lld"));
    assert!(i.rustflags.is_none());
}

#[test]
fn reld_injects_reld_linker_on_every_target() {
    // reld is a drop-in linker with a native ELF backend (Linux) plus an lld
    // bridge for COFF/Mach-O. On Linux its native backend does not inject the
    // CRT startup objects, so reld is driven through clang (`--ld-path=reld`,
    // since `-fuse-ld` rejects unknown linker names) there. On Windows it
    // bridges to lld-link, which takes rustc's MSVC argv, so reld is injected
    // directly with no extra flags.
    // soldr#3359: Apple targets also go through clang, because rustc's
    // `darwin-cc` flavor hands the linker clang-driver arguments.
    for triple in [LINUX, LINUX_MUSL, MAC_X64, MAC_ARM] {
        let i = resolve_for_target(LinkerChoice::Reld, triple).unwrap();
        assert_eq!(i.linker.as_deref(), Some("clang"), "{triple}");
        assert_eq!(
            i.rustflags.as_deref(),
            Some("-C link-arg=--ld-path=reld"),
            "{triple}"
        );
    }
    for triple in [WIN_MSVC, WIN_GNU] {
        let i = resolve_for_target(LinkerChoice::Reld, triple).unwrap();
        assert_eq!(i.linker.as_deref(), Some("reld"), "{triple}");
        assert!(i.rustflags.is_none(), "{triple}");
    }
}

#[test]
fn rust_lld_on_non_msvc_non_apple_uses_clang_with_fuse_lld() {
    for triple in [LINUX, LINUX_MUSL, WIN_GNU] {
        let i = resolve_for_target(LinkerChoice::RustLld, triple).unwrap();
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
        let i = resolve_for_target(LinkerChoice::RustLld, triple).unwrap();
        assert_apple_fast_linker(&i, triple);
    }
}

#[test]
fn fast_on_linux_uses_reld_via_clang_ld_path() {
    // reld's native ELF backend does not inject CRT startup objects, so
    // `Fast` (reld) is driven through clang (`--ld-path=reld`) on Linux.
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, LINUX, &|| true).unwrap();
    assert_eq!(i.linker.as_deref(), Some("clang"));
    assert_eq!(i.rustflags.as_deref(), Some("-C link-arg=--ld-path=reld"));
}

#[test]
fn fast_on_linux_without_reld_falls_back_to_lld() {
    // reld is not yet bundled or universally installed (soldr#3262); `Fast`
    // must degrade to rust-lld rather than fail the link with
    // `invalid linker name in argument '--ld-path=reld'`.
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, LINUX, &|| false).unwrap();
    assert_eq!(i.linker.as_deref(), Some("clang"));
    assert_eq!(i.rustflags.as_deref(), Some("-C link-arg=-fuse-ld=lld"));
}

#[test]
fn linux_driver_shim_preserves_build_rustflags_and_is_content_addressed() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().to_path_buf());
    let mut injection = LinkerInjection::clang_with_ld_path("/managed/reld with space");

    materialize_linker_driver_shim(&paths, LINUX, &mut injection, Some(fake_clang())).unwrap();

    assert!(injection.rustflags.is_none());
    let path = PathBuf::from(injection.linker.as_deref().unwrap());
    assert!(path.starts_with(paths.bin.join("linker-shims").join("v1")));
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("--ld-path=/managed/reld with space"));

    let first_path = path;
    let mut different = LinkerInjection::clang_with_fuse("lld");
    materialize_linker_driver_shim(&paths, LINUX, &mut different, Some(fake_clang())).unwrap();
    assert_ne!(PathBuf::from(different.linker.unwrap()), first_path);
    assert!(different.rustflags.is_none());
}

#[test]
fn linker_driver_shim_forwards_the_driver_argument_and_linker_argv() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let fake_bin = temp.path().join("fake bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let log = temp.path().join("clang-argv.txt");
    let clang = fake_bin.join("clang");
    std::fs::write(
        &clang,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$SOLDR_TEST_CLANG_LOG\"\n",
    )
    .unwrap();
    crate::platform::fs::permissions::make_executable(&clang).unwrap();

    let paths = SoldrPaths::with_root(temp.path().join("soldr root"));
    let mut injection = LinkerInjection::clang_with_ld_path("/managed/reld with space");
    materialize_linker_driver_shim(&paths, LINUX, &mut injection, Some(&clang)).unwrap();
    let status = Command::new(injection.linker.unwrap())
        .args(["first object.o", "-o", "output file"])
        .env("PATH", &fake_bin)
        .env("SOLDR_TEST_CLANG_LOG", &log)
        .status()
        .unwrap();

    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(log).unwrap(),
        "--ld-path=/managed/reld with space\nfirst object.o\n-o\noutput file\n"
    );
}

#[test]
fn windows_linker_driver_shim_quotes_spaces_and_cmd_metacharacters() {
    let body = render_windows_linker_driver_shim(
        "C:\\LLVM 100%\\bin\\clang.exe",
        "--ld-path=C:\\A B\\100% & tools\\reld.exe",
    );
    assert_eq!(
        body,
        "@echo off\r\n\"C:\\LLVM 100%%\\bin\\clang.exe\" \
         \"--ld-path=C:\\A B\\100%% & tools\\reld.exe\" %*\r\n"
    );
    assert!(!body.contains("\r\nclang "), "never a bare clang: {body}");
}

#[test]
fn fast_on_apple_uses_reld_via_clang_ld_path() {
    for triple in [MAC_X64, MAC_ARM] {
        let i = resolve_for_target_with_probe(LinkerChoice::Fast, triple, &|| true).unwrap();
        assert_eq!(i.linker.as_deref(), Some("clang"), "{triple}");
        assert_eq!(
            i.rustflags.as_deref(),
            Some("-C link-arg=--ld-path=reld"),
            "{triple}"
        );
    }
}

/// soldr#3359: the Apple clang route carries its `--ld-path` in a
/// content-addressed shim, like Linux, so it never enters Cargo's rustflags
/// precedence (which would replace a project's own `[build] rustflags`).
#[test]
fn apple_reld_driver_argument_moves_into_a_linker_shim() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().to_path_buf());
    let mut injection = resolve_for_target(LinkerChoice::Reld, MAC_ARM).unwrap();
    inject_resolved_reld(&mut injection, Path::new("/managed/reld")).unwrap();

    materialize_linker_driver_shim(&paths, MAC_ARM, &mut injection, Some(fake_clang())).unwrap();

    assert!(injection.rustflags.is_none());
    let path = PathBuf::from(injection.linker.as_deref().unwrap());
    assert!(path.starts_with(paths.bin.join("linker-shims").join("v1")));
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("--ld-path=/managed/reld"), "{body}");
}

#[test]
fn fast_on_apple_without_reld_falls_back_to_platform_linker() {
    for triple in [MAC_X64, MAC_ARM] {
        let i = resolve_for_target_with_probe(LinkerChoice::Fast, triple, &|| false).unwrap();
        assert_apple_fast_linker(&i, triple);
    }
}

#[test]
fn fast_on_windows_msvc_uses_reld() {
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, WIN_MSVC, &|| true).unwrap();
    assert_eq!(i.linker.as_deref(), Some("reld"));
    assert!(i.rustflags.is_none());
}

#[test]
fn fast_on_windows_msvc_without_reld_falls_back_to_rust_lld() {
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, WIN_MSVC, &|| false).unwrap();
    assert_eq!(i.linker.as_deref(), Some("rust-lld"));
    assert!(i.rustflags.is_none());
}

#[test]
fn fast_on_windows_gnu_uses_reld() {
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, WIN_GNU, &|| true).unwrap();
    assert_eq!(i.linker.as_deref(), Some("reld"));
    assert!(i.rustflags.is_none());
}

#[test]
fn fast_on_windows_gnu_without_reld_uses_the_bundle_linker() {
    // The blessed Windows GNU lifecycle provisions a relocatable GCC bundle
    // with its matching binutils, so the portable fallback is no injection.
    let i = resolve_for_target_with_probe(LinkerChoice::Fast, WIN_GNU, &|| false).unwrap();
    assert!(i.linker.is_none());
    assert!(i.rustflags.is_none());
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

// soldr#3262: reld is the fast/default linker and bridges to lld-link on
// MSVC, so it has the same proc-macro-DLL failure mode as rust-lld and must
// be stripped identically.
#[test]
fn proc_macro_on_msvc_loses_the_injected_reld() {
    let args = argv(&["rustc", "--crate-type", "proc-macro", "-C", "linker=reld"]);
    let out = strip_fast_linker_for_proc_macro(&args, MSVC);
    assert!(!out.iter().any(|a| a == "linker=reld"), "{out:?}");
}

#[test]
fn the_joined_reld_spelling_is_also_removed() {
    let args = argv(&["rustc", "--crate-type=proc-macro", "-Clinker=reld"]);
    let out = strip_fast_linker_for_proc_macro(&args, MSVC);
    assert!(!out.iter().any(|a| a.contains("reld")), "{out:?}");
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

#[test]
fn extract_ld_path_reads_the_linker_name() {
    assert_eq!(extract_ld_path("-C link-arg=--ld-path=reld"), Some("reld"));
    assert_eq!(
        extract_ld_path("-C link-arg=--ld-path=ld.lld"),
        Some("ld.lld")
    );
    assert_eq!(extract_ld_path("-C link-arg=-fuse-ld=lld"), None);
    assert_eq!(extract_ld_path("--ld-path="), None);
}

// soldr#3277: a project's own `.cargo/config.toml` `[target.<triple>]` linker
// settings must be readable so soldr can decline to override them.

#[test]
fn project_target_linker_is_read_from_cargo_config() {
    let root = tempfile::tempdir().expect("temporary project root");
    std::fs::create_dir_all(root.path().join(".cargo")).expect("create .cargo dir");
    std::fs::write(
        root.path().join(".cargo/config.toml"),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"cc\"\n",
    )
    .expect("write .cargo/config.toml");
    assert_eq!(
        target_config_value_in_root(root.path(), LINUX, "linker"),
        Some("cc".to_string())
    );
}

#[test]
fn project_target_rustflags_array_is_joined() {
    let root = tempfile::tempdir().expect("temporary project root");
    std::fs::create_dir_all(root.path().join(".cargo")).expect("create .cargo dir");
    std::fs::write(
        root.path().join(".cargo/config.toml"),
        "[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-C\", \"link-arg=-fuse-ld=lld\"]\n",
    )
    .expect("write .cargo/config.toml");
    assert_eq!(
        target_config_value_in_root(root.path(), LINUX, "rustflags"),
        Some("-C link-arg=-fuse-ld=lld".to_string())
    );
}

#[test]
fn project_target_config_ignores_other_triples() {
    let root = tempfile::tempdir().expect("temporary project root");
    std::fs::create_dir_all(root.path().join(".cargo")).expect("create .cargo dir");
    std::fs::write(
        root.path().join(".cargo/config.toml"),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"cc\"\n",
    )
    .expect("write .cargo/config.toml");
    assert!(target_config_value_in_root(root.path(), WIN_MSVC, "linker").is_none());
}

#[test]
fn project_target_config_ignores_empty_values() {
    let root = tempfile::tempdir().expect("temporary project root");
    std::fs::create_dir_all(root.path().join(".cargo")).expect("create .cargo dir");
    std::fs::write(
        root.path().join(".cargo/config.toml"),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"   \"\n",
    )
    .expect("write .cargo/config.toml");
    assert!(target_config_value_in_root(root.path(), LINUX, "linker").is_none());
}

#[test]
fn project_target_config_missing_file_is_none() {
    let root = tempfile::tempdir().expect("temporary project root");
    assert!(target_config_value_in_root(root.path(), LINUX, "linker").is_none());
    assert!(target_config_value_in_root(root.path(), LINUX, "rustflags").is_none());
}

#[test]
fn project_target_config_dot_cargo_config_without_extension_is_read() {
    let root = tempfile::tempdir().expect("temporary project root");
    std::fs::create_dir_all(root.path().join(".cargo")).expect("create .cargo dir");
    std::fs::write(
        root.path().join(".cargo/config"),
        "[target.aarch64-apple-darwin]\nlinker = \"cc\"\n",
    )
    .expect("write legacy .cargo/config");
    assert_eq!(
        target_config_value_in_root(root.path(), MAC_ARM, "linker"),
        Some("cc".to_string())
    );
}

// soldr#3276: the shared project-level linker resolver.

fn write_cargo_config(root: &Path, body: &str) {
    std::fs::create_dir_all(root.join(".cargo")).expect("create .cargo dir");
    std::fs::write(root.join(".cargo/config.toml"), body).expect("write .cargo/config.toml");
}

fn write_cargo_toml_metadata(root: &Path, body: &str) {
    std::fs::write(root.join("Cargo.toml"), body).expect("write Cargo.toml");
}

#[test]
fn resolve_project_choice_env_wins_over_everything() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(
        root.path(),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"reld\"\n",
    );
    write_cargo_toml_metadata(
        root.path(),
        "[package]\nname = \"x\"\nversion = \"0\"\n\n[package.metadata.soldr]\nlinker = \"mold\"\n",
    );
    let selection = resolve_project_choice(
        Some(OsStr::new("ld")),
        Some("rust-lld"),
        Some(LINUX),
        root.path(),
        None,
    )
    .expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Ld);
    assert_eq!(selection.source, LinkerSource::Env);
    assert!(selection.reld_cargo_config.is_none());
    assert!(selection.is_explicit());
    assert!(!selection.needs_reld());
}

#[test]
fn resolve_project_choice_cargo_config_wins_over_metadata_and_user_config() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(
        root.path(),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"reld\"\n",
    );
    write_cargo_toml_metadata(
        root.path(),
        "[package]\nname = \"x\"\nversion = \"0\"\n\n[package.metadata.soldr]\nlinker = \"mold\"\n",
    );
    let selection = resolve_project_choice(None, Some("rust-lld"), Some(LINUX), root.path(), None)
        .expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Reld);
    assert_eq!(selection.source, LinkerSource::CargoConfig);
    assert_eq!(
        selection.reld_cargo_config,
        Some(ReldCargoConfig::BareLinker)
    );
    assert!(selection.needs_reld());
}

#[test]
fn resolve_project_choice_metadata_wins_over_user_config() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_toml_metadata(
        root.path(),
        "[package]\nname = \"x\"\nversion = \"0\"\n\n[package.metadata.soldr]\nlinker = \"mold\"\n",
    );
    let selection = resolve_project_choice(None, Some("rust-lld"), Some(LINUX), root.path(), None)
        .expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Mold);
    assert_eq!(selection.source, LinkerSource::CargoTomlMetadata);
    assert!(selection.reld_cargo_config.is_none());
}

#[test]
fn resolve_project_choice_package_metadata_fallback_when_no_workspace() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_toml_metadata(
        root.path(),
        "[package]\nname = \"x\"\nversion = \"0\"\n\n[package.metadata.soldr]\nlinker = \"rust-lld\"\n",
    );
    let selection =
        resolve_project_choice(None, None, Some(LINUX), root.path(), None).expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::RustLld);
    assert_eq!(selection.source, LinkerSource::CargoTomlMetadata);
}

#[test]
fn resolve_project_choice_invalid_metadata_value_errors() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_toml_metadata(
        root.path(),
        "[package]\nname = \"x\"\nversion = \"0\"\n\n[package.metadata.soldr]\nlinker = \"gold\"\n",
    );
    let err = resolve_project_choice(None, None, Some(LINUX), root.path(), None).unwrap_err();
    assert!(err.to_string().contains("invalid SOLDR_LINKER value"));
}

#[test]
fn resolve_project_choice_user_config_when_nothing_else_declares() {
    let root = tempfile::tempdir().expect("project root");
    let selection = resolve_project_choice(None, Some("mold"), Some(LINUX), root.path(), None)
        .expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Mold);
    assert_eq!(selection.source, LinkerSource::UserConfig);
}

#[test]
fn resolve_project_choice_default_when_nothing_declares_anything() {
    let root = tempfile::tempdir().expect("project root");
    let selection =
        resolve_project_choice(None, None, Some(LINUX), root.path(), None).expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Fast);
    assert_eq!(selection.source, LinkerSource::Default);
    assert!(!selection.is_explicit());
}

#[test]
fn resolve_project_choice_bare_reld_rustflags_is_detected() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(
        root.path(),
        "[target.x86_64-unknown-linux-gnu]\nrustflags = [\"-C\", \"link-arg=--ld-path=reld\"]\n",
    );
    let selection =
        resolve_project_choice(None, None, Some(LINUX), root.path(), None).expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Reld);
    assert_eq!(selection.source, LinkerSource::CargoConfig);
    assert_eq!(
        selection.reld_cargo_config,
        Some(ReldCargoConfig::Rustflags)
    );
    assert!(selection.needs_reld());
}

#[test]
fn resolve_project_choice_absolute_path_reld_linker_is_not_bare() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(
        root.path(),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"/opt/reld/bin/reld\"\n",
    );
    let selection =
        resolve_project_choice(None, None, Some(LINUX), root.path(), None).expect("resolve");
    // An absolute-path reld is left alone: `Default` injects nothing so
    // soldr does not overwrite the project's own pinned linker.
    assert_eq!(selection.choice, LinkerChoice::Default);
    assert_eq!(selection.source, LinkerSource::CargoConfig);
    assert!(selection.reld_cargo_config.is_none());
    assert!(!selection.needs_reld());
}

#[test]
fn resolve_project_choice_other_declared_linker_suppresses_default_without_reld() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(
        root.path(),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"cc\"\n",
    );
    let selection = resolve_project_choice(None, Some("mold"), Some(LINUX), root.path(), None)
        .expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Default);
    assert_eq!(selection.source, LinkerSource::CargoConfig);
}

#[test]
fn resolve_project_choice_reads_cargo_home_config_when_project_has_none() {
    let root = tempfile::tempdir().expect("project root");
    let cargo_home = tempfile::tempdir().expect("cargo home");
    std::fs::write(
        cargo_home.path().join("config.toml"),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"reld\"\n",
    )
    .expect("write cargo home config");
    let selection = resolve_project_choice(
        None,
        None,
        Some(LINUX),
        root.path(),
        Some(cargo_home.path()),
    )
    .expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Reld);
    assert_eq!(selection.source, LinkerSource::CargoConfig);
    assert_eq!(
        selection.reld_cargo_config,
        Some(ReldCargoConfig::BareLinker)
    );
}

#[test]
fn resolve_project_choice_project_config_wins_over_cargo_home() {
    let root = tempfile::tempdir().expect("project root");
    let cargo_home = tempfile::tempdir().expect("cargo home");
    write_cargo_config(
        root.path(),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"cc\"\n",
    );
    std::fs::write(
        cargo_home.path().join("config.toml"),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"reld\"\n",
    )
    .expect("write cargo home config");
    let selection = resolve_project_choice(
        None,
        None,
        Some(LINUX),
        root.path(),
        Some(cargo_home.path()),
    )
    .expect("resolve");
    // Project config declares `cc`; cargo_home's `reld` must not surface.
    assert_eq!(selection.choice, LinkerChoice::Default);
    assert_eq!(selection.source, LinkerSource::CargoConfig);
}

#[test]
fn resolve_project_choice_missing_cargo_toml_is_not_an_error() {
    let root = tempfile::tempdir().expect("project root");
    let selection = resolve_project_choice(None, Some("mold"), Some(LINUX), root.path(), None)
        .expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Mold);
    assert_eq!(selection.source, LinkerSource::UserConfig);
}

#[test]
fn resolve_project_choice_no_target_skips_cargo_config_lookup() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(
        root.path(),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"reld\"\n",
    );
    let selection =
        resolve_project_choice(None, Some("mold"), None, root.path(), None).expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Mold);
    assert_eq!(selection.source, LinkerSource::UserConfig);
}

#[test]
fn linker_candidate_identity_differs_across_reld_versions() {
    let mut a = LinkerInjection::reld();
    a.linker = Some("/managed/reld-0.1.0/reld".to_string());
    let mut b = LinkerInjection::reld();
    b.linker = Some("/managed/reld-0.2.0/reld".to_string());
    assert_ne!(linker_candidate_identity(&a), linker_candidate_identity(&b));
}

#[test]
fn project_target_config_matches_cfg_sections() {
    // soldr#3483 (closing the soldr#3277 limitation): cfg-spec target
    // sections such as `[target.'cfg(all())']` — how every dylint crate
    // declares its linker — are now detected by
    // `target_config_value_in_root`, which previously matched only exact
    // triple keys and silently ignored them. An exact-triple section in
    // the same file still wins over the cfg form (cargo's precedence).
    let root = tempfile::tempdir().expect("temporary project root");
    std::fs::create_dir_all(root.path().join(".cargo")).expect("create .cargo dir");
    std::fs::write(
        root.path().join(".cargo/config.toml"),
        "[target.'cfg(all())']\nrustflags = [\"-C\", \"linker=dylint-link\"]\n",
    )
    .expect("write .cargo/config.toml");
    let value = target_config_value_in_root(root.path(), LINUX, "rustflags");
    assert!(
        value.as_deref().is_some_and(|v| v.contains("dylint-link")),
        "cfg(all()) rustflags must be visible to the guard, got {value:?}"
    );

    // Exact triple beats cfg(all()) within one file.
    std::fs::write(
        root.path().join(".cargo/config.toml"),
        "[target.'cfg(all())']\nrustflags = [\"cfg\"]\n\n[target.x86_64-unknown-linux-gnu]\nrustflags = [\"exact\"]\n",
    )
    .expect("rewrite .cargo/config.toml");
    let value = target_config_value_in_root(root.path(), LINUX, "rustflags");
    assert!(
        value.as_deref().is_some_and(|v| v.contains("exact")),
        "the exact triple must win over cfg(all()), got {value:?}"
    );
}

#[test]
fn the_lld_requirement_follows_the_driver_argument_and_lld_must_be_reachable() {
    use crate::linker_shim::{driver_arg_needs_lld_for_tests, lld_reachable_for_tests};
    assert!(driver_arg_needs_lld_for_tests("-fuse-ld=lld"));
    assert!(!driver_arg_needs_lld_for_tests("--ld-path=/managed/reld"));

    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let clang = bin.join("clang");
    std::fs::write(&clang, b"").unwrap();
    assert!(!lld_reachable_for_tests(&clang, None), "no lld anywhere");
    std::fs::write(bin.join("ld.lld"), b"").unwrap();
    assert!(lld_reachable_for_tests(&clang, None), "lld beside clang");

    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("ld.lld"), b"").unwrap();
    let lonely = temp.path().join("lonely");
    std::fs::create_dir_all(&lonely).unwrap();
    let other_clang = lonely.join("clang");
    std::fs::write(&other_clang, b"").unwrap();
    let search = std::env::join_paths([&elsewhere]).unwrap();
    assert!(
        lld_reachable_for_tests(&other_clang, Some(&search)),
        "lld on the search path"
    );
}

/// soldr#3430: CI runners have a clang without an lld and cannot fetch the
/// managed LLVM. That must degrade to the old behavior (use the clang, warn),
/// not fail with a message claiming no clang exists.
#[test]
fn an_unavailable_managed_llvm_falls_back_to_the_system_clang_with_a_warning() {
    use crate::linker_shim::pick_after_managed_for_tests as pick;
    let system = Some(PathBuf::from("/usr/bin/clang"));

    let (clang, warning) = pick(system.clone(), Ok(PathBuf::from("/managed/clang"))).unwrap();
    assert_eq!(
        clang,
        PathBuf::from("/managed/clang"),
        "managed wins when available"
    );
    assert!(warning.is_none());

    let (clang, warning) = pick(system, Err("catalogue offline".into())).unwrap();
    assert_eq!(clang, PathBuf::from("/usr/bin/clang"));
    let warning = warning.expect("the fallback must be visible");
    assert!(
        warning.contains("catalogue offline") && warning.contains("/usr/bin/clang"),
        "{warning}"
    );

    assert_eq!(
        pick(None, Err("catalogue offline".into())).unwrap_err(),
        "catalogue offline",
        "with neither, the fetch error is the cause"
    );
}

// --- soldr#3483: the guard must see the shapes Dylint lints actually use ---

/// Every Dylint lint crate declares its linker under the universally-matching
/// `[target.'cfg(all())']` section (running-process: `linker = "dylint-link"`;
/// this repo's six lints: `rustflags = ["-C", "linker=dylint-link"]`). Cargo
/// applies that section to every target; before soldr#3483 the guard only
/// looked at exact-triple sections, found nothing, resolved the automatic
/// `Fast` default, and injected `CARGO_TARGET_<triple>_LINKER` — which Cargo
/// gives precedence over every `cfg` section, silently disabling
/// `dylint-link` for the lint build.
#[test]
fn resolve_project_choice_honors_the_universal_cfg_all_section() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(
        root.path(),
        "[target.'cfg(all())']\nlinker = \"dylint-link\"\n",
    );
    let selection =
        resolve_project_choice(None, None, Some(LINUX), root.path(), None).expect("resolve");
    assert_eq!(
        selection.source,
        LinkerSource::CargoConfig,
        "the cfg(all()) declaration must be what the guard saw, not the fallback default"
    );
    assert_eq!(
        selection.choice,
        LinkerChoice::Default,
        "a declared non-reld linker suppresses the automatic default (soldr#3277)"
    );
    assert!(selection.reld_cargo_config.is_none());
}

/// The same shape carries a *bare reld* declaration for other projects, and
/// the cfg(all()) fallback must classify it exactly as an exact-triple one —
/// the section key changes nothing about the value's meaning.
#[test]
fn resolve_project_choice_sees_a_bare_reld_declared_under_cfg_all() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(root.path(), "[target.'cfg(all())']\nlinker = \"reld\"\n");
    let selection =
        resolve_project_choice(None, None, Some(LINUX), root.path(), None).expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Reld);
    assert_eq!(selection.source, LinkerSource::CargoConfig);
    assert_eq!(
        selection.reld_cargo_config,
        Some(ReldCargoConfig::BareLinker)
    );
}

/// Within one file an exact `[target.<triple>]` section outranks
/// `[target.'cfg(all())']`, mirroring Cargo's rule that a `<triple>` linker
/// beats a `<cfg>` one. If the cfg section won, the bare `reld` here would
/// still resolve — so the fixture gives each section a *different* declared
/// linker and asserts the triple's value is the one classified.
#[test]
fn an_exact_triple_section_outranks_cfg_all_in_the_same_file() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(
        root.path(),
        "[target.'cfg(all())']\nlinker = \"dylint-link\"\n\n\
         [target.x86_64-unknown-linux-gnu]\nlinker = \"reld\"\n",
    );
    let selection =
        resolve_project_choice(None, None, Some(LINUX), root.path(), None).expect("resolve");
    assert_eq!(
        selection.choice,
        LinkerChoice::Reld,
        "the exact triple's bare reld must win over the cfg section's dylint-link"
    );
    assert_eq!(
        selection.reld_cargo_config,
        Some(ReldCargoConfig::BareLinker)
    );
}

/// soldr#3483's other miss: the lint package's config lives beside the lint,
/// not at the workspace root the outer `cargo dylint` runs from. The lint
/// roots are layered ahead of the project root so the declaration is seen
/// even when the workspace itself declares nothing.
#[test]
fn lint_library_roots_contribute_their_cargo_configs() {
    let root = tempfile::tempdir().expect("project root");
    let lint = root.path().join("lints").join("fixture");
    write_cargo_config(&lint, "[target.'cfg(all())']\nlinker = \"dylint-link\"\n");
    let selection = resolve_project_choice_with_lint_roots(
        None,
        None,
        Some(LINUX),
        root.path(),
        None,
        std::slice::from_ref(&lint),
    )
    .expect("resolve");
    assert_eq!(selection.source, LinkerSource::CargoConfig);
    assert_eq!(selection.choice, LinkerChoice::Default);
}

/// An empty lint-root slice is the plain-build contract: the workspace root
/// stays the first place looked at, unchanged from before soldr#3483.
#[test]
fn no_lint_roots_keeps_the_workspace_root_lookup_unchanged() {
    let root = tempfile::tempdir().expect("project root");
    write_cargo_config(
        root.path(),
        "[target.x86_64-unknown-linux-gnu]\nlinker = \"reld\"\n",
    );
    let selection =
        resolve_project_choice_with_lint_roots(None, None, Some(LINUX), root.path(), None, &[])
            .expect("resolve");
    assert_eq!(selection.choice, LinkerChoice::Reld);
    assert_eq!(selection.source, LinkerSource::CargoConfig);
}
