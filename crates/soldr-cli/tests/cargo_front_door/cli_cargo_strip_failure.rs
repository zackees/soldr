//! The compiler only warns when a requested strip subprocess fails. Soldr must
//! turn that otherwise-zero Cargo invocation into an actionable failure.

use crate::common::*;
use std::fs;

fn broken_rust_objcopy_cargo_script() -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\n\
         echo warning: stripping debug info with `rust-objcopy` failed: exit status: 127 1>&2\n\
         echo = note: rust-objcopy: error while loading shared libraries: libLLVM.so.21.1-rust-1.94.1-stable: cannot open shared object file 1>&2\n\
         exit /b 0\n"
            .to_owned()
    } else {
        "#!/bin/sh\n\
         printf '%s\\n' 'warning: stripping debug info with `rust-objcopy` failed: exit status: 127' >&2\n\
         printf '%s\\n' '= note: rust-objcopy: error while loading shared libraries: libLLVM.so.21.1-rust-1.94.1-stable: cannot open shared object file' >&2\n\
         exit 0\n"
            .to_owned()
    }
}

#[test]
fn requested_strip_failure_overrides_cargos_success_exit_code() {
    let root = unique_temp_dir("cargo-strip-failure");
    let workspace = root.join("workspace");
    let tool_dir = root.join("tool");
    let cache_root = root.join("cache");
    fs::create_dir_all(workspace.join("src")).expect("workspace source directory");
    fs::create_dir_all(&tool_dir).expect("tool directory");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"strip_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn fixture() {}\n").expect("source");

    let cargo = fake_script_path(&tool_dir, "cargo");
    write_fake_script(&cargo, &broken_rust_objcopy_cargo_script());
    let output = isolated_soldr_command()
        .args(["--no-cache", "cargo", "build", "--release"])
        .current_dir(&workspace)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("ZCCACHE_DISABLE", "1")
        .output()
        .expect("run fake strip-failure cargo");

    assert!(
        !output.status.success(),
        "a warning-only strip failure must fail soldr cargo build\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requested artifact stripping with `rust-objcopy` failed"));
    assert!(stderr.contains("libLLVM.so.21.1-rust-1.94.1-stable"));
}
