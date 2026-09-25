//! Unit coverage split from `linker.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;

const LINUX: &str = "x86_64-unknown-linux-gnu";
const LINUX_MUSL: &str = "x86_64-unknown-linux-musl";
const MAC_X64: &str = "x86_64-apple-darwin";
const MAC_ARM: &str = "aarch64-apple-darwin";
const WIN_MSVC: &str = "x86_64-pc-windows-msvc";
const WIN_GNU: &str = "x86_64-pc-windows-gnu";

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
    // since `-fuse-ld` rejects unknown linker names) there; on Windows/macOS it
    // bridges to lld-link/ld64.lld, which handle the CRT, so reld is injected
    // directly on PATH with no extra flags.
    for triple in [LINUX, LINUX_MUSL] {
        let i = resolve_for_target(LinkerChoice::Reld, triple).unwrap();
        assert_eq!(i.linker.as_deref(), Some("clang"), "{triple}");
        assert_eq!(
            i.rustflags.as_deref(),
            Some("-C link-arg=--ld-path=reld"),
            "{triple}"
        );
    }
    for triple in [MAC_X64, MAC_ARM, WIN_MSVC, WIN_GNU] {
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

    materialize_linker_driver_shim(&paths, LINUX, &mut injection).unwrap();

    assert!(injection.rustflags.is_none());
    let path = PathBuf::from(injection.linker.as_deref().unwrap());
    assert!(path.starts_with(paths.bin.join("linker-shims").join("v1")));
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("--ld-path=/managed/reld with space"));

    let first_path = path;
    let mut different = LinkerInjection::clang_with_fuse("lld");
    materialize_linker_driver_shim(&paths, LINUX, &mut different).unwrap();
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
    materialize_linker_driver_shim(&paths, LINUX, &mut injection).unwrap();
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
    let body = render_windows_linker_driver_shim("--ld-path=C:\\A B\\100% & tools\\reld.exe");
    assert_eq!(
        body,
        "@echo off\r\nclang \"--ld-path=C:\\A B\\100%% & tools\\reld.exe\" %*\r\n"
    );
}

#[test]
fn fast_on_apple_uses_reld() {
    for triple in [MAC_X64, MAC_ARM] {
        let i = resolve_for_target_with_probe(LinkerChoice::Fast, triple, &|| true).unwrap();
        assert_eq!(i.linker.as_deref(), Some("reld"), "{triple}");
        assert!(i.rustflags.is_none(), "{triple}");
    }
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
fn project_target_config_does_not_match_cfg_sections() {
    // Known soldr#3277 limitation: cfg-spec target sections (e.g.
    // `[target.'cfg(all())']`) are not detected by
    // `target_config_value_in_root`, which only matches an exact triple key.
    // This repo's `dylints/*` manifests rely on `SOLDR_LINKER=default` to
    // opt out of injection rather than a cfg-spec `[target]` section.
    let root = tempfile::tempdir().expect("temporary project root");
    std::fs::create_dir_all(root.path().join(".cargo")).expect("create .cargo dir");
    std::fs::write(
        root.path().join(".cargo/config.toml"),
        "[target.'cfg(all())']\nrustflags = [\"-C\", \"linker=dylint-link\"]\n",
    )
    .expect("write .cargo/config.toml");
    assert!(target_config_value_in_root(root.path(), LINUX, "rustflags").is_none());
}
