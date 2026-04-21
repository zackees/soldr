use serde_json::Value;
use std::process::Command;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn rustup_which(tool: &str) -> String {
    let output = Command::new("rustup")
        .args(["which", tool])
        .output()
        .expect("failed to resolve tool with rustup");
    assert!(output.status.success(), "rustup which failed for {tool}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

fn path_display_variants(path: &Path) -> Vec<String> {
    let mut variants = vec![path.display().to_string()];
    if let Ok(canonical) = fs::canonicalize(path) {
        let canonical = canonical.display().to_string();
        if !variants.contains(&canonical) {
            variants.push(canonical);
        }
    }
    variants
}

fn log_contains_toolchain_homes(
    log: &str,
    prefix: &str,
    cargo_home: &Path,
    rustup_home: &Path,
) -> bool {
    for cargo_home in path_display_variants(cargo_home) {
        for rustup_home in path_display_variants(rustup_home) {
            if log.contains(&format!(
                "{prefix} cargo_home={cargo_home} rustup_home={rustup_home}"
            )) {
                return true;
            }
        }
    }
    false
}

fn fake_script_path(dir: &Path, name: &str) -> PathBuf {
    #[cfg(windows)]
    {
        return dir.join(format!("{name}.cmd"));
    }
    #[cfg(not(windows))]
    {
        dir.join(name)
    }
}

fn write_fake_script(path: &Path, body: &str) {
    #[cfg(windows)]
    {
        fs::write(path, body.replace('\n', "\r\n")).expect("failed to write fake script");
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, body).expect("failed to write fake script");
        let mut perms = fs::metadata(path)
            .expect("failed to stat fake script")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("failed to chmod fake script");
    }
}

fn fake_cargo_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             echo cargo wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% session=%ZCCACHE_SESSION_ID% zcdir=%ZCCACHE_CACHE_DIR% sccachedir=%SCCACHE_DIR%>>\"{}\"\n\
             if defined RUSTC_WRAPPER (\n\
             call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name demo --emit dep-info,link\n\
             ) else (\n\
             call \"%RUSTC%\" --crate-name demo --emit dep-info,link\n\
             )\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}} zcdir=${{ZCCACHE_CACHE_DIR:-}} sccachedir=${{SCCACHE_DIR:-}}\" >> \"{}\"\n\
             if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
               \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name demo --emit dep-info,link\n\
             else\n\
               \"$RUSTC\" --crate-name demo --emit dep-info,link\n\
             fi\n",
            log_path.display()
        )
    }
}

fn fake_rustc_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             echo rustc %*>>\"{}\"\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             echo \"rustc $*\" >> \"{}\"\n",
            log_path.display()
        )
    }
}

fn fake_version_tool_script(log_path: &Path, tool_name: &str) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             echo {0} cargo_home=%CARGO_HOME% rustup_home=%RUSTUP_HOME% args=%*>>\"{1}\"\n\
             echo {0} 1.0.0 (fake)\n",
            tool_name,
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             echo \"{0} cargo_home=${{CARGO_HOME:-}} rustup_home=${{RUSTUP_HOME:-}} args=$*\" >> \"{1}\"\n\
             echo \"{0} 1.0.0 (fake)\"\n",
            tool_name,
            log_path.display()
        )
    }
}

fn fake_rustup_script(log_path: &Path, tool_dir: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             echo rustup %* cargo_home=%CARGO_HOME% rustup_home=%RUSTUP_HOME%>>\"{0}\"\n\
             if \"%~1\"==\"which\" (\n\
               if \"%~2\"==\"cargo\" (\n\
                 echo {1}\n\
                 exit /b 0\n\
               )\n\
               if \"%~2\"==\"rustc\" (\n\
                 echo {2}\n\
                 exit /b 0\n\
               )\n\
               if \"%~2\"==\"rustfmt\" (\n\
                 echo {3}\n\
                 exit /b 0\n\
               )\n\
             )\n\
             echo unsupported rustup invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display(),
            tool_dir.join("cargo.cmd").display(),
            tool_dir.join("rustc.cmd").display(),
            tool_dir.join("rustfmt.cmd").display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             echo \"rustup $* cargo_home=${{CARGO_HOME:-}} rustup_home=${{RUSTUP_HOME:-}}\" >> \"{0}\"\n\
             if [ \"$1\" = \"which\" ]; then\n\
               case \"$2\" in\n\
                 cargo)\n\
                   echo \"{1}\"\n\
                   exit 0\n\
                   ;;\n\
                 rustc)\n\
                   echo \"{2}\"\n\
                   exit 0\n\
                   ;;\n\
                 rustfmt)\n\
                   echo \"{3}\"\n\
                   exit 0\n\
                   ;;\n\
               esac\n\
             fi\n\
             echo \"unsupported rustup invocation: $*\" >&2\n\
             exit 1\n",
            log_path.display(),
            tool_dir.join("cargo").display(),
            tool_dir.join("rustc").display(),
            tool_dir.join("rustfmt").display()
        )
    }
}

fn fake_failing_rustup_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             echo rustup %* cargo_home=%CARGO_HOME% rustup_home=%RUSTUP_HOME%>>\"{}\"\n\
             echo rustup should not have been invoked 1>&2\n\
             exit /b 1\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             echo \"rustup $* cargo_home=${{CARGO_HOME:-}} rustup_home=${{RUSTUP_HOME:-}}\" >> \"{}\"\n\
             echo \"rustup should not have been invoked\" >&2\n\
             exit 1\n",
            log_path.display()
        )
    }
}

fn fake_zccache_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             if \"%~1\"==\"start\" (\n\
               echo zccache start cache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"session-start\" (\n\
               echo zccache session-start cache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
               if not \"%~4\"==\"\" type nul > \"%~4\"\n\
               echo {{\"session_id\":\"test-session\"}}\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"session-end\" (\n\
               echo zccache session-end %~2 cache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
               echo hits: 1\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"status\" (\n\
               echo zccache status cache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
               echo hits=7\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"clear\" (\n\
               echo zccache clear cache_dir=%ZCCACHE_CACHE_DIR%>>\"{0}\"\n\
               exit /b 0\n\
             )\n\
             set \"rustc=%~1\"\n\
             shift\n\
             echo zccache wrapper cache_dir=%ZCCACHE_CACHE_DIR% %rustc% %*>>\"{0}\"\n\
             call \"%rustc%\" %*\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display()
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               start)\n\
                 echo \"zccache start cache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
                 exit 0\n\
                 ;;\n\
               session-start)\n\
                 echo \"zccache session-start cache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
                 : > \"$4\"\n\
                 echo '{{\"session_id\":\"test-session\"}}'\n\
                 exit 0\n\
                 ;;\n\
               session-end)\n\
                 echo \"zccache session-end $2 cache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
                 echo 'hits: 1'\n\
                 exit 0\n\
                 ;;\n\
               status)\n\
                 echo \"zccache status cache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
                 echo 'hits=7'\n\
                 exit 0\n\
                 ;;\n\
               clear)\n\
                 echo \"zccache clear cache_dir=${{ZCCACHE_CACHE_DIR:-}}\" >> \"{0}\"\n\
                 exit 0\n\
                 ;;\n\
             esac\n\
             rustc=\"$1\"\n\
             shift\n\
             echo \"zccache wrapper cache_dir=${{ZCCACHE_CACHE_DIR:-}} $rustc $*\" >> \"{0}\"\n\
             \"$rustc\" \"$@\"\n",
            log_path.display()
        )
    }
}

fn fake_custom_wrapper_script(log_path: &Path, wrapper_name: &str) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             set \"rustc=%~1\"\n\
             shift\n\
             echo {1} wrapper sccachedir=%SCCACHE_DIR% %rustc% %*>>\"{0}\"\n\
             call \"%rustc%\" %*\n\
             exit /b %ERRORLEVEL%\n",
            log_path.display(),
            wrapper_name
        )
    }
    #[cfg(not(windows))]
    {
        format!(
            "#!/bin/sh\n\
             rustc=\"$1\"\n\
             shift\n\
             echo \"{1} wrapper sccachedir=${{SCCACHE_DIR:-}} $rustc $*\" >> \"{0}\"\n\
             \"$rustc\" \"$@\"\n",
            log_path.display(),
            wrapper_name
        )
    }
}

fn install_fake_toolchain(log_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let dir = unique_temp_dir("fake-toolchain");
    let cargo = fake_script_path(&dir, "cargo");
    let rustc = fake_script_path(&dir, "rustc");
    let zccache = fake_script_path(&dir, "zccache");
    write_fake_script(&cargo, &fake_cargo_script(log_path));
    write_fake_script(&rustc, &fake_rustc_script(log_path));
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (cargo, rustc, zccache)
}

fn install_fake_version_toolchain(tool_dir: &Path, log_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let cargo = fake_script_path(tool_dir, "cargo");
    let rustc = fake_script_path(tool_dir, "rustc");
    let rustfmt = fake_script_path(tool_dir, "rustfmt");
    write_fake_script(&cargo, &fake_version_tool_script(log_path, "cargo"));
    write_fake_script(&rustc, &fake_version_tool_script(log_path, "rustc"));
    write_fake_script(&rustfmt, &fake_version_tool_script(log_path, "rustfmt"));
    (cargo, rustc, rustfmt)
}

fn install_fake_wrapper(log_path: &Path, wrapper_name: &str) -> PathBuf {
    let dir = unique_temp_dir("fake-wrapper");
    let wrapper = fake_script_path(&dir, wrapper_name);
    write_fake_script(
        &wrapper,
        &fake_custom_wrapper_script(log_path, wrapper_name),
    );
    wrapper
}

fn install_fake_rustup_toolchain(log_path: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = unique_temp_dir("fake-rustup-toolchain");
    let cargo = fake_script_path(&dir, "cargo");
    let rustc = fake_script_path(&dir, "rustc");
    let rustfmt = fake_script_path(&dir, "rustfmt");
    #[cfg(windows)]
    let rustup = dir.join("rustup.bat");
    #[cfg(not(windows))]
    let rustup = fake_script_path(&dir, "rustup");
    write_fake_script(&cargo, &fake_version_tool_script(log_path, "cargo"));
    write_fake_script(&rustc, &fake_version_tool_script(log_path, "rustc"));
    write_fake_script(&rustfmt, &fake_version_tool_script(log_path, "rustfmt"));
    write_fake_script(&rustup, &fake_rustup_script(log_path, &dir));
    (rustup, cargo, rustc, rustfmt)
}

fn install_failing_fake_rustup(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("fake-rustup-failure");
    #[cfg(windows)]
    let rustup = dir.join("rustup.bat");
    #[cfg(not(windows))]
    let rustup = fake_script_path(&dir, "rustup");
    write_fake_script(&rustup, &fake_failing_rustup_script(log_path));
    rustup
}

#[cfg(windows)]
fn prepend_to_path(dir: &Path) -> std::ffi::OsString {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&existing));
    std::env::join_paths(paths).expect("failed to join PATH")
}

/// PATH value for tests that need to verify soldr's tool resolution falls back
/// to its rustup path. Strips the runner's real cargo/rustc entries so
/// `probe_toolchain_binary`'s PATH search can't shadow the in-test fakes.
/// On Windows we keep `System32` so `Command::new` can still spawn `.cmd`
/// shims via `cmd.exe`.
fn isolated_test_path() -> std::ffi::OsString {
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
        let dirs = [system_root.join("System32"), system_root];
        std::env::join_paths(dirs).expect("failed to join isolated PATH")
    }
    #[cfg(not(windows))]
    {
        std::ffi::OsString::from("/usr/bin:/bin")
    }
}

#[test]
fn version_command_prints_workspace_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("version")
        .output()
        .expect("failed to run soldr version");

    assert!(output.status.success(), "version command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        format!("soldr {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn version_command_emits_versioned_json() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["version", "--json"])
        .output()
        .expect("failed to run soldr version --json");

    assert!(output.status.success(), "version --json command failed");

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("version --json did not return JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "version");
    assert_eq!(json["soldr_version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn help_lists_phase_one_command_surface() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("--help")
        .output()
        .expect("failed to run soldr --help");

    assert!(output.status.success(), "help command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status"), "help output missing status");
    assert!(stdout.contains("clean"), "help output missing clean");
    assert!(stdout.contains("config"), "help output missing config");
    assert!(stdout.contains("cache"), "help output missing cache");
    assert!(stdout.contains("version"), "help output missing version");
    assert!(stdout.contains("cargo"), "help output missing cargo");
}

#[test]
fn cargo_front_door_runs_real_cargo() {
    let cache_root = unique_temp_dir("cargo-version");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["--no-cache", "cargo", "--version"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cargo --version");

    assert!(output.status.success(), "cargo front door failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("cargo"),
        "unexpected cargo output: {stdout}"
    );
    assert!(
        !stderr.contains("soldr: fetching cargo"),
        "cargo front door should not fetch cargo: {stderr}"
    );
}

#[test]
fn cargo_front_door_consumes_no_cache_flag() {
    let cache_root = unique_temp_dir("cargo-no-cache");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["--no-cache", "cargo", "--version"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr --no-cache cargo --version");

    assert!(
        output.status.success(),
        "cargo front door with top-level --no-cache failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("cargo"),
        "unexpected cargo output with --no-cache: {stdout}"
    );
    assert!(
        !stderr.contains("unexpected argument '--no-cache'"),
        "--no-cache should be consumed by soldr, not forwarded to cargo: {stderr}"
    );
}

#[test]
fn cargo_subcommand_rejects_no_cache_flag() {
    let cache_root = unique_temp_dir("cargo-subcommand-no-cache");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "--no-cache", "--version"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr cargo --no-cache --version");

    assert!(
        !output.status.success(),
        "cargo subcommand form should no longer accept --no-cache"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--no-cache"),
        "expected cargo-subcommand form to fail mentioning --no-cache: {stderr}"
    );
}

#[test]
fn cargo_front_door_uses_soldr_wrapper_and_managed_zccache_by_default() {
    let cache_root = unique_temp_dir("cargo-default-cache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cargo build with fake zccache");

    assert!(
        output.status.success(),
        "cache-enabled front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("cargo wrapper="),
        "fake cargo did not record wrapper env: {log}"
    );
    assert!(
        log.contains(env!("CARGO_BIN_EXE_soldr")),
        "soldr should own the wrapper slot in cache-enabled mode: {log}"
    );
    assert!(
        log.contains("cache=1"),
        "cache-enabled front door should propagate cache flag: {log}"
    );
    assert!(
        log.contains("zccache start"),
        "managed zccache should be started for cache-enabled builds: {log}"
    );
    assert!(
        log.contains("zccache session-start"),
        "managed zccache session should start before cargo runs: {log}"
    );
    assert!(
        log.contains("zccache wrapper"),
        "wrapper mode should dispatch into zccache on cache-enabled builds: {log}"
    );
    assert!(
        log.contains("rustc ") && log.contains("--crate-name demo"),
        "real rustc invocation should still happen through the wrapper path: {log}"
    );
    assert!(
        log.contains("zccache session-end test-session"),
        "managed zccache session should end after cargo finishes: {log}"
    );
    let zccache_cache_dir = cache_root.join("cache").join("zccache");
    assert!(
        path_display_variants(&zccache_cache_dir)
            .iter()
            .any(|path| log.contains(&format!("cache_dir={path}"))),
        "managed zccache commands should use Soldr's zccache cache root at {}: {log}",
        zccache_cache_dir.display()
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("soldr: zccache session summary"),
        "expected zccache session summary in stderr: {stderr}"
    );

    let journal = cache_root
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session.jsonl");
    assert!(
        journal.exists(),
        "expected session journal at {}",
        journal.display()
    );
}

#[test]
fn cargo_front_door_uses_custom_rustc_wrapper_from_env_var() {
    let cache_root = unique_temp_dir("cargo-custom-wrapper");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let wrapper = install_fake_wrapper(&log_path, "sccache");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
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
    let sccache_dir = cache_root.join("cache").join("sccache");
    assert!(
        path_display_variants(&sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccachedir={path}"))),
        "custom sccache wrapper should receive Soldr's sccache cache root at {}: {log}",
        sccache_dir.display()
    );
    assert!(
        !log.contains(env!("CARGO_BIN_EXE_soldr")),
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
fn explicit_sccache_dir_wins_for_custom_sccache_wrapper() {
    let cache_root = unique_temp_dir("cargo-custom-sccache-explicit-dir");
    let explicit_sccache_dir = unique_temp_dir("explicit-sccache-dir");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let wrapper = install_fake_wrapper(&log_path, "sccache");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("SOLDR_RUSTC_WRAPPER", &wrapper)
        .env("SCCACHE_DIR", &explicit_sccache_dir)
        .output()
        .expect("failed to run soldr cargo build with explicit SCCACHE_DIR");

    assert!(
        output.status.success(),
        "custom-wrapper front door failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        path_display_variants(&explicit_sccache_dir)
            .iter()
            .any(|path| log.contains(&format!("sccachedir={path}"))),
        "explicit SCCACHE_DIR should be preserved: {log}"
    );
    assert!(
        !log.contains(
            &cache_root
                .join("cache")
                .join("sccache")
                .display()
                .to_string()
        ),
        "Soldr's default sccache dir should not override explicit SCCACHE_DIR: {log}"
    );
}

#[test]
fn managed_zccache_rejects_conflicting_explicit_cache_dir() {
    let cache_root = unique_temp_dir("cargo-zccache-conflicting-dir");
    let explicit_zccache_dir = unique_temp_dir("explicit-zccache-dir");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("ZCCACHE_CACHE_DIR", &explicit_zccache_dir)
        .output()
        .expect("failed to run soldr cargo build with explicit ZCCACHE_CACHE_DIR");

    assert!(
        !output.status.success(),
        "managed zccache should reject a conflicting explicit cache dir"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ZCCACHE_CACHE_DIR") && stderr.contains("managed zccache builds require"),
        "expected explicit cache-dir precedence error: {stderr}"
    );
}

#[test]
fn managed_zccache_accepts_explicit_cache_dir_when_it_matches_soldr_root() {
    let cache_root = unique_temp_dir("cargo-zccache-matching-dir");
    let zccache_cache_dir = cache_root.join("cache").join("zccache");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cargo", "build"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .env("ZCCACHE_CACHE_DIR", &zccache_cache_dir)
        .output()
        .expect("failed to run soldr cargo build with matching ZCCACHE_CACHE_DIR");

    assert!(
        output.status.success(),
        "matching explicit cache dir should be accepted\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        path_display_variants(&zccache_cache_dir)
            .iter()
            .any(|path| log.contains(&format!("cache_dir={path}"))),
        "managed zccache should use matching explicit Soldr cache root: {log}"
    );
}

#[test]
fn empty_rustc_wrapper_override_disables_wrapper_injection() {
    let cache_root = unique_temp_dir("cargo-wrapper-disabled");
    let log_path = cache_root.join("tool.log");
    let (cargo, rustc, zccache) = install_fake_toolchain(&log_path);
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
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
        !log.contains(env!("CARGO_BIN_EXE_soldr")),
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
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
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
        let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
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
        let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
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

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
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

#[test]
fn rustup_resolution_failure_reports_raw_error_and_ci_guidance() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["--no-cache", "rustc", "--version"])
        .env("RUSTUP_TOOLCHAIN", "soldr-ci-missing-toolchain")
        .output()
        .expect("failed to run soldr --no-cache rustc --version with invalid RUSTUP_TOOLCHAIN");

    assert!(
        !output.status.success(),
        "expected soldr rustc --version to fail when rustup resolution fails"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to resolve rustc via rustup: error: override toolchain 'soldr-ci-missing-toolchain' is not installed"),
        "expected raw rustup stderr to be preserved: {stderr}"
    );
    assert!(
        stderr.contains(
            "the RUSTUP_TOOLCHAIN environment variable specifies an uninstalled toolchain"
        ),
        "expected raw rustup explanation in stderr: {stderr}"
    );
    assert!(
        stderr.contains("pins Rust in rust-toolchain.toml"),
        "expected rust-toolchain.toml guidance in stderr: {stderr}"
    );
    assert!(
        stderr.contains("generic stable toolchain"),
        "expected exact-channel guidance in stderr: {stderr}"
    );
    assert!(
        stderr.contains("RUSTUP_TOOLCHAIN"),
        "expected RUSTUP_TOOLCHAIN guidance in stderr: {stderr}"
    );
    assert!(
        stderr.contains("setup-soldr action path"),
        "expected setup-soldr guidance in stderr: {stderr}"
    );
}

#[test]
fn status_reports_cache_control_defaults() {
    let cache_root = unique_temp_dir("status");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("status")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr status");

    assert!(output.status.success(), "status command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cache dir:"),
        "status missing cache dir: {stdout}"
    );
    assert!(
        stdout.contains("cache default: enabled"),
        "status missing cache default: {stdout}"
    );
    assert!(
        stdout.contains("zccache version:"),
        "status missing zccache version: {stdout}"
    );
    assert!(
        stdout.contains("zccache artifact cache dir:"),
        "status missing zccache artifact cache dir: {stdout}"
    );
    assert!(
        stdout.contains("not fetched yet"),
        "status should explain unfetched managed zccache state: {stdout}"
    );
}

#[test]
fn status_json_reports_stable_machine_fields() {
    let cache_root = unique_temp_dir("status-json");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["status", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .output()
        .expect("failed to run soldr status --json");

    assert!(output.status.success(), "status --json command failed");

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("status --json did not return JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "status");
    assert_eq!(json["soldr_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["cache_default_enabled"], true);
    assert_eq!(json["cache_enabled_for_invocation"], true);
    assert_eq!(json["managed_zccache_version"], "1.3.0");
    assert_eq!(json["root_dir"], cache_root.display().to_string());
    assert_eq!(
        json["cache_dir"],
        cache_root.join("cache").display().to_string()
    );
    assert_eq!(json["zccache"]["binary_fetched"], false);
    assert_eq!(json["zccache"]["journal_present"], false);
    assert_eq!(
        json["zccache"]["artifact_cache_dir"],
        cache_root
            .join("cache")
            .join("zccache")
            .display()
            .to_string()
    );
    assert!(
        json["target"].as_str().is_some(),
        "status JSON missing target"
    );
}

#[test]
fn cache_command_reports_managed_zccache_status() {
    let cache_root = unique_temp_dir("cache-command");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);
    let journal = cache_root
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session.jsonl");
    fs::create_dir_all(journal.parent().expect("journal parent missing"))
        .expect("failed to create journal dir");
    fs::write(&journal, "{\"event\":\"hit\"}\n").expect("failed to seed journal");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("cache")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cache");

    assert!(output.status.success(), "cache command failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("soldr zccache state dir:"),
        "cache command missing state dir: {stdout}"
    );
    assert!(
        stdout.contains("zccache artifact cache dir:"),
        "cache command missing artifact cache dir: {stdout}"
    );
    assert!(
        stdout.contains("last session journal:"),
        "cache command missing journal path: {stdout}"
    );
    assert!(
        stdout.contains("(present)"),
        "cache command should report present journal: {stdout}"
    );
    assert!(
        stdout.contains("zccache: hits=7"),
        "cache command should surface managed zccache status output: {stdout}"
    );
}

#[test]
fn cache_json_reports_managed_zccache_status() {
    let cache_root = unique_temp_dir("cache-command-json");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);
    let journal = cache_root
        .join("cache")
        .join("zccache")
        .join("logs")
        .join("last-session.jsonl");
    fs::create_dir_all(journal.parent().expect("journal parent missing"))
        .expect("failed to create journal dir");
    fs::write(&journal, "{\"event\":\"hit\"}\n").expect("failed to seed journal");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["cache", "--json"])
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr cache --json");

    assert!(output.status.success(), "cache --json command failed");

    let json: Value =
        serde_json::from_slice(&output.stdout).expect("cache --json did not return JSON");
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["command"], "cache");
    assert_eq!(json["managed_zccache_version"], "1.3.0");
    assert_eq!(json["zccache"]["journal_present"], true);
    assert_eq!(json["zccache"]["binary_fetched"], true);
    assert_eq!(
        json["zccache"]["artifact_cache_dir"],
        cache_root
            .join("cache")
            .join("zccache")
            .display()
            .to_string()
    );
    assert_eq!(
        json["zccache"]["journal_path"],
        journal.display().to_string()
    );
    assert_eq!(
        json["zccache"]["status_lines"][0],
        Value::String("hits=7".to_string())
    );
}

#[test]
fn clean_clears_managed_zccache_and_state_dir() {
    let cache_root = unique_temp_dir("clean-command");
    let log_path = cache_root.join("tool.log");
    let (_, _, zccache) = install_fake_toolchain(&log_path);
    let state_dir = cache_root.join("cache").join("zccache");
    let journal = state_dir.join("logs").join("last-session.jsonl");
    fs::create_dir_all(journal.parent().expect("journal parent missing"))
        .expect("failed to create journal dir");
    fs::write(&journal, "{\"event\":\"hit\"}\n").expect("failed to seed journal");

    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .arg("clean")
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_ZCCACHE_BIN", &zccache)
        .output()
        .expect("failed to run soldr clean");

    assert!(output.status.success(), "clean command failed");
    assert!(
        !state_dir.exists(),
        "clean should remove soldr zccache state dir at {}",
        state_dir.display()
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cleared zccache artifact cache"),
        "clean should report artifact cache cleanup: {stdout}"
    );
    assert!(
        stdout.contains("removed soldr zccache state dir:"),
        "clean should report state dir cleanup: {stdout}"
    );

    let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
    assert!(
        log.contains("zccache clear"),
        "clean should call managed zccache clear: {log}"
    );
}

#[test]
fn clean_rejects_json_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["clean", "--json"])
        .output()
        .expect("failed to run soldr clean --json");

    assert!(
        !output.status.success(),
        "clean --json should be rejected because JSON is not supported there"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--json"),
        "expected clap to reject clean --json: {stderr}"
    );
}

#[cfg(windows)]
#[test]
fn cargo_front_door_forces_msvc_target_even_with_polluted_path() {
    let fake_tools = unique_temp_dir("fake-tools");
    fs::write(
        fake_tools.join("cargo.cmd"),
        "@echo off\r\necho fake cargo should not be used 1>&2\r\nexit /b 1\r\n",
    )
    .expect("failed to write fake cargo.cmd");
    fs::write(
        fake_tools.join("rustc.cmd"),
        "@echo off\r\necho fake rustc should not be used 1>&2\r\nexit /b 1\r\n",
    )
    .expect("failed to write fake rustc.cmd");

    let target_dir = unique_temp_dir("target-dir");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("windows-msvc-default");
    let output = Command::new(env!("CARGO_BIN_EXE_soldr"))
        .args(["--no-cache", "cargo", "build"])
        .current_dir(&fixture)
        .env("PATH", prepend_to_path(&fake_tools))
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("SOLDR_CACHE_DIR", unique_temp_dir("msvc-cache-root"))
        .output()
        .expect("failed to run soldr cargo build");

    assert!(
        output.status.success(),
        "soldr cargo build failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let artifact = target_dir
        .join("x86_64-pc-windows-msvc")
        .join("debug")
        .join("windows-msvc-default.exe");
    assert!(
        artifact.exists(),
        "expected MSVC target artifact at {}",
        artifact.display()
    );
}
