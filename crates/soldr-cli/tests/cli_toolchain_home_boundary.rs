mod common;

use common::*;
use soldr_cli::timed_test;
use std::fs;

timed_test!(
    no_manifest_preserves_host_rustup_home_with_real_tool_overrides,
    {
        let cache_root = unique_temp_dir("cargo-real-tool-host-rustup-home");
        let log_path = cache_root.join("tool.log");
        let tool_dir = unique_temp_dir("cargo-real-tool-host-toolchain");
        let (cargo, rustc, _) = install_fake_version_toolchain(&tool_dir, &log_path);
        let explicit_cargo_home = unique_temp_dir("explicit-host-cargo-home");

        fs::create_dir_all(cache_root.join("cargo"))
            .expect("failed to create soldr-managed cargo home");
        fs::create_dir_all(cache_root.join("rustup"))
            .expect("failed to create soldr-managed rustup home");

        let output = isolated_soldr_command()
            .args(["--no-cache", "cargo", "--version"])
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_REAL_CARGO", &cargo)
            .env("SOLDR_REAL_RUSTC", &rustc)
            .env("CARGO_HOME", &explicit_cargo_home)
            .env("PATH", isolated_test_path())
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .expect("failed to run soldr cargo --version without a toolchain manifest");

        assert!(
            output.status.success(),
            "host-toolchain cargo front door failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).expect("failed to read fake cargo log");
        let cargo_line = log
            .lines()
            .find(|line| line.starts_with("cargo "))
            .unwrap_or_else(|| panic!("real cargo override was not invoked: {log}"));
        assert!(
            path_display_variants(&explicit_cargo_home)
                .iter()
                .any(|path| cargo_line.contains(&format!("cargo_home={path}"))),
            "explicit host CARGO_HOME should reach cargo unchanged: {cargo_line}"
        );
        #[cfg(windows)]
        let rustup_home_is_unset = cargo_line.contains("rustup_home=%RUSTUP_HOME% args=--version");
        #[cfg(not(windows))]
        let rustup_home_is_unset = cargo_line.contains("rustup_home= args=--version");
        assert!(
            rustup_home_is_unset,
            "a no-manifest host toolchain must not inherit Soldr's managed RUSTUP_HOME: {cargo_line}"
        );
    }
);

timed_test!(managed_toolchain_binary_keeps_soldr_toolchain_homes, {
    let cache_root = unique_temp_dir("cargo-real-tool-managed-rustup-home");
    let log_path = cache_root.join("tool.log");
    let managed_cargo_home = cache_root.join("cargo");
    let managed_cargo_bin = managed_cargo_home.join("bin");
    let managed_rustup_home = cache_root.join("rustup");
    fs::create_dir_all(&managed_cargo_bin).expect("failed to create managed cargo bin");
    fs::create_dir_all(&managed_rustup_home).expect("failed to create managed rustup home");
    let (cargo, rustc, _) = install_fake_version_toolchain(&managed_cargo_bin, &log_path);

    let output = isolated_soldr_command()
        .args(["--no-cache", "cargo", "--version"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_REAL_CARGO", &cargo)
        .env("SOLDR_REAL_RUSTC", &rustc)
        .env("PATH", isolated_test_path())
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .expect("failed to run soldr cargo --version with managed tool binaries");

    assert!(
        output.status.success(),
        "managed-toolchain cargo front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake cargo log");
    let cargo_line = log
        .lines()
        .find(|line| line.starts_with("cargo "))
        .unwrap_or_else(|| panic!("managed cargo override was not invoked: {log}"));
    assert!(
        path_display_variants(&managed_cargo_home)
            .iter()
            .any(|path| cargo_line.contains(&format!("cargo_home={path}"))),
        "managed Cargo must receive Soldr's managed CARGO_HOME: {cargo_line}"
    );
    assert!(
        path_display_variants(&managed_rustup_home)
            .iter()
            .any(|path| cargo_line.contains(&format!("rustup_home={path}"))),
        "managed Cargo must receive Soldr's managed RUSTUP_HOME: {cargo_line}"
    );
});

timed_test!(
    cargo_fmt_host_toolchain_does_not_mix_in_managed_rustup_home,
    {
        let cache_root = unique_temp_dir("cargo-fmt-host-rustup-home");
        let log_path = cache_root.join("tool.log");
        let source_path = write_rustfmt_source(&cache_root);
        let (_rustup, cargo, rustc, rustfmt, zccache) =
            install_fake_cargo_fmt_toolchain(&log_path, &source_path);
        let explicit_cargo_home = unique_temp_dir("cargo-fmt-explicit-host-cargo-home");

        fs::create_dir_all(cache_root.join("cargo"))
            .expect("failed to create soldr-managed cargo home");
        fs::create_dir_all(cache_root.join("rustup"))
            .expect("failed to create soldr-managed rustup home");

        let output = isolated_soldr_command()
            .args(["cargo", "fmt"])
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_REAL_CARGO", &cargo)
            .env("SOLDR_REAL_RUSTC", &rustc)
            .env("SOLDR_REAL_RUSTFMT", &rustfmt)
            .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
            .env("CARGO_HOME", &explicit_cargo_home)
            .env("PATH", isolated_test_path())
            .env_remove("RUSTFMT")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("ZCCACHE_CACHE_DIR")
            .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
            .env_remove("ZCCACHE_DISABLE")
            .output()
            .expect("failed to run cargo fmt with a host-owned toolchain");

        assert!(
            output.status.success(),
            "host-toolchain cargo fmt failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).expect("failed to read fake rustfmt log");
        let rustfmt_line = log
            .lines()
            .find(|line| line.starts_with("rustfmt "))
            .unwrap_or_else(|| panic!("cargo fmt did not invoke rustfmt: {log}"));
        #[cfg(windows)]
        let rustup_home_is_unset = rustfmt_line.contains("rustup_home=%RUSTUP_HOME% args=");
        #[cfg(not(windows))]
        let rustup_home_is_unset = rustfmt_line.contains("rustup_home= args=");
        assert!(
            rustup_home_is_unset,
            "host rustfmt must not inherit Soldr's managed RUSTUP_HOME: {rustfmt_line}"
        );
    }
);
