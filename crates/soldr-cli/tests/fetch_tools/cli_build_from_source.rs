use crate::common::*;
use std::path::{Path, PathBuf};

fn fake_build_from_source_cargo_script(log_path: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} workspace_wrapper=${{RUSTC_WORKSPACE_WRAPPER:-}} args=$*\" >> \"{0}\"\n\
         root=\"\"\n\
         tool=\"\"\n\
         while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             install)\n\
               shift\n\
               tool=\"${{1%%@*}}\"\n\
               ;;\n\
             --root)\n\
               shift\n\
               root=\"$1\"\n\
               ;;\n\
           esac\n\
           shift || true\n\
         done\n\
         if [ -z \"$root\" ] || [ -z \"$tool\" ]; then\n\
           echo \"missing --root or tool\" >&2\n\
           exit 2\n\
         fi\n\
         mkdir -p \"$root/bin\"\n\
         printf '#!/bin/sh\\nexit 0\\n' > \"$root/bin/$tool\"\n\
         chmod +x \"$root/bin/$tool\"\n",
        log_path.display()
    )
}

fn install_fake_build_from_source_cargo(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("fake-build-from-source-cargo");
    let cargo = fake_script_path(&dir, "cargo");
    write_fake_script(&cargo, &fake_build_from_source_cargo_script(log_path));
    cargo
}

#[test]
fn build_from_source_replaces_inherited_rustc_wrappers() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("build-from-source-wrapper-policy");
    let log_path = cache_root.join("cargo.log");
    let cargo = install_fake_build_from_source_cargo(&log_path);

    let output = isolated_soldr_command()
        .args([
            "build-from-source",
            "crgx",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--version",
            "1.2.3",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("RUSTC_WRAPPER", "/tmp/outer-wrapper")
        .env("RUSTC_WORKSPACE_WRAPPER", "/tmp/outer-workspace-wrapper")
        .output()
        .expect("failed to run soldr build-from-source");

    assert!(
        output.status.success(),
        "soldr build-from-source failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&log_path).expect("read fake cargo log");
    // The inherited wrappers must never reach the inner cargo. Propagating
    // them would route the source build back through soldr's own front door
    // and re-enter the cache logic recursively.
    assert!(
        !log.contains("/tmp/outer-wrapper") && !log.contains("/tmp/outer-workspace-wrapper"),
        "build-from-source must not propagate inherited rustc wrappers: {log}"
    );
    // RUSTC_WORKSPACE_WRAPPER stays cleared. RUSTC_WRAPPER is deliberately
    // re-pointed at soldr's own compiler-named zccache shim (issue #1788) so
    // tool source builds hit the shared object cache instead of recompiling
    // every dependency cold — see `apply_source_build_cache_wrapper`. The
    // kill-switch case below pins the fully-uncached spawn.
    assert!(
        log.contains("workspace_wrapper= args=install crgx@1.2.3"),
        "build-from-source cargo install should clear the workspace wrapper: {log}"
    );
    let installed = cache_root
        .join("bin")
        .join("crgx-from-source")
        .join("1.2.3")
        .join("x86_64-unknown-linux-gnu")
        .join("crgx");
    assert!(
        installed.is_file(),
        "expected source-built binary at {}",
        installed.display()
    );
}

// Issue #1788 routes source-build rustc through soldr's zccache shim by
// default. `SOLDR_SOURCE_BUILD_CACHE=off` is the documented escape hatch back
// to the historical fully-uncached spawn, so pin it: without this, a
// regression that ignores the kill switch would go unnoticed.
#[test]
fn build_from_source_cache_kill_switch_scrubs_every_wrapper() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("build-from-source-wrapper-killswitch");
    let log_path = cache_root.join("cargo.log");
    let cargo = install_fake_build_from_source_cargo(&log_path);

    let output = isolated_soldr_command()
        .args([
            "build-from-source",
            "crgx",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--version",
            "1.2.3",
        ])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_SOURCE_BUILD_CACHE", "off")
        .env("RUSTC_WRAPPER", "/tmp/outer-wrapper")
        .env("RUSTC_WORKSPACE_WRAPPER", "/tmp/outer-workspace-wrapper")
        .output()
        .expect("failed to run soldr build-from-source");

    assert!(
        output.status.success(),
        "soldr build-from-source failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&log_path).expect("read fake cargo log");
    assert!(
        log.contains("cargo wrapper= workspace_wrapper= args=install crgx@1.2.3"),
        "kill switch must restore the fully-uncached spawn: {log}"
    );
}
