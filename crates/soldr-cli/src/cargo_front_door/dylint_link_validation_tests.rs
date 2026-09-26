//! soldr#3274: `dylint-link` is a transparent linker wrapper, so the generic
//! `--version` / `--help` probes a correct binary cannot pass were rejecting
//! Soldr's own managed pair on `x86_64-pc-windows-msvc`. These tests drive
//! `validate_dylint_path_binary` against shims that reproduce MSVC
//! `link.exe`'s response shape.
//!
//! The fixtures are `#!/bin/sh` scripts, so each test returns early on a
//! Windows host. That gate is a **runtime** host check, not `#[cfg(unix)]`:
//! host `cfg` outside `soldr-platform` is denied by the #2493 boundary
//! (`dylints/ban_platform_cfg_outside_boundary` and
//! `.github/scripts/platform_cfg_boundary_ratchet.py`), and this module
//! previously carried `#[cfg(all(test, unix))]` in `mod.rs` — see soldr#3284.
//!
//! The behaviour under test is platform-independent: it is decided entirely by
//! exit status plus captured output, and the predicate itself
//! (`dylint_link_help_output_is_valid`) is unit-tested host-agnostically in
//! `soldr-fetch`.

use super::*;

/// Whether this host can run the `#!/bin/sh` fixtures below.
fn shell_fixtures_supported() -> bool {
    crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows
}

/// Write an executable `#!/bin/sh` shim that emits `payload` on stdout and
/// exits 1, mirroring how a real `dylint-link` surfaces its linker's reply.
fn write_shim(dir: &Path, name: &str, payload: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\ncat <<'EOF'\n{payload}\nEOF\nexit 1\n"),
    )
    .unwrap();
    let source = std::fs::metadata(&path).unwrap().permissions();
    crate::platform::fs::permissions::make_executable_from(&path, &source).unwrap();
    path
}

const MSVC_BANNER: &str = "Microsoft (R) Incremental Linker Version 14.44.35207.1\n\
Copyright (C) Microsoft Corporation.  All rights reserved.\n\n\
   usage: LINK [options] [files] [@commandfile]";

#[test]
fn dylint_link_exiting_one_with_the_msvc_banner_is_accepted() {
    if !shell_fixtures_supported() {
        return;
    }
    // The exact false negative from soldr#3274: a healthy managed
    // dylint-link forwards `/?` to link.exe, which prints its banner and
    // usage and exits non-zero. Rejecting that made `soldr cargo dylint`
    // unrunnable on Windows.
    let dir = tempfile::tempdir().unwrap();
    let shim = write_shim(dir.path(), "dylint-link", MSVC_BANNER);

    validate_dylint_path_binary(&shim, "dylint-link", "6.0.3")
        .expect("a dylint-link that prints the MSVC banner and usage must be accepted");
}

#[test]
fn dylint_link_exiting_one_without_a_banner_is_still_rejected() {
    if !shell_fixtures_supported() {
        return;
    }
    // The #2432 binary-or-exit-1 invariant: a genuinely unusable pair still
    // fails with the actionable diagnostic.
    let dir = tempfile::tempdir().unwrap();
    let shim = write_shim(dir.path(), "dylint-link", "LINK : fatal error LNK1181");

    let error = validate_dylint_path_binary(&shim, "dylint-link", "6.0.3")
        .expect_err("a dylint-link with no linker banner must be rejected");
    let message = error.to_string();
    assert!(message.contains("dylint-link"), "{message}");
    assert!(
        message.contains("Dylint v6.0.3 is not built for this machine"),
        "{message}"
    );
    assert!(message.contains("Corrective action:"), "{message}");
    assert!(
        message.contains(&shim.display().to_string()),
        "the diagnostic must name the rejected binary: {message}"
    );
}

#[test]
fn the_msvc_banner_allowance_does_not_leak_to_cargo_dylint() {
    if !shell_fixtures_supported() {
        return;
    }
    // `cargo-dylint` is an ordinary CLI; it must still be judged by a clean
    // `--version` / `--help` exit, so banner-shaped output buys it nothing.
    let dir = tempfile::tempdir().unwrap();
    let shim = write_shim(dir.path(), "cargo-dylint", MSVC_BANNER);

    assert!(
        validate_dylint_path_binary(&shim, "cargo-dylint", "6.0.3").is_err(),
        "cargo-dylint must not be accepted on a linker banner"
    );
}

#[test]
fn a_healthy_unix_style_dylint_link_still_passes_on_version() {
    if !shell_fixtures_supported() {
        return;
    }
    // Unix linkers answer `--version` cleanly; that fast path must survive.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dylint-link");
    std::fs::write(&path, "#!/bin/sh\necho 'dylint-link 6.0.3'\nexit 0\n").unwrap();
    let source = std::fs::metadata(&path).unwrap().permissions();
    crate::platform::fs::permissions::make_executable_from(&path, &source).unwrap();

    validate_dylint_path_binary(&path, "dylint-link", "6.0.3")
        .expect("a dylint-link that exits 0 on --version must be accepted");
}

/// soldr#3382: a broken PATH `cargo-dylint` used to be probed with both
/// streams nulled, so the error could only say "exited with 1". The error
/// now carries the component's own stderr.
#[test]
fn cargo_dylint_probe_failure_names_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let shim = crate::core::tool_output::write_fake_tool(
        dir.path(),
        "cargo-dylint",
        "",
        "MARKER_DYLINT_3382",
        1,
    );
    let error = validate_dylint_path_binary(&shim, "cargo-dylint", "6.0.3")
        .expect_err("a component exiting 1 must be rejected")
        .to_string();
    assert!(error.contains("MARKER_DYLINT_3382"), "{error}");
    assert!(error.contains("--version exited with"), "{error}");
}
