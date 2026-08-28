use crate::common::*;
use std::path::{Path, PathBuf};

fn fake_rust_analyzer_script(log_path: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"rust-analyzer args=$* cache=${{SOLDR_CACHE_ENABLED:-}} child_shims=${{SOLDR_CHILD_SHIMS_ACTIVE:-}} zccache_dir=${{ZCCACHE_CACHE_DIR:-}} path_remap=${{ZCCACHE_PATH_REMAP:-}} worktree_root=${{ZCCACHE_WORKTREE_ROOT:-}}\" >> \"{0}\"\n\
         cargo check\n",
        log_path.display()
    )
}

fn fake_rust_analyzer_nested_cargo_script(log_path: &Path) -> String {
    let output_dir = fake_rustc_output_dir(log_path);
    format!(
        "#!/bin/sh\n\
         echo \"cargo args=$* wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} child_shims=${{SOLDR_CHILD_SHIMS_ACTIVE:-}}\" >> \"{0}\"\n\
         if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
           \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name ra_demo --emit dep-info,link -o \"{1}/ra_demo\" --out-dir \"{1}\"\n\
         else\n\
           \"$RUSTC\" --crate-name ra_demo --emit dep-info,link -o \"{1}/ra_demo\" --out-dir \"{1}\"\n\
         fi\n",
        log_path.display(),
        output_dir.display()
    )
}

fn install_fake_rust_analyzer(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("fake-rust-analyzer");
    let rust_analyzer = fake_script_path(&dir, "rust-analyzer");
    write_fake_script(&rust_analyzer, &fake_rust_analyzer_script(log_path));
    rust_analyzer
}

#[test]
fn rust_analyzer_spawned_cargo_routes_through_child_shims_and_zccache() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("rust-analyzer-zccache-shims");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    write_fake_script(&cargo, &fake_rust_analyzer_nested_cargo_script(&log_path));
    let rust_analyzer = install_fake_rust_analyzer(&log_path);

    let output = isolated_soldr_command()
        .arg("rust-analyzer")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_REAL_RUST_ANALYZER", &rust_analyzer)
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("SOLDR_CACHE_ENABLED")
        .env_remove("SOLDR_CHILD_SHIMS_ACTIVE")
        .env_remove("SOLDR_DISABLE_CHILD_SHIMS")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr rust-analyzer");

    assert!(
        output.status.success(),
        "soldr rust-analyzer failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&log_path).expect("read fake tool log");
    let rust_analyzer_line = log
        .lines()
        .find(|line| line.starts_with("rust-analyzer "))
        .unwrap_or_else(|| panic!("missing rust-analyzer invocation in log: {log}"));
    assert!(
        rust_analyzer_line.contains("cache=1") && rust_analyzer_line.contains("child_shims=1"),
        "rust-analyzer should inherit cache policy and child-shim guard: {log}"
    );

    let cargo_line = log
        .lines()
        .find(|line| line.starts_with("cargo args=check "))
        .unwrap_or_else(|| panic!("missing nested cargo check invocation in log: {log}"));
    assert!(
        cargo_line.contains("wrapper=")
            && !cargo_line.contains("wrapper= rustc=")
            && cargo_line.contains("cache=1")
            && cargo_line.contains("child_shims=1"),
        "nested cargo should re-enter soldr with cache enabled and no extra shim layer: {log}"
    );
    assert!(
        log.lines()
            .any(|line| line.starts_with("rustc ") && line.contains("--crate-name ra_demo")),
        "rust-analyzer-spawned cargo should reach the compiler through Soldr's embedded route: {log}"
    );
}
