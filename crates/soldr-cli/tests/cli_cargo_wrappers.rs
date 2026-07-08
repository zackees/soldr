#![allow(unused_imports)]

mod common;

use common::*;
use serde_json::Value;
use soldr_cli::timed_test;
use std::io::Write;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn fake_cargo_fmt_script(log_path: &Path, source_path: &Path, rustfmt: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             set \"fmt=%RUSTFMT%\"\n\
             if not defined RUSTFMT set \"fmt={2}\"\n\
             echo cargo fmt rustfmt=%fmt% env_rustfmt=%RUSTFMT% cache=%SOLDR_CACHE_ENABLED%>>\"{0}\"\n\
             if \"%~1\"==\"fmt\" (\n\
               call \"%fmt%\" \"{1}\"\n\
               exit /b %ERRORLEVEL%\n\
             )\n\
             echo unsupported fake cargo fmt invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display(),
            source_path.display(),
            rustfmt.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             fmt=\"${{RUSTFMT:-{2}}}\"\n\
             echo \"cargo fmt rustfmt=$fmt env_rustfmt=${{RUSTFMT:-}} cache=${{SOLDR_CACHE_ENABLED:-}}\" >> \"{0}\"\n\
             if [ \"$1\" = \"fmt\" ]; then\n\
               \"$fmt\" \"{1}\"\n\
               exit $?\n\
             fi\n\
             echo \"unsupported fake cargo fmt invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display(),
            source_path.display(),
            rustfmt.display()
        )
    }
}

fn fake_cargo_doc_script(log_path: &Path, source_path: &Path, rustdoc: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             set \"rustdoc=%RUSTDOC%\"\n\
             if not defined RUSTDOC set \"rustdoc={2}\"\n\
             echo cargo %* wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% rustdoc=%rustdoc% env_rustdoc=%RUSTDOC% cache=%SOLDR_CACHE_ENABLED%>>\"{0}\"\n\
             if \"%~1\"==\"doc\" (\n\
               if defined RUSTC_WRAPPER (\n\
                 call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name doc_demo --emit dep-info,link \"{1}\"\n\
                 if errorlevel 1 exit /b 1\n\
               ) else (\n\
                 call \"%RUSTC%\" --crate-name doc_demo --emit dep-info,link \"{1}\"\n\
                 if errorlevel 1 exit /b 1\n\
               )\n\
               call \"%rustdoc%\" \"{1}\"\n\
               exit /b\n\
             )\n\
             if \"%~1\"==\"test\" if \"%~2\"==\"--doc\" (\n\
               if defined RUSTC_WRAPPER (\n\
                 call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name doctest_demo --emit dep-info,link \"{1}\"\n\
                 if errorlevel 1 exit /b 1\n\
               ) else (\n\
                 call \"%RUSTC%\" --crate-name doctest_demo --emit dep-info,link \"{1}\"\n\
                 if errorlevel 1 exit /b 1\n\
               )\n\
               call \"%rustdoc%\" \"{1}\"\n\
               exit /b\n\
             )\n\
             echo unsupported fake cargo doc invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display(),
            source_path.display(),
            rustdoc.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             rustdoc=\"${{RUSTDOC:-{2}}}\"\n\
             echo \"cargo $* wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} rustdoc=$rustdoc env_rustdoc=${{RUSTDOC:-}} cache=${{SOLDR_CACHE_ENABLED:-}}\" >> \"{0}\"\n\
             run_doc_compile() {{\n\
               crate_name=\"$1\"\n\
               if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
                 \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name \"$crate_name\" --emit dep-info,link \"{1}\" || exit $?\n\
               else\n\
                 \"$RUSTC\" --crate-name \"$crate_name\" --emit dep-info,link \"{1}\" || exit $?\n\
               fi\n\
               \"$rustdoc\" \"{1}\"\n\
             }}\n\
             if [ \"$1\" = \"doc\" ]; then\n\
               run_doc_compile doc_demo\n\
               exit $?\n\
             fi\n\
             if [ \"$1\" = \"test\" ] && [ \"${{2:-}}\" = \"--doc\" ]; then\n\
               run_doc_compile doctest_demo\n\
               exit $?\n\
             fi\n\
             echo \"unsupported fake cargo doc invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display(),
            source_path.display(),
            rustdoc.display()
        )
    }
}

fn fake_cargo_miri_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             echo cargo miri wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% session=%ZCCACHE_SESSION_ID%>>\"{0}\"\n\
             if \"%~1\"==\"miri\" (\n\
               if defined RUSTC_WRAPPER (\n\
                 call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name miri_demo --emit metadata,link\n\
               ) else (\n\
                 call \"%RUSTC%\" --crate-name miri_demo --emit metadata,link\n\
               )\n\
               exit /b %ERRORLEVEL%\n\
             )\n\
             echo unsupported fake cargo miri invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             echo \"cargo miri wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}}\" >> \"{0}\"\n\
             if [ \"$1\" = \"miri\" ]; then\n\
               if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
                 \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name miri_demo --emit metadata,link\n\
               else\n\
                 \"$RUSTC\" --crate-name miri_demo --emit metadata,link\n\
               fi\n\
               exit $?\n\
             fi\n\
             echo \"unsupported fake cargo miri invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display()
        )
    }
}

fn install_fake_cargo_fmt_toolchain(
    log_path: &Path,
    source_path: &Path,
) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (rustup, cargo, rustc, rustfmt) = install_fake_rustup_toolchain(log_path);
    let tool_dir = cargo
        .parent()
        .expect("fake cargo should live in a tool dir")
        .to_path_buf();
    let zccache = fake_script_path(&tool_dir, "zccache");
    write_fake_script(
        &cargo,
        &fake_cargo_fmt_script(log_path, source_path, &rustfmt),
    );
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (rustup, cargo, rustc, rustfmt, zccache)
}

fn install_fake_cargo_doc_toolchain(
    log_path: &Path,
    source_path: &Path,
) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (rustup, cargo, rustc, _rustfmt) = install_fake_rustup_toolchain(log_path);
    let tool_dir = cargo
        .parent()
        .expect("fake cargo should live in a tool dir")
        .to_path_buf();
    let rustdoc = fake_script_path(&tool_dir, "rustdoc");
    let zccache = fake_script_path(&tool_dir, "zccache");
    write_fake_script(
        &cargo,
        &fake_cargo_doc_script(log_path, source_path, &rustdoc),
    );
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (rustup, cargo, rustc, rustdoc, zccache)
}

fn install_fake_cargo_miri_toolchain(log_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (cargo, rustc, zccache) = install_fake_toolchain(log_path);
    write_fake_script(&cargo, &fake_cargo_miri_script(log_path));
    (cargo, rustc, zccache)
}

fn install_fake_direct_rustc_like_toolchain(
    log_path: &Path,
) -> (PathBuf, PathBuf, PathBuf, PathBuf, PathBuf) {
    let (rustup, _cargo, rustc, _rustfmt) = install_fake_rustup_toolchain(log_path);
    let tool_dir = rustc
        .parent()
        .expect("fake rustc should live in a tool dir")
        .to_path_buf();
    let clippy_driver = fake_script_path(&tool_dir, "clippy-driver");
    let zccache = fake_script_path(&tool_dir, "zccache");
    write_fake_script(
        &clippy_driver,
        &fake_version_tool_script(log_path, "clippy-driver"),
    );
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (rustup, rustc, clippy_driver, zccache, tool_dir)
}

fn write_rustfmt_source(cache_root: &Path) -> PathBuf {
    let src_dir = cache_root.join("src");
    fs::create_dir_all(&src_dir).expect("failed to create rustfmt source dir");
    let source_path = src_dir.join("lib.rs");
    fs::write(&source_path, "fn main( ) {}\n").expect("failed to write rustfmt source");
    source_path
}

fn write_rustc_like_source(cache_root: &Path) -> PathBuf {
    let src_dir = cache_root.join("src");
    fs::create_dir_all(&src_dir).expect("failed to create rustc-like source dir");
    let source_path = src_dir.join("lib.rs");
    fs::write(&source_path, "pub fn value() -> usize { 42 }\n")
        .expect("failed to write rustc-like source");
    source_path
}

fn write_rustdoc_source(cache_root: &Path) -> PathBuf {
    let src_dir = cache_root.join("src");
    fs::create_dir_all(&src_dir).expect("failed to create rustdoc source dir");
    let source_path = src_dir.join("lib.rs");
    fs::write(
        &source_path,
        "/// Adds two numbers.\npub fn add(left: usize, right: usize) -> usize { left + right }\n",
    )
    .expect("failed to write rustdoc source");
    source_path
}

fn log_contains_cache_dir(log: &str, cache_root: &Path) -> bool {
    let expected = cache_root.join("cache").join("zccache");
    path_display_variants(&expected)
        .iter()
        .any(|path| log.contains(&format!("cache_dir={path}")))
}

fn expected_link_shim_path(dir: &Path, tool: &str) -> PathBuf {
    #[cfg(windows)]
    {
        dir.join(format!("{tool}.cmd"))
    }
    #[cfg(not(windows))]
    {
        dir.join(tool)
    }
}

fn assert_zccache_wrapped_rustc_compile(log: &str, rustc: &Path, crate_name: &str) {
    let zccache_line = log
        .lines()
        .find(|line| line.contains("zccache wrapper") && line.contains(crate_name))
        .unwrap_or_else(|| {
            panic!("expected zccache wrapper line for rustc crate {crate_name}: {log}")
        });
    assert!(
        path_display_variants(rustc)
            .iter()
            .any(|path| zccache_line.contains(path)),
        "zccache wrapper should receive rustc for crate {crate_name}: {log}"
    );
}

fn assert_zccache_wrapped_compiler(log: &str, compiler: &Path, crate_name: &str) {
    let zccache_line = log
        .lines()
        .find(|line| line.contains("zccache wrapper") && line.contains(crate_name))
        .unwrap_or_else(|| {
            panic!("expected zccache wrapper line for compiler crate {crate_name}: {log}")
        });
    let compiler_name = compiler
        .file_name()
        .and_then(|name| name.to_str())
        .expect("fake compiler should have a utf-8 file name");
    let compiler_stem = compiler
        .file_stem()
        .and_then(|name| name.to_str())
        .expect("fake compiler should have a utf-8 file stem");
    assert!(
        path_display_variants(compiler)
            .iter()
            .any(|path| zccache_line.contains(path))
            || zccache_line
                .split_whitespace()
                .any(|word| word == compiler_name || word == compiler_stem),
        "zccache wrapper should receive compiler for crate {crate_name}: {log}"
    );
}

fn assert_zccache_did_not_wrap_rustdoc(log: &str, rustdoc: &Path) {
    let rustdoc_paths = path_display_variants(rustdoc);
    assert!(
        !log.lines().any(|line| {
            line.contains("zccache wrapper") && rustdoc_paths.iter().any(|path| line.contains(path))
        }),
        "rustdoc should not be routed through zccache: {log}"
    );
}

#[test]
fn cargo_front_door_uses_real_tool_overrides_before_path_probe() {
    let cache_root = unique_temp_dir("cargo-real-tool-overrides");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let shim_dir = unique_temp_dir("cargo-shim-dir");
    let shim_cargo = fake_script_path(&shim_dir, "cargo");
    write_fake_script(
        &shim_cargo,
        &fake_version_tool_script(&log_path, "shim-cargo"),
    );

    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_REAL_CARGO", &cargo)
        .env("SOLDR_REAL_RUSTC", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("PATH", prepend_to_path(&shim_dir))
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with real tool overrides");

    assert!(
        output.status.success(),
        "real-tool override front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo wrapper="),
        "real cargo should have been invoked: {log}"
    );
    assert!(
        !log.contains("shim-cargo"),
        "PATH shim should not be resolved when SOLDR_REAL_CARGO is set: {log}"
    );
}

#[test]
fn cargo_front_door_keeps_cache_enabled_for_non_build_subcommands() {
    let cache_root = unique_temp_dir("cargo-non-build-no-cache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command()
        .args(["cargo", "metadata"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cargo metadata with fake tools");

    assert!(
        output.status.success(),
        "non-build cargo front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    // soldr#1368: the managed session subprocess is gone; the front door
    // just keeps the compile cache enabled for unmodeled subcommands.
    assert!(
        log.contains("cache=1"),
        "cargo front door should keep cache enabled for unmodeled subcommands: {log}"
    );
}

#[test]
fn cargo_front_door_detects_build_after_global_cargo_options() {
    let cache_root = unique_temp_dir("cargo-global-options-cache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command()
        .args(["cargo", "--manifest-path", "demo/Cargo.toml", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with global cargo options");

    assert!(
        output.status.success(),
        "global-option cargo front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    // soldr#1368: rustc compiles route to the soldr-daemon embedded
    // service via RUSTC_WRAPPER=soldr, not a managed `zccache start`.
    assert!(
        log.contains("cache=1") && log.contains("cargo wrapper="),
        "build after global cargo options should still enable caching + wrap rustc: {log}"
    );
}

timed_test!(cargo_miri_keeps_inner_rustc_wrapped_by_policy, {
    let cache_root = unique_temp_dir("cargo-miri-zccache-policy");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_cargo_miri_toolchain(&log_path);
    let output = isolated_soldr_command()
        .args(["cargo", "miri"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo miri with fake tools");

    assert!(
        output.status.success(),
        "cargo miri policy route failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo miri wrapper=") && log.contains("cache=1"),
        "cargo miri should keep cache env and wrapper available: {log}"
    );
    assert_zccache_wrapped_rustc_compile(&log, &rustc, "miri_demo");
});

timed_test!(
    cargo_clippy_routes_workspace_clippy_driver_through_zccache,
    {
        let cache_root = unique_temp_dir("cargo-clippy-clippy-driver-zccache");
        let log_path = cache_root.join("tool.log");
        let (cargo, rustc, zccache, clippy_driver) = install_fake_clippy_toolchain(&log_path);
        let output = isolated_soldr_command()
            .args(["cargo", "clippy"])
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
            .env_remove("SOLDR_TARGET_CACHE_MODE")
            .env_remove("SOLDR_BUILD_CACHE_MODE")
            .output()
            .expect("failed to run soldr cargo clippy with fake tools");

        assert!(
            output.status.success(),
            "cargo clippy front door failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
        assert!(
            log.contains("cargo wrapper=") && log.contains("workspace_wrapper="),
            "fake cargo should model Cargo's nested workspace wrapper: {log}"
        );
        let zccache_line = log
            .lines()
            .find(|line| line.contains("zccache wrapper"))
            .expect("clippy-driver workspace wrapper should be routed through zccache");
        assert!(
            path_display_variants(&clippy_driver)
                .iter()
                .any(|path| zccache_line.contains(path)),
            "zccache should receive clippy-driver as the wrapped compiler: {log}"
        );
        assert!(
            log.contains("clippy-driver") && log.contains("--crate-name demo"),
            "clippy-driver should still receive the rustc compile args: {log}"
        );
    }
);

#[test]
fn direct_rustc_like_commands_route_through_zccache_with_and_without_global_flags() {
    for tool in ["rustc", "clippy-driver"] {
        for (label, prefix_args) in [
            ("plain", vec![tool]),
            ("global-zccache", vec!["--zccache", "system", tool]),
        ] {
            let cache_root = unique_temp_dir(&format!("direct-{tool}-{label}-zccache"));
            let log_path = cache_root.join("tool.log");
            let source_path = write_rustc_like_source(&cache_root);
            let (rustup, rustc, clippy_driver, zccache, tool_dir) =
                install_fake_direct_rustc_like_toolchain(&log_path);
            let compiler = if tool == "rustc" {
                rustc.as_path()
            } else {
                clippy_driver.as_path()
            };

            let mut args = prefix_args;
            args.extend(["--crate-name", "direct_demo", "--emit", "metadata,link"]);
            let mut command = isolated_soldr_command();
            command.args(args).arg(&source_path);
            let output = command
                .current_dir(&cache_root)
                .env("SOLDR_CACHE_DIR", &cache_root)
                .env("SOLDR_TEST_RUSTC_BIN", &rustc)
                .env("SOLDR_REAL_CLIPPY_DRIVER", &clippy_driver)
                .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
                .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
                .env("PATH", prepend_to_path(&tool_dir))
                .env_remove("CARGO_HOME")
                .env_remove("RUSTUP_HOME")
                .env_remove("RUSTUP_TOOLCHAIN")
                .env_remove("ZCCACHE_CACHE_DIR")
                .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
                .env_remove("SOLDR_CACHE_ENABLED")
                .env_remove("ZCCACHE_DISABLE")
                .output()
                .unwrap_or_else(|_| panic!("failed to run direct {tool} route {label}"));

            assert!(
                output.status.success(),
                "direct {tool} route {label} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
            assert_zccache_wrapped_compiler(&log, compiler, "direct_demo");
            assert!(
                log.lines().any(|line| line.starts_with(tool)),
                "direct {tool} should still invoke the real compiler after zccache: {log}"
            );
        }
    }
}

#[test]
fn direct_rustc_like_cache_disable_modes_remain_direct() {
    for tool in ["rustc", "clippy-driver"] {
        for (label, prefix_args, zccache_disable) in [
            (
                "no-cache-global",
                vec!["--no-cache", "--zccache", "system", tool],
                false,
            ),
            ("zccache-disable", vec!["--zccache", "system", tool], true),
        ] {
            let cache_root = unique_temp_dir(&format!("direct-{tool}-{label}-direct"));
            let log_path = cache_root.join("tool.log");
            let source_path = write_rustc_like_source(&cache_root);
            let (rustup, rustc, clippy_driver, zccache, tool_dir) =
                install_fake_direct_rustc_like_toolchain(&log_path);

            let mut args = prefix_args;
            args.extend(["--crate-name", "direct_bypass", "--emit", "metadata,link"]);
            let mut command = isolated_soldr_command();
            command.args(args).arg(&source_path);
            command
                .current_dir(&cache_root)
                .env("SOLDR_CACHE_DIR", &cache_root)
                .env("SOLDR_TEST_RUSTC_BIN", &rustc)
                .env("SOLDR_REAL_CLIPPY_DRIVER", &clippy_driver)
                .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
                .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
                .env("PATH", prepend_to_path(&tool_dir))
                .env_remove("CARGO_HOME")
                .env_remove("RUSTUP_HOME")
                .env_remove("RUSTUP_TOOLCHAIN")
                .env_remove("ZCCACHE_CACHE_DIR")
                .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR");
            if zccache_disable {
                command.env("ZCCACHE_DISABLE", "1");
            } else {
                command.env_remove("ZCCACHE_DISABLE");
            }

            let output = command.output().unwrap_or_else(|_| {
                panic!("failed to run direct {tool} cache-disable route {label}")
            });

            assert!(
                output.status.success(),
                "direct {tool} cache-disable route {label} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );

            let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
            assert!(
                !log.contains("zccache wrapper"),
                "direct {tool} cache-disable route {label} should not use zccache: {log}"
            );
            assert!(
                log.lines().any(|line| line.starts_with(tool)),
                "direct {tool} cache-disable route {label} should invoke compiler directly: {log}"
            );
        }
    }
}

#[test]
fn direct_rustc_non_cacheable_print_probe_stays_direct() {
    for (label, args) in [
        ("plain", vec!["rustc", "--print", "cfg"]),
        (
            "global-zccache",
            vec!["--zccache", "system", "rustc", "--print", "cfg"],
        ),
    ] {
        let cache_root = unique_temp_dir(&format!("direct-rustc-print-{label}"));
        let log_path = cache_root.join("tool.log");
        let (rustup, rustc, clippy_driver, zccache, tool_dir) =
            install_fake_direct_rustc_like_toolchain(&log_path);

        let output = isolated_soldr_command()
            .args(args)
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("SOLDR_REAL_CLIPPY_DRIVER", &clippy_driver)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
            .env("PATH", prepend_to_path(&tool_dir))
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("ZCCACHE_CACHE_DIR")
            .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
            .env_remove("ZCCACHE_DISABLE")
            .output()
            .unwrap_or_else(|_| panic!("failed to run direct rustc print probe {label}"));

        assert!(
            output.status.success(),
            "direct rustc print probe {label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
        assert!(
            !log.contains("zccache wrapper"),
            "direct rustc print probe {label} should bypass zccache: {log}"
        );
        assert!(
            log.lines().any(|line| line.starts_with("rustc ")),
            "direct rustc print probe {label} should invoke rustc directly: {log}"
        );
    }
}

#[test]
fn rustdoc_driver_is_intentionally_direct_without_zccache() {
    let cache_root = unique_temp_dir("rustdoc-direct-no-zccache");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let zccache_dir = unique_temp_dir("rustdoc-direct-zccache-bin");
    let zccache = fake_script_path(&zccache_dir, "zccache");
    write_fake_script(&zccache, &fake_zccache_script(&log_path));

    let output = isolated_soldr_command()
        .arg("rustdoc")
        .arg(&source_path)
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("PATH", isolated_test_path())
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr rustdoc with fake tools");

    assert!(
        output.status.success(),
        "rustdoc direct passthrough failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.lines().any(|line| line.starts_with("rustdoc ")),
        "rustdoc should run directly: {log}"
    );
    assert!(
        path_display_variants(&source_path)
            .iter()
            .any(|path| log.contains(path)),
        "rustdoc should receive the source file: {log}"
    );
    assert!(
        !log.contains("zccache wrapper"),
        "direct rustdoc should not route through zccache: {log}"
    );
}

#[test]
fn cargo_doc_keeps_rustc_wrapped_but_rustdoc_direct() {
    for (label, args) in [
        ("cargo-doc", vec!["cargo", "doc"]),
        ("bare-doc", vec!["doc"]),
    ] {
        let cache_root = unique_temp_dir(&format!("cargo-doc-rustdoc-policy-{label}"));
        let log_path = cache_root.join("tool.log");
        let source_path = write_rustdoc_source(&cache_root);
        let (rustup, cargo, rustc, rustdoc, zccache) =
            install_fake_cargo_doc_toolchain(&log_path, &source_path);

        let output = isolated_soldr_command()
            .args(args)
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
            .env("PATH", isolated_test_path())
            .env_remove("RUSTDOC")
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("ZCCACHE_CACHE_DIR")
            .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
            .env_remove("ZCCACHE_DISABLE")
            .output()
            .unwrap_or_else(|_| panic!("failed to run soldr doc route {label}"));

        assert!(
            output.status.success(),
            "cargo doc route {label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
        assert!(
            log.contains("cargo doc wrapper=") && log.contains("cache=1"),
            "cargo doc should run with cache enabled and a wrapper: {log}"
        );
        assert_zccache_wrapped_rustc_compile(&log, &rustc, "doc_demo");
        assert!(
            log.lines().any(|line| line.starts_with("rustdoc ")),
            "cargo doc should still invoke rustdoc directly: {log}"
        );
        assert_zccache_did_not_wrap_rustdoc(&log, &rustdoc);
    }
}

#[test]
fn cargo_doc_tests_keep_rustc_wrapped_but_rustdoc_direct() {
    let cache_root = unique_temp_dir("cargo-doctest-rustdoc-policy");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, cargo, rustc, rustdoc, zccache) =
        install_fake_cargo_doc_toolchain(&log_path, &source_path);

    let output = isolated_soldr_command()
        .args(["cargo", "test", "--doc"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("PATH", isolated_test_path())
        .env_remove("RUSTDOC")
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr cargo test --doc with fake tools");

    assert!(
        output.status.success(),
        "cargo doctest route failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo test --doc wrapper=") && log.contains("cache=1"),
        "cargo doctest should run with cache enabled and a wrapper: {log}"
    );
    assert_zccache_wrapped_rustc_compile(&log, &rustc, "doctest_demo");
    assert!(
        log.lines().any(|line| line.starts_with("rustdoc ")),
        "cargo doctest should still invoke rustdoc directly: {log}"
    );
    assert_zccache_did_not_wrap_rustdoc(&log, &rustdoc);
}

#[test]
fn rustdoc_path_shim_reenters_direct_passthrough_without_zccache() {
    let cache_root = unique_temp_dir("rustdoc-link-shim-no-zccache");
    let log_path = cache_root.join("tool.log");
    let shim_dir = cache_root.join("shims");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let zccache_dir = unique_temp_dir("rustdoc-link-shim-zccache-bin");
    let zccache = fake_script_path(&zccache_dir, "zccache");
    write_fake_script(&zccache, &fake_zccache_script(&log_path));

    let link_output = isolated_soldr_command()
        .args([
            "toolchain",
            "link",
            "--shim-dir",
            &shim_dir.display().to_string(),
        ])
        .current_dir(&cache_root)
        .output()
        .expect("failed to run soldr toolchain link");

    assert!(
        link_output.status.success(),
        "toolchain link for rustdoc shim failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&link_output.stdout),
        String::from_utf8_lossy(&link_output.stderr)
    );

    let rustdoc_shim = expected_link_shim_path(&shim_dir, "rustdoc");
    let mut command = Command::new(&rustdoc_shim);
    scrub_outer_soldr_env(&mut command);
    let output = command
        .arg(&source_path)
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("PATH", isolated_test_path())
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run rustdoc PATH shim");

    assert!(
        output.status.success(),
        "rustdoc PATH shim failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.lines().any(|line| line.starts_with("rustdoc ")),
        "rustdoc shim should re-enter direct rustdoc passthrough: {log}"
    );
    assert!(
        !log.contains("zccache wrapper"),
        "rustdoc shim should not route through zccache: {log}"
    );
}

#[test]
fn rustfmt_file_invocation_routes_through_zccache_formatter() {
    let cache_root = unique_temp_dir("rustfmt-zccache-formatter");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustfmt_source(&cache_root);
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let zccache_dir = unique_temp_dir("rustfmt-zccache-bin");
    let zccache = fake_script_path(&zccache_dir, "zccache");
    write_fake_script(&zccache, &fake_zccache_script(&log_path));

    let output = isolated_soldr_command()
        .arg("rustfmt")
        .arg(&source_path)
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("PATH", isolated_test_path())
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run soldr rustfmt with fake tools");

    assert!(
        output.status.success(),
        "rustfmt zccache route failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("zccache wrapper") && log.contains("rustfmt"),
        "rustfmt file invocation should route through zccache: {log}"
    );
    assert!(
        log_contains_cache_dir(&log, &cache_root),
        "rustfmt zccache route should use soldr's managed zccache cache dir: {log}"
    );
    assert!(
        path_display_variants(&source_path)
            .iter()
            .any(|path| log.contains(path)),
        "rustfmt should receive the source file: {log}"
    );
}

#[test]
fn rustfmt_no_cache_disable_and_version_bypass_zccache_formatter() {
    for (label, prefix_args, zccache_disable, include_source) in [
        ("no-cache", vec!["--no-cache", "rustfmt"], false, true),
        ("zccache-disable", vec!["rustfmt"], true, true),
        ("version", vec!["rustfmt", "--version"], false, false),
    ] {
        let cache_root = unique_temp_dir(&format!("rustfmt-bypass-{label}"));
        let log_path = cache_root.join("tool.log");
        let source_path = write_rustfmt_source(&cache_root);
        let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
        let zccache_dir = unique_temp_dir(&format!("rustfmt-bypass-zccache-{label}"));
        let zccache = fake_script_path(&zccache_dir, "zccache");
        write_fake_script(&zccache, &fake_zccache_script(&log_path));

        let mut command = isolated_soldr_command();
        command.args(prefix_args);
        if include_source {
            command.arg(&source_path);
        }
        command
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
            .env("PATH", isolated_test_path())
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("ZCCACHE_CACHE_DIR")
            .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR");
        if zccache_disable {
            command.env("ZCCACHE_DISABLE", "1");
        } else {
            command.env_remove("ZCCACHE_DISABLE");
        }

        let output = command
            .output()
            .unwrap_or_else(|_| panic!("failed to run rustfmt bypass case {label}"));

        assert!(
            output.status.success(),
            "rustfmt bypass case {label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).unwrap_or_else(|err| {
            panic!(
                "failed to read fake tool log for bypass case {label}: {err}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert!(
            log.contains("rustfmt"),
            "rustfmt should still run directly in bypass case {label}: {log}"
        );
        assert!(
            !log.contains("zccache wrapper"),
            "rustfmt bypass case {label} should not route through zccache: {log}"
        );
    }
}

#[test]
fn cargo_fmt_routes_rustfmt_through_zccache_formatter() {
    for (label, args) in [
        ("cargo-fmt", vec!["cargo", "fmt"]),
        ("bare-fmt", vec!["fmt"]),
    ] {
        let cache_root = unique_temp_dir(&format!("cargo-fmt-zccache-{label}"));
        let log_path = cache_root.join("tool.log");
        let source_path = write_rustfmt_source(&cache_root);
        let (rustup, cargo, rustc, _rustfmt, zccache) =
            install_fake_cargo_fmt_toolchain(&log_path, &source_path);

        let output = isolated_soldr_command()
            .args(args)
            .current_dir(&cache_root)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_CARGO_BIN", &cargo)
            .env("SOLDR_TEST_RUSTC_BIN", &rustc)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
            .env("PATH", isolated_test_path())
            .env_remove("RUSTFMT")
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .env_remove("ZCCACHE_CACHE_DIR")
            .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
            .env_remove("ZCCACHE_DISABLE")
            .output()
            .unwrap_or_else(|_| panic!("failed to run soldr {label} with fake tools"));

        assert!(
            output.status.success(),
            "cargo fmt route {label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
        assert!(
            log.contains("cargo fmt rustfmt=") && log.contains("env_rustfmt="),
            "fake cargo fmt should receive an explicit RUSTFMT shim: {log}"
        );
        assert!(
            log.contains("zccache wrapper") && log.contains("rustfmt"),
            "cargo fmt should route rustfmt through zccache: {log}"
        );
        assert!(
            log_contains_cache_dir(&log, &cache_root),
            "cargo fmt rustfmt shim should use soldr's managed zccache cache dir: {log}"
        );
    }
}

#[test]
fn cargo_fmt_no_cache_leaves_rustfmt_direct() {
    let cache_root = unique_temp_dir("cargo-fmt-no-cache-direct");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustfmt_source(&cache_root);
    let (rustup, cargo, rustc, rustfmt, zccache) =
        install_fake_cargo_fmt_toolchain(&log_path, &source_path);

    let output = isolated_soldr_command()
        .args(["--no-cache", "cargo", "fmt"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("PATH", isolated_test_path())
        .env_remove("RUSTFMT")
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        .output()
        .expect("failed to run no-cache cargo fmt with fake tools");

    assert!(
        output.status.success(),
        "no-cache cargo fmt failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        path_display_variants(&rustfmt)
            .iter()
            .any(|path| log.contains(&format!("rustfmt={path}"))),
        "no-cache cargo fmt should fall back to direct rustfmt: {log}"
    );
    assert!(
        !log.contains("zccache wrapper"),
        "no-cache cargo fmt should not route rustfmt through zccache: {log}"
    );
}

#[cfg(not(windows))]
#[test]
fn cargo_front_door_preserves_jobserver_fds_into_managed_zccache_wrapper() {
    let cache_root = unique_temp_dir("cargo-jobserver-fds");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_jobserver_toolchain(&log_path);
    let output = isolated_soldr_command()
        .args(["cargo", "test", "--no-run"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo test --no-run with fake jobserver fds");

    assert!(
        output.status.success(),
        "cache-enabled front door lost jobserver fds\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("failed to connect to jobserver"),
        "jobserver warning should not be emitted: {stderr}"
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("zccache jobserver fds ok read=3 write=4"),
        "managed zccache wrapper did not observe open jobserver fds: {log}"
    );
}

#[test]
fn cache_enabled_zccache_build_completes_under_20_seconds() {
    let cache_root = unique_temp_dir("cargo-zccache-timing");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);

    let started = Instant::now();
    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with fake zccache");
    let elapsed = started.elapsed();

    assert!(
        output.status.success(),
        "cache-enabled zccache build failed in {elapsed:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "cache-enabled zccache build took {elapsed:?}, expected under 20s"
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    // soldr#1368: the compile still routes through the zccache wrapper
    // seam (soldr-daemon embedded service in production); the managed
    // start/session lifecycle is gone.
    assert!(
        log.contains("zccache wrapper"),
        "timed build should still route rustc through the zccache wrapper: {log}"
    );
}

#[test]
fn managed_zccache_honors_explicit_cache_dir_override_when_trusted() {
    let cache_root = unique_temp_dir("cargo-explicit-zccache-dir");
    let user_zccache_dir = cache_root.join("user-zccache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("ZCCACHE_CACHE_DIR", &user_zccache_dir)
        .env("SOLDR_TRUST_INHERITED_ENV", "1")
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cargo build with explicit ZCCACHE_CACHE_DIR");

    assert!(
        output.status.success(),
        "explicit ZCCACHE_CACHE_DIR should be forwarded to zccache\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        path_display_variants(&user_zccache_dir)
            .iter()
            .any(|path| log.contains(&format!("zccache_dir={path}"))
                && log.contains(&format!("cache_dir={path}"))),
        "explicit ZCCACHE_CACHE_DIR should reach cargo and zccache commands: {log}"
    );
}

#[test]
fn nested_soldr_ignores_inherited_managed_zccache_cache_dir() {
    let parent_cache_root = unique_temp_dir("cargo-parent-managed-zccache-dir");
    let child_cache_root = unique_temp_dir("cargo-child-managed-zccache-dir");
    let parent_zccache_dir = parent_cache_root.join("cache").join("zccache");
    let log_path = child_cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &child_cache_root)
        .env("ZCCACHE_CACHE_DIR", &parent_zccache_dir)
        .env("SOLDR_MANAGED_ZCCACHE_CACHE_DIR", &parent_zccache_dir)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run nested soldr cargo build with inherited managed ZCCACHE_CACHE_DIR");

    assert!(
        output.status.success(),
        "inherited soldr-managed ZCCACHE_CACHE_DIR should not block nested soldr\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        !path_display_variants(&parent_zccache_dir)
            .iter()
            .any(|path| log.contains(&format!("zccache_dir={path}"))
                || log.contains(&format!("cache_dir={path}"))),
        "nested soldr should ignore inherited soldr-managed ZCCACHE_CACHE_DIR: {log}"
    );
}

#[test]
fn managed_zccache_injects_normalized_path_remap_by_default() {
    let cache_root = unique_temp_dir("cargo-normalized-remap");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let repo_root = unique_temp_dir("cargo-normalized-remap-repo");
    let nested = repo_root.join("crates").join("demo");
    fs::create_dir_all(repo_root.join(".git")).expect("failed to create fake git root");
    fs::create_dir_all(&nested).expect("failed to create nested cwd");

    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&nested)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env_remove("ZCCACHE_PATH_REMAP")
        .env_remove("ZCCACHE_WORKTREE_ROOT")
        .env_remove("SOLDR_PATH_REMAP")
        .env_remove("SOLDR_TARGET_CACHE_MODE")
        .env_remove("SOLDR_BUILD_CACHE_MODE")
        .output()
        .expect("failed to run soldr cargo build with normalized remap defaults");

    assert!(
        output.status.success(),
        "normalized remap front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("path_remap=auto"),
        "managed zccache should enable path remap by default: {log}"
    );
    assert!(
        path_display_variants(&repo_root)
            .iter()
            .any(|path| log.contains(&format!("worktree_root={path}"))),
        "managed zccache should pass the git root as ZCCACHE_WORKTREE_ROOT: {log}"
    );
}

#[test]
fn cargo_front_door_uses_custom_rustc_wrapper_from_env_var() {
    let cache_root = unique_temp_dir("cargo-custom-wrapper");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let wrapper = install_fake_wrapper(&log_path, "sccache");
    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_RUSTC_WRAPPER", &wrapper)
        .output()
        .expect("failed to run soldr cargo build with custom rustc wrapper");

    assert!(
        output.status.success(),
        "custom-wrapper front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains(&format!("cargo wrapper={}", wrapper.display())),
        "cargo should receive the custom wrapper path: {log}"
    );
    assert!(
        log.contains("sccache wrapper"),
        "custom wrapper should be invoked for rustc: {log}"
    );
    let expected_sccache_dir = cache_root.join("cache").join("sccache");
    assert!(
        path_display_variants(&expected_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccache_dir={path}"))),
        "cargo should receive soldr-owned SCCACHE_DIR at {}: {log}",
        expected_sccache_dir.display()
    );
    assert!(
        expected_sccache_dir.is_dir(),
        "soldr should pre-create the owned sccache cache dir at {}",
        expected_sccache_dir.display()
    );
    assert!(
        !log.contains(common::soldr_bin().to_string_lossy().as_ref()),
        "soldr should not stay in the wrapper slot when overridden: {log}"
    );
    assert!(
        !log.contains("zccache start")
            && !log.contains("zccache session-start")
            && !log.contains("zccache wrapper")
            && !log.contains("zccache session-end"),
        "managed zccache should be skipped when using a custom wrapper: {log}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("soldr: zccache session summary"),
        "custom wrapper path should not emit zccache session output: {stderr}"
    );
}

#[test]
fn custom_sccache_wrapper_preserves_caller_sccache_dir() {
    let cache_root = unique_temp_dir("cargo-custom-wrapper-preserve-sccache-dir");
    let caller_sccache_dir = unique_temp_dir("caller-sccache-dir");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let wrapper = install_fake_wrapper(&log_path, "sccache");
    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_RUSTC_WRAPPER", &wrapper)
        .env("SCCACHE_DIR", &caller_sccache_dir)
        .output()
        .expect("failed to run soldr cargo build with caller SCCACHE_DIR");

    assert!(
        output.status.success(),
        "custom-wrapper front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        path_display_variants(&caller_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccache_dir={path}"))),
        "cargo should preserve caller-provided SCCACHE_DIR at {}: {log}",
        caller_sccache_dir.display()
    );
    let soldr_sccache_dir = cache_root.join("cache").join("sccache");
    assert!(
        !path_display_variants(&soldr_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccache_dir={path}"))),
        "cargo should not override caller SCCACHE_DIR with {}: {log}",
        soldr_sccache_dir.display()
    );
}

#[test]
fn empty_rustc_wrapper_override_disables_wrapper_injection() {
    let cache_root = unique_temp_dir("cargo-wrapper-disabled");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_RUSTC_WRAPPER", "")
        .output()
        .expect("failed to run soldr cargo build with wrapper disabled");

    assert!(
        output.status.success(),
        "wrapper-disabled front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo wrapper= rustc="),
        "cargo should not receive a wrapper when override is empty: {log}"
    );
    assert!(
        !log.contains("zccache start")
            && !log.contains("zccache session-start")
            && !log.contains("zccache wrapper")
            && !log.contains("zccache session-end"),
        "managed zccache should be skipped when wrapper injection is disabled: {log}"
    );
    assert!(
        log.contains("rustc ") && log.contains("--crate-name demo"),
        "rustc should still run directly when wrapper injection is disabled: {log}"
    );
}

#[test]
fn no_cache_bypasses_wrapper_and_zccache() {
    let cache_root = unique_temp_dir("cargo-no-cache-fake");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = isolated_soldr_command()
        .args(["--no-cache", "cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr --no-cache cargo build with fake tools");

    assert!(
        output.status.success(),
        "no-cache front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cache=0"),
        "no-cache front door should propagate cache disable flag: {log}"
    );
    assert!(
        !log.contains("zccache start"),
        "no-cache front door should not start zccache: {log}"
    );
    assert!(
        !log.contains(common::soldr_bin().to_string_lossy().as_ref()),
        "no-cache front door should not set soldr as wrapper: {log}"
    );
    assert!(
        log.contains("rustc ") && log.contains("--crate-name demo"),
        "no-cache front door should call rustc directly: {log}"
    );
}

#[test]
fn rustc_wrapper_mode_passes_through_to_rustc() {
    let rustc = rustup_which("rustc");
    let output = isolated_soldr_command()
        .arg(rustc)
        .arg("--version")
        .output()
        .expect("failed to run soldr in wrapper mode");

    assert!(output.status.success(), "wrapper mode failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("rustc"),
        "unexpected rustc output: {stdout}"
    );
}

#[test]
fn repo_local_toolchain_homes_are_used_when_env_vars_are_unset() {
    let cache_root = unique_temp_dir("repo-local-toolchain-homes");
    let log_path = cache_root.join("tool.log");
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let repo_root = unique_temp_dir("repo-local-toolchain-root");
    let repo_cargo_home = repo_root.join(".cargo");
    let repo_rustup_home = repo_root.join(".rustup");
    let nested = repo_root.join("workspace").join("crate");
    fs::create_dir_all(&repo_cargo_home).expect("failed to create repo-local .cargo");
    fs::create_dir_all(&repo_rustup_home).expect("failed to create repo-local .rustup");
    fs::create_dir_all(&nested).expect("failed to create nested working dir");

    for args in [
        vec!["--no-cache", "cargo", "--version"],
        vec!["rustfmt", "--version"],
        vec!["--no-cache", "rustc", "--version"],
    ] {
        let output = isolated_soldr_command()
            .args(&args)
            .current_dir(&nested)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", isolated_test_path())
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap_or_else(|_| panic!("failed to run soldr with args {args:?}"));

        assert!(
            output.status.success(),
            "soldr invocation failed for {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let log = fs::read_to_string(&log_path).expect("failed to read fake rustup log");
    assert!(
        log_contains_toolchain_homes(
            &log,
            "rustup which cargo",
            &repo_cargo_home,
            &repo_rustup_home
        ),
        "cargo resolution should use repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(&log, "cargo", &repo_cargo_home, &repo_rustup_home),
        "cargo execution should inherit repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(
            &log,
            "rustup which rustfmt",
            &repo_cargo_home,
            &repo_rustup_home
        ),
        "rustfmt resolution should use repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(&log, "rustfmt", &repo_cargo_home, &repo_rustup_home),
        "rustfmt execution should inherit repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(
            &log,
            "rustup which rustc",
            &repo_cargo_home,
            &repo_rustup_home
        ),
        "rustc resolution should use repo-local homes: {log}"
    );
    assert!(
        log_contains_toolchain_homes(&log, "rustc", &repo_cargo_home, &repo_rustup_home),
        "rustc execution should inherit repo-local homes: {log}"
    );
}

#[test]
fn repo_local_cargo_bin_tools_work_without_rustup() {
    let cache_root = unique_temp_dir("repo-local-cargo-bin");
    let log_path = cache_root.join("tool.log");
    let rustup = install_failing_fake_rustup(&log_path);
    let repo_root = unique_temp_dir("repo-local-cargo-bin-root");
    let repo_cargo_bin = repo_root.join(".cargo").join("bin");
    let repo_rustup_home = repo_root.join(".rustup");
    let nested = repo_root.join("workspace").join("crate");
    fs::create_dir_all(&repo_cargo_bin).expect("failed to create repo-local .cargo/bin");
    // Anchor the rustup-home ancestor walk inside the test sandbox so it can't
    // climb up to a runner-installed `~/.rustup` (Windows GitHub runners put
    // TEMP under USERPROFILE, where `.rustup` typically exists).
    fs::create_dir_all(&repo_rustup_home).expect("failed to create repo-local .rustup");
    fs::create_dir_all(&nested).expect("failed to create nested working dir");
    install_fake_version_toolchain(&repo_cargo_bin, &log_path);

    for args in [
        vec!["--no-cache", "cargo", "--version"],
        vec!["rustfmt", "--version"],
        vec!["--no-cache", "rustc", "--version"],
    ] {
        let output = isolated_soldr_command()
            .args(&args)
            .current_dir(&nested)
            .env("SOLDR_CACHE_DIR", &cache_root)
            .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
            .env("PATH", isolated_test_path())
            .env_remove("CARGO_HOME")
            .env_remove("RUSTUP_HOME")
            .env_remove("RUSTUP_TOOLCHAIN")
            .output()
            .unwrap_or_else(|_| panic!("failed to run soldr with args {args:?}"));

        assert!(
            output.status.success(),
            "soldr invocation failed for {:?}\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.lines().any(|line| line.starts_with("cargo ")),
        "expected repo-local cargo shim to run: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("rustfmt ")),
        "expected repo-local rustfmt shim to run: {log}"
    );
    assert!(
        log.lines().any(|line| line.starts_with("rustc ")),
        "expected repo-local rustc shim to run: {log}"
    );
    assert!(
        !log.lines().any(|line| line.starts_with("rustup ")),
        "repo-local .cargo/bin tools should bypass rustup entirely: {log}"
    );
}

#[test]
fn explicit_toolchain_home_env_vars_win_over_repo_local_homes() {
    let cache_root = unique_temp_dir("explicit-toolchain-homes");
    let log_path = cache_root.join("tool.log");
    let (rustup, _, _, _) = install_fake_rustup_toolchain(&log_path);
    let repo_root = unique_temp_dir("explicit-toolchain-repo");
    let repo_cargo_home = repo_root.join(".cargo");
    let repo_rustup_home = repo_root.join(".rustup");
    let nested = repo_root.join("workspace").join("crate");
    let explicit_cargo_home = unique_temp_dir("explicit-cargo-home");
    let explicit_rustup_home = unique_temp_dir("explicit-rustup-home");
    fs::create_dir_all(&repo_cargo_home).expect("failed to create repo-local .cargo");
    fs::create_dir_all(&repo_rustup_home).expect("failed to create repo-local .rustup");
    fs::create_dir_all(&nested).expect("failed to create nested working dir");

    let output = isolated_soldr_command()
        .args(["--no-cache", "cargo", "--version"])
        .current_dir(&nested)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
        .env("CARGO_HOME", &explicit_cargo_home)
        .env("RUSTUP_HOME", &explicit_rustup_home)
        .env("PATH", isolated_test_path())
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .expect("failed to run soldr cargo --version with explicit homes");

    assert!(
        output.status.success(),
        "soldr cargo --version failed with explicit homes\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake rustup log");
    let explicit_cargo_home = explicit_cargo_home.display().to_string();
    let explicit_rustup_home = explicit_rustup_home.display().to_string();
    assert!(
        log.contains(&format!(
            "rustup which cargo cargo_home={explicit_cargo_home} rustup_home={explicit_rustup_home}"
        )),
        "cargo resolution should prefer explicit homes: {log}"
    );
    assert!(
        log.contains(&format!(
            "cargo cargo_home={explicit_cargo_home} rustup_home={explicit_rustup_home}"
        )),
        "cargo execution should inherit explicit homes: {log}"
    );
    assert!(
        !log.contains(&repo_cargo_home.display().to_string())
            && !log.contains(&repo_rustup_home.display().to_string()),
        "repo-local homes should not leak into explicit-home runs: {log}"
    );
}
