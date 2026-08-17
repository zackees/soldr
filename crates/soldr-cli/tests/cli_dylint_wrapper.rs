mod common;

use common::{fake_script_path, isolated_soldr_command, prepend_to_path, unique_temp_dir};
use std::fs;
use std::path::{Path, PathBuf};

fn write_script(path: &Path, body: String) {
    fs::write(path, body).expect("write fake tool");
    soldr_platform::fs::permissions::make_executable(path).expect("chmod fake tool");
}

fn install_dylint_toolchain(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let tools = root.join("tools");
    fs::create_dir_all(&tools).expect("create fake tool dir");
    let log = root.join("tool.log");
    let cargo = fake_script_path(&tools, "cargo");
    let rustc = fake_script_path(&tools, "rustc");
    let zccache = fake_script_path(&tools, "zccache");
    let cargo_dylint = fake_script_path(&tools, "cargo-dylint");
    let dylint_link = fake_script_path(&tools, "dylint-link");
    let dylint_driver = fake_script_path(&tools, "dylint-driver");

    write_script(
        &cargo,
        format!(
            r#"#!/bin/sh
echo "cargo argv=$* wrapper=${{RUSTC_WRAPPER:-}}" >> "{log}"
if [ "$1" = "metadata" ]; then
  printf '{{"packages":[],"workspace_members":[],"workspace_default_members":[],"resolve":null,"target_directory":"target","version":1,"workspace_root":"."}}\n'
  exit 0
fi
exec "{cargo_dylint}" "$@"
"#,
            log = log.display(),
            cargo_dylint = cargo_dylint.display(),
        ),
    );
    write_script(
        &cargo_dylint,
        format!(
            r#"#!/bin/sh
echo "cargo-dylint argv=$* wrapper=${{RUSTC_WRAPPER:-}}" >> "{log}"
case "${{1:-}}" in
  --version) printf 'cargo-dylint 6.0.3\n'; exit 0 ;;
  --help) exit 0 ;;
esac
case "${{RUSTC_WRAPPER:-}}" in
  /*/soldr-dylint) ;;
  *) echo "RUSTC_WRAPPER is not an absolute soldr-dylint path: ${{RUSTC_WRAPPER:-}}" >&2; exit 91 ;;
esac
if [ "${{DYLINT_TEST_FAIL:-0}}" != "1" ]; then
  "$RUSTC_WRAPPER" "$RUSTC" --crate-name dylint_direct --emit link direct.rs || exit $?
fi
RUSTC_WORKSPACE_WRAPPER="{driver}" \
  "$RUSTC_WRAPPER" "{driver}" "$RUSTC" --crate-name dylint_nested --emit link nested.rs
"#,
            log = log.display(),
            driver = dylint_driver.display(),
        ),
    );
    write_script(
        &zccache,
        format!(
            r#"#!/bin/sh
echo "zccache compiler=$1 argv=$*" >> "{log}"
compiler="$1"
shift
exec "$compiler" "$@"
"#,
            log = log.display(),
        ),
    );
    write_script(&dylint_link, "#!/bin/sh\nexit 0\n".to_string());
    write_script(
        &dylint_driver,
        format!(
            r#"#!/bin/sh
echo "dylint-driver argv=$*" >> "{log}"
compiler="$1"
shift
exec "$compiler" "$@"
"#,
            log = log.display(),
        ),
    );
    write_script(
        &rustc,
        format!(
            r#"#!/bin/sh
echo "rustc argv=$*" >> "{log}"
if [ "${{1:-}}" = "-vV" ]; then
  printf 'rustc 1.89.0-nightly\nrelease: 1.89.0-nightly\ncommit-hash: 0123456789abcdef0123456789abcdef01234567\nhost: x86_64-unknown-linux-gnu\n'
  exit 0
fi
if [ "${{DYLINT_TEST_FAIL:-0}}" = "1" ]; then
  echo "dylint nested diagnostic on stdout"
  echo "dylint nested diagnostic on stderr" >&2
  exit 7
fi
echo "dylint compile diagnostic on stdout"
echo "dylint compile diagnostic on stderr" >&2
"#,
            log = log.display(),
        ),
    );

    let driver_root = root.join("drivers");
    let driver_channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );
    let prebuilt_driver = driver_root.join(driver_channel).join("dylint-driver");
    fs::create_dir_all(prebuilt_driver.parent().expect("driver parent"))
        .expect("create prebuilt driver dir");
    write_script(
        &prebuilt_driver,
        "#!/bin/sh\nprintf 'dylint-driver 6.0.3\\n'\n".to_string(),
    );

    (cargo, rustc, zccache, driver_root)
}

fn dylint_command(root: &Path) -> std::process::Command {
    let (cargo, rustc, zccache, driver_root) = install_dylint_toolchain(root);
    let channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );
    let identity = format!("{channel}|1.89.0-nightly|0123456789abcdef0123456789abcdef01234567");
    let mut command = isolated_soldr_command();
    command
        .current_dir(root)
        .env("PATH", prepend_to_path(&root.join("tools")))
        .env("SOLDR_HOME", root.join("soldr-home"))
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("SOLDR_TEST_RUSTC_BIN", rustc)
        .env("SOLDR_TEST_ZCCACHE_BIN", zccache)
        .env("DYLINT_DRIVER_PATH", driver_root)
        .env("SOLDR_DYLINT_CONFIGURED_TOOLCHAIN", channel)
        .env("SOLDR_DYLINT_CONFIGURED_RUSTC_RELEASE", "1.89.0-nightly")
        .env(
            "SOLDR_DYLINT_CONFIGURED_RUSTC_COMMIT_HASH",
            "0123456789abcdef0123456789abcdef01234567",
        )
        .env("SOLDR_DYLINT_PREPARED_IDENTITY", identity)
        // soldr#2436 phase 1 (D9): every dylint containment test carries
        // the tripwire, so any surviving implicit source-build path fails
        // with a distinctive message instead of silently compiling.
        .env("SOLDR_TEST_FORBID_SOURCE_BUILD", "1");
    command
}

#[test]
fn dylint_front_door_preserves_direct_and_nested_compiler_chains() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-wrapper-success");
    let output = dylint_command(&root)
        .args(["cargo", "dylint", "--all"])
        .output()
        .expect("run soldr cargo dylint");

    assert!(
        output.status.success(),
        "soldr cargo dylint failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(root.join("tool.log")).expect("read fake tool log");
    assert!(
        log.contains("cargo-dylint argv=dylint --all"),
        "logical cargo-dylint argv was not preserved: {log}"
    );
    assert!(
        log.lines().any(|line| line.contains("cargo-dylint")
            && line.contains("wrapper=/")
            && line.contains("/soldr-dylint")),
        "cargo-dylint did not receive an absolute dedicated wrapper: {log}"
    );
    assert!(
        log.lines().any(|line| line.contains("zccache compiler=")
            && line.contains("/rustc")
            && line.contains("dylint_direct")),
        "direct Dylint rustc compile did not reach zccache: {log}"
    );
    assert!(
        log.lines().any(|line| line.contains("zccache compiler=")
            && line.contains("/dylint-driver")
            && line.contains("/rustc")
            && line.contains("dylint_nested")),
        "nested Dylint driver chain did not reach zccache intact: {log}"
    );
    assert!(
        log.contains("dylint-driver argv=") && log.contains("dylint_nested"),
        "Dylint driver was not executed with the nested rustc invocation: {log}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("dylint compile diagnostic on stdout")
            && String::from_utf8_lossy(&output.stderr)
                .contains("dylint compile diagnostic on stderr"),
        "successful compiler diagnostics were not replayed"
    );
}

#[test]
fn dylint_front_door_preserves_failing_nested_diagnostics_and_exit() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-wrapper-failure");
    let output = dylint_command(&root)
        .args(["dylint", "--all"])
        .env("DYLINT_TEST_FAIL", "1")
        .output()
        .expect("run failing soldr dylint");

    assert_eq!(output.status.code(), Some(7));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("dylint nested diagnostic on stdout"),
        "failing stdout diagnostic was not replayed"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("dylint nested diagnostic on stderr"),
        "failing stderr diagnostic was not replayed"
    );
    let log = fs::read_to_string(root.join("tool.log")).expect("read fake tool log");
    assert!(
        log.contains("cargo-dylint argv=dylint --all")
            && log.contains("dylint-driver argv=")
            && log.contains("dylint_nested"),
        "top-level soldr dylint did not preserve the nested failure chain: {log}"
    );
}

#[test]
fn missing_prebuilt_driver_fails_before_cargo_dylint_launch() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-missing-prebuilt");
    let mut command = dylint_command(&root);
    fs::remove_dir_all(root.join("drivers")).expect("remove prebuilt driver fixture");

    let output = command
        .args(["dylint", "--all"])
        .output()
        .expect("run soldr dylint without a prebuilt driver");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Dylint v6.0.3 is not built for this machine"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        stderr.contains("Corrective action:"),
        "unexpected stderr: {stderr}"
    );
    let log = fs::read_to_string(root.join("tool.log")).unwrap_or_default();
    assert!(
        !log.contains("cargo-dylint argv=dylint"),
        "cargo-dylint lint execution must not launch when its driver is absent: {log}"
    );
}

/// soldr#2436 phase 1 (A3): a driver whose version probe hangs must be
/// killed AND reaped within the probe's 2-second deadline — bounded
/// failure, no orphaned child, no compiler process spawned.
#[test]
fn hanging_driver_probe_is_killed_and_reaped() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-hang-probe");
    let mut command = dylint_command(&root);
    let channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );
    let hung_driver = root.join("drivers").join(channel).join("dylint-driver");
    write_script(&hung_driver, "#!/bin/sh\nsleep 60\n".to_string());

    let started = std::time::Instant::now();
    let output = command
        .args(["dylint", "--all"])
        .output()
        .expect("run soldr dylint with a hanging driver probe");
    let elapsed = started.elapsed();

    assert_eq!(output.status.code(), Some(1));
    // Probe deadline is 2s; 30s leaves a 15x scheduler margin while
    // still proving the failure is bounded, not a hang.
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "hanging probe must fail bounded, took {elapsed:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("version probe exceeded the 2-second deadline"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        !stderr.contains("test tripwire"),
        "the source-build chokepoint must not be reached: {stderr}"
    );

    // Reap check (Linux): no process may survive whose cmdline names the
    // hung driver script. /proc scan, runtime-branched — no cfg.
    let needle = hung_driver.display().to_string();
    let survivors: Vec<String> = std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let cmdline = entry.path().join("cmdline");
                    std::fs::read(cmdline).ok().and_then(|bytes| {
                        let text = String::from_utf8_lossy(&bytes).replace('\0', " ");
                        text.contains(&needle).then_some(text)
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        survivors.is_empty(),
        "hung driver child was not reaped: {survivors:?}"
    );
}

/// soldr#2436 phase 1 (A6): an inherited Dylint scope is trusted — a
/// recursive invocation whose parent already established the scope must
/// NOT re-run the driver preflight (its parent owns that verdict), while
/// the entrypoint case without a driver stays a hard failure (covered by
/// missing_prebuilt_driver_fails_before_cargo_dylint_launch above).
#[test]
fn inherited_dylint_scope_skips_the_driver_preflight() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-inherited-scope");
    let mut command = dylint_command(&root);
    fs::remove_dir_all(root.join("drivers")).expect("remove prebuilt driver fixture");
    let channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );

    let output = command
        .env("SOLDR_DYLINT_TOOLCHAIN", channel)
        .args(["cargo", "dylint", "--all"])
        .output()
        .expect("run recursive soldr cargo dylint with inherited scope");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("is not built for this machine"),
        "inherited scope must not re-run the driver preflight: {stderr}"
    );
    assert!(
        !stderr.contains("test tripwire"),
        "inherited scope must not reach a source-build chokepoint: {stderr}"
    );
    assert!(
        output.status.success(),
        "inherited-scope invocation failed\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
}

/// soldr#2436 phase 6 (fact 6): one successful fake-tool dylint run must
/// leave the prepared marker behind, so the second run takes the warm path
/// (cached identity, no catalogue fetch).
#[test]
fn dylint_run_writes_the_prepared_marker_for_the_warm_path() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let root = unique_temp_dir("dylint-wrapper-marker");
    let output = dylint_command(&root)
        .args(["cargo", "dylint", "--all"])
        .output()
        .expect("run soldr cargo dylint");
    assert!(
        output.status.success(),
        "soldr cargo dylint failed\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // SoldrPaths roots at SOLDR_CACHE_DIR (= <root>/cache in this harness);
    // prepare_resolved derives the marker name from the configured release
    // (1.89.0-nightly -> "1.89").
    let marker = root
        .join("cache")
        .join("dylint")
        .join("prepared")
        .join("v1")
        .join("1.89.identity");
    assert!(
        marker.is_file(),
        "the prepared marker must exist after a successful dylint run \
         (looked at {}); without it every run repeats the cold catalogue \
         fetch + verification",
        marker.display()
    );
}
