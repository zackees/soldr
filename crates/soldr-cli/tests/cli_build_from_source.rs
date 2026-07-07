#![cfg(not(windows))]

mod common;

use common::*;
use soldr_cli::timed_test;
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

timed_test!(build_from_source_clears_inherited_rustc_wrappers, {
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
    assert!(
        log.contains("cargo wrapper= workspace_wrapper= args=install crgx@1.2.3"),
        "build-from-source cargo install should scrub rustc wrapper env: {log}"
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
});
