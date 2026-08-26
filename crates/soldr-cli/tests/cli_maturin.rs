mod common;

use common::*;
use soldr_cli::fetch::MANAGED_MATURIN_VERSION;
use std::path::{Path, PathBuf};

fn fake_maturin_script(log_path: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         echo \"maturin args=$* cargo=${{CARGO:-}} rustc=${{RUSTC:-}} wrapper=${{RUSTC_WRAPPER:-}} cache=${{SOLDR_CACHE_ENABLED:-}} path_remap=${{ZCCACHE_PATH_REMAP:-}} worktree_root=${{ZCCACHE_WORKTREE_ROOT:-}}\" >> \"{0}\"\n\
         if [ \"${{1:-}}\" = \"--version\" ]; then\n\
           echo \"maturin {1}\"\n\
           exit 0\n\
         fi\n\
         \"${{CARGO:-cargo}}\" build\n",
        log_path.display(),
        MANAGED_MATURIN_VERSION
    )
}

fn seed_cached_fake_maturin(cache_root: &Path, log_path: &Path) -> PathBuf {
    let dir = cache_root
        .join("bin")
        .join(format!("maturin-{MANAGED_MATURIN_VERSION}"));
    std::fs::create_dir_all(&dir).expect("create fake maturin cache dir");
    let maturin = dir.join("maturin");
    write_fake_script(&maturin, &fake_maturin_script(log_path));
    maturin
}

fn line_contains_path(line: &str, prefix: &str, path: &Path) -> bool {
    path_display_variants(path)
        .iter()
        .any(|candidate| line.contains(&format!("{prefix}{candidate}")))
}

#[test]
fn direct_maturin_build_routes_nested_cargo_through_zccache() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("direct-maturin-zccache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, _zccache) = install_fake_toolchain(&log_path);
    seed_cached_fake_maturin(&cache_root, &log_path);

    let output = isolated_soldr_command()
        .args(["maturin", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("SOLDR_RUSTC_WRAPPER")
        .env_remove("SOLDR_CACHE_ENABLED")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr maturin build");

    assert!(
        output.status.success(),
        "soldr maturin build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&log_path).expect("read fake tool log");
    let maturin_line = log
        .lines()
        .find(|line| line.starts_with("maturin args=build "))
        .unwrap_or_else(|| panic!("missing maturin invocation in log: {log}"));
    assert!(
        line_contains_path(maturin_line, "cargo=", &cargo),
        "direct maturin should receive soldr-resolved CARGO: {log}"
    );
    assert!(
        line_contains_path(maturin_line, "rustc=", &rustc),
        "direct maturin should receive soldr-resolved RUSTC: {log}"
    );
    assert!(
        maturin_line.contains("wrapper=") && !maturin_line.contains("wrapper= cache="),
        "direct maturin should receive an auto-injected RUSTC_WRAPPER: {log}"
    );
    assert!(
        maturin_line.contains("cache=1"),
        "direct maturin should mark the child build cache-enabled: {log}"
    );
    assert!(
        log.lines()
            .any(|line| line.contains("zccache wrapper") && line.contains("demo")),
        "nested maturin cargo rustc call should route through zccache: {log}"
    );
}

#[test]
fn direct_maturin_preserves_explicit_cargo_rustc_and_wrapper() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let cache_root = unique_temp_dir("direct-maturin-explicit-env");
    let log_path = cache_root.join("tool.log");
    let (caller_cargo, caller_rustc, _) = install_fake_toolchain(&log_path);
    let wrapper_dir = unique_temp_dir("direct-maturin-custom-wrapper");
    let custom_wrapper = fake_script_path(&wrapper_dir, "caller-wrapper");
    write_fake_script(
        &custom_wrapper,
        &fake_custom_wrapper_script(&log_path, "caller"),
    );
    seed_cached_fake_maturin(&cache_root, &log_path);

    let output = isolated_soldr_command()
        .args(["maturin", "develop"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTC_BIN", &caller_rustc)
        .env("CARGO", &caller_cargo)
        .env("RUSTC", &caller_rustc)
        .env("RUSTC_WRAPPER", &custom_wrapper)
        .env_remove("SOLDR_RUSTC_WRAPPER")
        .env_remove("SOLDR_CACHE_ENABLED")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr maturin develop");

    assert!(
        output.status.success(),
        "soldr maturin develop failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = std::fs::read_to_string(&log_path).expect("read fake tool log");
    let maturin_line = log
        .lines()
        .find(|line| line.starts_with("maturin args=develop "))
        .unwrap_or_else(|| panic!("missing maturin invocation in log: {log}"));
    assert!(
        line_contains_path(maturin_line, "cargo=", &caller_cargo),
        "caller-provided CARGO must win: {log}"
    );
    assert!(
        line_contains_path(maturin_line, "rustc=", &caller_rustc),
        "caller-provided RUSTC must win: {log}"
    );
    assert!(
        line_contains_path(maturin_line, "wrapper=", &custom_wrapper),
        "caller-provided RUSTC_WRAPPER must win: {log}"
    );
    assert!(
        log.lines()
            .any(|line| line.contains("caller wrapper") && line.contains("demo")),
        "nested cargo should use the caller wrapper: {log}"
    );
    assert!(
        !log.contains("zccache wrapper"),
        "soldr must not replace a caller-provided RUSTC_WRAPPER with zccache: {log}"
    );
}
