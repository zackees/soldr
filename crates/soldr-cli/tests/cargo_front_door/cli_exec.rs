use crate::common::*;
use std::path::Path;

fn fake_exec_nested_cargo_script(log_path: &Path) -> String {
    let output_dir = fake_rustc_output_dir(log_path);
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo cargo wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% child_shims=%SOLDR_CHILD_SHIMS_ACTIVE%>>\"{0}\"\n\
             if defined RUSTC_WRAPPER (\n\
               call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name exec_demo --emit dep-info,link -o \"{1}\\exec_demo\" --out-dir \"{1}\"\n\
             ) else (\n\
               call \"%RUSTC%\" --crate-name exec_demo --emit dep-info,link -o \"{1}\\exec_demo\" --out-dir \"{1}\"\n\
             )\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display(),
            output_dir.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} child_shims=${{SOLDR_CHILD_SHIMS_ACTIVE:-}}\" >> \"{0}\"\n\
             if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
               \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name exec_demo --emit dep-info,link -o \"{1}/exec_demo\" --out-dir \"{1}\"\n\
             else\n\
               \"$RUSTC\" --crate-name exec_demo --emit dep-info,link -o \"{1}/exec_demo\" --out-dir \"{1}\"\n\
             fi\n",
            log_path.display(),
            output_dir.display()
        )
    }
}

fn fake_direct_rustup_cargo_script(log_path: &Path) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo direct rustup cargo %*>>\"{0}\"\n\
             exit /b 66\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"direct rustup cargo $*\" >> \"{0}\"\n\
             exit 66\n",
            log_path.display()
        )
    }
}

#[test]
fn exec_cargo_build_routes_through_child_shims() {
    let cache_root = unique_temp_dir("exec-cargo-zccache-shims");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    write_fake_script(&cargo, &fake_exec_nested_cargo_script(&log_path));

    let cargo_home = cache_root.join("cargo-home");
    let cargo_home_bin = cargo_home.join("bin");
    std::fs::create_dir_all(&cargo_home_bin).expect("create fake cargo home bin");
    let direct_cargo = fake_script_path(&cargo_home_bin, "cargo");
    write_fake_script(&direct_cargo, &fake_direct_rustup_cargo_script(&log_path));

    let output = isolated_soldr_command()
        .args(["exec", "cargo", "build"])
        .env("CARGO_HOME", &cargo_home)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("SOLDR_CHILD_SHIMS_ACTIVE")
        .env_remove("SOLDR_DISABLE_CHILD_SHIMS")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr exec cargo build");

    assert!(
        output.status.success(),
        "soldr exec cargo build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("soldr exec: PATH prefix ") && stderr.contains("soldr-shims-"),
        "soldr exec must install its transient child-shim PATH layer: {stderr}"
    );

    let log = std::fs::read_to_string(&log_path).expect("read fake tool log");
    assert!(
        !log.contains("direct rustup cargo"),
        "exec should resolve cargo through the soldr child shim, not direct rustup cargo: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("cargo wrapper=")),
        "nested cargo must execute after child-shim installation: {log}"
    );
    assert!(
        log.lines()
            .any(|line| line.starts_with("rustc ") && line.contains("exec_demo")),
        "the child shim must forward nested cargo's compiler invocation: {log}"
    );
}
