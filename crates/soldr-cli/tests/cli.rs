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
             echo cargo wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% cache=%SOLDR_CACHE_ENABLED% session=%ZCCACHE_SESSION_ID%>>\"{}\"\n\
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
             echo \"cargo wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} cache=${{SOLDR_CACHE_ENABLED:-}} session=${{ZCCACHE_SESSION_ID:-}}\" >> \"{}\"\n\
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

fn fake_zccache_script(log_path: &Path) -> String {
    #[cfg(windows)]
    {
        format!(
            "@echo off\n\
             if \"%~1\"==\"start\" (\n\
               echo zccache start>>\"{0}\"\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"session-start\" (\n\
               echo zccache session-start>>\"{0}\"\n\
               if not \"%~4\"==\"\" type nul > \"%~4\"\n\
               echo {{\"session_id\":\"test-session\"}}\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"session-end\" (\n\
               echo zccache session-end %~2>>\"{0}\"\n\
               echo hits: 1\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"status\" (\n\
               echo hits=7\n\
               exit /b 0\n\
             )\n\
             if \"%~1\"==\"clear\" (\n\
               echo zccache clear>>\"{0}\"\n\
               exit /b 0\n\
             )\n\
             set \"rustc=%~1\"\n\
             shift\n\
             echo zccache wrapper %rustc% %*>>\"{0}\"\n\
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
                 echo \"zccache start\" >> \"{0}\"\n\
                 exit 0\n\
                 ;;\n\
               session-start)\n\
                 echo \"zccache session-start\" >> \"{0}\"\n\
                 : > \"$4\"\n\
                 echo '{{\"session_id\":\"test-session\"}}'\n\
                 exit 0\n\
                 ;;\n\
               session-end)\n\
                 echo \"zccache session-end $2\" >> \"{0}\"\n\
                 echo 'hits: 1'\n\
                 exit 0\n\
                 ;;\n\
               status)\n\
                 echo 'hits=7'\n\
                 exit 0\n\
                 ;;\n\
               clear)\n\
                 echo \"zccache clear\" >> \"{0}\"\n\
                 exit 0\n\
                 ;;\n\
             esac\n\
             rustc=\"$1\"\n\
             shift\n\
             echo \"zccache wrapper $rustc $*\" >> \"{0}\"\n\
             \"$rustc\" \"$@\"\n",
            log_path.display()
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
    assert_eq!(json["managed_zccache_version"], "1.2.8");
    assert_eq!(json["root_dir"], cache_root.display().to_string());
    assert_eq!(
        json["cache_dir"],
        cache_root.join("cache").display().to_string()
    );
    assert_eq!(json["zccache"]["binary_fetched"], false);
    assert_eq!(json["zccache"]["journal_present"], false);
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
    assert_eq!(json["managed_zccache_version"], "1.2.8");
    assert_eq!(json["zccache"]["journal_present"], true);
    assert_eq!(json["zccache"]["binary_fetched"], true);
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
fn prepend_to_path(dir: &Path) -> std::ffi::OsString {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&existing));
    std::env::join_paths(paths).expect("failed to join PATH")
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
