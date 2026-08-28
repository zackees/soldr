//! Cargo-doc front-door route deadline and direct-rustdoc integration tests.
//!
//! This module owns the doc-specific fake toolchain and route harness so the
//! general wrapper contract module can remain within the repository LOC ratchet.

#![allow(unused_imports)]

use crate::common;
use crate::common::*;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn fake_cargo_doc_script(log_path: &Path, source_path: &Path, rustdoc: &Path) -> String {
    let output_dir = fake_rustc_output_dir(log_path);
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             set \"rustdoc=%RUSTDOC%\"\n\
             if not defined RUSTDOC set \"rustdoc={2}\"\n\
             echo cargo %* wrapper=%RUSTC_WRAPPER% rustc=%RUSTC% rustdoc=%rustdoc% env_rustdoc=%RUSTDOC% cache=%SOLDR_CACHE_ENABLED%>>\"{0}\"\n\
             if \"%~1\"==\"doc\" (\n\
               echo cargo-doc phase=before-wrapper>>\"{0}\"\n\
               if /I \"%SOLDR_TEST_CARGO_DOC_HOLD_PHASE%\"==\"before-wrapper\" (\n\
                 echo cargo-doc phase=before-wrapper hold-descendant=spawned>>\"{0}\"\n\
                 start \"\" /b cmd /c \"ping -n 120 127.0.0.1 > nul\"\n\
                 ping -n 120 127.0.0.1 > nul\n\
               )\n\
               if defined RUSTC_WRAPPER (\n\
                 call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name doc_demo --emit dep-info,link \"{1}\" -o \"{3}\\doc_demo\" --out-dir \"{3}\"\n\
                 if errorlevel 1 exit /b 1\n\
               ) else (\n\
                 call \"%RUSTC%\" --crate-name doc_demo --emit dep-info,link \"{1}\" -o \"{3}\\doc_demo\" --out-dir \"{3}\"\n\
                 if errorlevel 1 exit /b 1\n\
               )\n\
               echo cargo-doc phase=after-wrapper>>\"{0}\"\n\
               echo cargo-doc phase=before-rustdoc>>\"{0}\"\n\
               if /I \"%SOLDR_TEST_CARGO_DOC_HOLD_PHASE%\"==\"before-rustdoc\" (\n\
                 echo cargo-doc phase=before-rustdoc hold-descendant=spawned>>\"{0}\"\n\
                 start \"\" /b cmd /c \"ping -n 120 127.0.0.1 > nul\"\n\
                 ping -n 120 127.0.0.1 > nul\n\
               )\n\
               call \"%rustdoc%\" \"{1}\"\n\
               if errorlevel 1 exit /b 1\n\
               echo cargo-doc phase=after-rustdoc>>\"{0}\"\n\
               exit /b\n\
             )\n\
             if \"%~1\"==\"test\" if \"%~2\"==\"--doc\" (\n\
               if defined RUSTC_WRAPPER (\n\
                 call \"%RUSTC_WRAPPER%\" \"%RUSTC%\" --crate-name doctest_demo --emit dep-info,link \"{1}\" -o \"{3}\\doctest_demo\" --out-dir \"{3}\"\n\
                 if errorlevel 1 exit /b 1\n\
               ) else (\n\
                 call \"%RUSTC%\" --crate-name doctest_demo --emit dep-info,link \"{1}\" -o \"{3}\\doctest_demo\" --out-dir \"{3}\"\n\
                 if errorlevel 1 exit /b 1\n\
               )\n\
               call \"%rustdoc%\" \"{1}\"\n\
               exit /b\n\
             )\n\
             echo unsupported fake cargo doc invocation %* 1>&2\n\
             exit /b 1\n",
            log_path.display(),
            source_path.display(),
            rustdoc.display(),
            output_dir.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             rustdoc=\"${{RUSTDOC:-{2}}}\"\n\
             echo \"cargo $* wrapper=${{RUSTC_WRAPPER:-}} rustc=${{RUSTC:-}} rustdoc=$rustdoc env_rustdoc=${{RUSTDOC:-}} cache=${{SOLDR_CACHE_ENABLED:-}}\" >> \"{0}\"\n\
             doc_phase() {{\n\
               echo \"cargo-doc phase=$1\" >> \"{0}\"\n\
               if [ \"${{SOLDR_TEST_CARGO_DOC_HOLD_PHASE:-}}\" = \"$1\" ]; then\n\
                 /bin/sleep 120 &\n\
                 echo \"cargo-doc phase=$1 hold-descendant=$!\" >> \"{0}\"\n\
                 wait \"$!\"\n\
               fi\n\
             }}\n\
             run_doc_compile() {{\n\
               crate_name=\"$1\"\n\
               doc_phase before-wrapper\n\
               if [ -n \"${{RUSTC_WRAPPER:-}}\" ]; then\n\
                 \"$RUSTC_WRAPPER\" \"$RUSTC\" --crate-name \"$crate_name\" --emit dep-info,link \"{1}\" -o \"{3}/$crate_name\" --out-dir \"{3}\" || exit $?\n\
               else\n\
                 \"$RUSTC\" --crate-name \"$crate_name\" --emit dep-info,link \"{1}\" -o \"{3}/$crate_name\" --out-dir \"{3}\" || exit $?\n\
               fi\n\
               doc_phase after-wrapper\n\
               doc_phase before-rustdoc\n\
               \"$rustdoc\" \"{1}\" || return $?\n\
               doc_phase after-rustdoc\n\
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
            rustdoc.display(),
            output_dir.display()
        )
    }
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
    write_fake_script(&rustc, &fake_rustc_script(log_path));
    write_fake_script(
        &cargo,
        &fake_cargo_doc_script(log_path, source_path, &rustdoc),
    );
    write_fake_script(&zccache, &fake_zccache_script(log_path));
    (rustup, cargo, rustc, rustdoc, zccache)
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
// The old `.output()` call delegated every route's liveness to Nextest's
// generic 120-second deadline. Keep a separate startup observation window for
// the real Soldr front door, then give every fixture phase its own short,
// named deadline once fake Cargo has accepted the route.
const CARGO_DOC_ROUTE_STARTUP_BUDGET: Duration = Duration::from_secs(30);
const CARGO_DOC_ROUTE_NORMAL_PHASE_BUDGET: Duration = Duration::from_secs(15);
const CARGO_DOC_ROUTE_HOLD_PHASE_BUDGET: Duration = Duration::from_secs(2);
const CARGO_DOC_ROUTE_REAP_BUDGET: Duration = Duration::from_secs(5);
const CARGO_DOC_ROUTE_POLL: Duration = Duration::from_millis(25);

struct CargoDocRouteOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    phase_markers: Vec<String>,
}

struct CargoDocRouteTimeout {
    phase_markers: Vec<String>,
    elapsed: Duration,
    diagnostics: String,
}

enum CargoDocRouteResult {
    Completed(CargoDocRouteOutput),
    TimedOut(CargoDocRouteTimeout),
}

struct CargoDocRoute<'a> {
    label: &'a str,
    args: &'a [&'a str],
    cache_root: &'a Path,
    log_path: &'a Path,
    cargo: &'a Path,
    rustc: &'a Path,
    rustup: &'a Path,
    hold_phase: Option<&'a str>,
    phase_budget: Duration,
}

fn spawn_route_pipe_drain<R>(mut pipe: R) -> Receiver<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = sender.send(pipe.read_to_end(&mut bytes).map(|_| bytes));
    });
    receiver
}

fn receive_route_pipe(
    receiver: &Receiver<std::io::Result<Vec<u8>>>,
    deadline: Instant,
    label: &str,
    pid: u32,
    pipe: &str,
) -> Result<Vec<u8>, String> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| {
            format!(
                "route {label} child pid {pid} left its {pipe} pipe open after tree cleanup: {error}"
            )
        })?
        .map_err(|error| format!("route {label} child pid {pid} failed to drain {pipe}: {error}"))
}

fn read_route_log(log_path: &Path) -> String {
    fs::read_to_string(log_path).unwrap_or_default()
}

fn cargo_doc_phase_markers(log: &str) -> Vec<String> {
    log.lines()
        .filter_map(|line| line.strip_prefix("cargo-doc phase=").map(str::to_string))
        .collect()
}

fn diagnostic_tail(text: &str) -> String {
    let mut lines: Vec<_> = text.lines().rev().take(12).collect();
    lines.reverse();
    lines.join("\n")
}

fn route_diagnostics(
    label: &str,
    pid: u32,
    elapsed: Duration,
    phase_markers: &[String],
    stdout: &[u8],
    stderr: &[u8],
    log_path: &Path,
) -> String {
    let log = read_route_log(log_path);
    format!(
        "route={label} child_pid={pid} elapsed={elapsed:?} phase_markers={phase_markers:?}\nstdout_tail:\n{}\nstderr_tail:\n{}\nfixture_log_tail:\n{}",
        diagnostic_tail(&String::from_utf8_lossy(stdout)),
        diagnostic_tail(&String::from_utf8_lossy(stderr)),
        diagnostic_tail(&log),
    )
}

fn reap_route_child(
    child: &mut std::process::Child,
    label: &str,
    pid: u32,
) -> Result<std::process::ExitStatus, String> {
    let deadline = Instant::now() + CARGO_DOC_ROUTE_REAP_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(CARGO_DOC_ROUTE_POLL),
            Ok(None) => {
                return Err(format!(
                    "route {label} child pid {pid} did not reap within {CARGO_DOC_ROUTE_REAP_BUDGET:?} after tree termination"
                ));
            }
            Err(error) => {
                return Err(format!(
                    "route {label} child pid {pid} could not be reaped: {error}"
                ));
            }
        }
    }
}

fn run_cargo_doc_route(route: CargoDocRoute<'_>) -> Result<CargoDocRouteResult, String> {
    let CargoDocRoute {
        label,
        args,
        cache_root,
        log_path,
        cargo,
        rustc,
        rustup,
        hold_phase,
        phase_budget,
    } = route;
    let mut command = isolated_soldr_command();
    command
        .args(args)
        .current_dir(cache_root)
        .env("SOLDR_CACHE_DIR", cache_root)
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("SOLDR_TEST_RUSTC_BIN", rustc)
        .env("SOLDR_TEST_RUSTUP_BIN", rustup)
        .env("PATH", isolated_test_path())
        .env_remove("RUSTDOC")
        .env_remove("CARGO_HOME")
        .env_remove("RUSTUP_HOME")
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("ZCCACHE_CACHE_DIR")
        .env_remove("SOLDR_MANAGED_ZCCACHE_CACHE_DIR")
        .env_remove("ZCCACHE_DISABLE")
        // The child root owns the process group below. The one-hop marker
        // makes the nested front door keep its Cargo descendant in that
        // group, so the merged tree terminator can prove pipe EOF.
        .env("SOLDR_INTERNAL_INHERIT_PROCESS_GROUP", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(phase) = hold_phase {
        command.env("SOLDR_TEST_CARGO_DOC_HOLD_PHASE", phase);
    } else {
        command.env_remove("SOLDR_TEST_CARGO_DOC_HOLD_PHASE");
    }
    // Match the product timeout boundary: the merged platform helper kills
    // this root's complete process group on Unix and snapshots its tree on
    // Windows, including fake Cargo's intentional hold descendant.
    soldr_platform::process::command::configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn cargo-doc route {label}: {error}"))?;
    let pid = child.id();
    let stdout = spawn_route_pipe_drain(child.stdout.take().expect("piped route stdout"));
    let stderr = spawn_route_pipe_drain(child.stderr.take().expect("piped route stderr"));
    let started = Instant::now();
    let startup_deadline = started + CARGO_DOC_ROUTE_STARTUP_BUDGET;
    let mut phase_deadline = None;
    let mut phase_count = 0;

    loop {
        let markers = cargo_doc_phase_markers(&read_route_log(log_path));
        if markers.len() > phase_count {
            phase_count = markers.len();
            phase_deadline = Some(Instant::now() + phase_budget);
        }
        let deadline = phase_deadline.unwrap_or(startup_deadline);
        if Instant::now() >= deadline {
            use soldr_platform::process::terminate::{terminate_tree, TreeKill};

            let tree_kill = terminate_tree(&mut child).map_err(|error| {
                format!("route {label} child pid {pid} tree termination failed: {error}")
            })?;
            let status = reap_route_child(&mut child, label, pid)?;
            let drain_deadline = Instant::now() + CARGO_DOC_ROUTE_REAP_BUDGET;
            let stdout = receive_route_pipe(&stdout, drain_deadline, label, pid, "stdout")?;
            let stderr = receive_route_pipe(&stderr, drain_deadline, label, pid, "stderr")?;
            let phase_markers = cargo_doc_phase_markers(&read_route_log(log_path));
            let diagnostics = route_diagnostics(
                label,
                pid,
                started.elapsed(),
                &phase_markers,
                &stdout,
                &stderr,
                log_path,
            );
            if tree_kill != TreeKill::TreeKilled {
                return Err(format!(
                    "route {label} timed out but tree cleanup was not verified ({tree_kill:?}); child status {status:?}\n{diagnostics}"
                ));
            }
            return Ok(CargoDocRouteResult::TimedOut(CargoDocRouteTimeout {
                phase_markers,
                elapsed: started.elapsed(),
                diagnostics,
            }));
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let drain_deadline = Instant::now() + CARGO_DOC_ROUTE_REAP_BUDGET;
                let stdout = receive_route_pipe(&stdout, drain_deadline, label, pid, "stdout")?;
                let stderr = receive_route_pipe(&stderr, drain_deadline, label, pid, "stderr")?;
                let phase_markers = cargo_doc_phase_markers(&read_route_log(log_path));
                return Ok(CargoDocRouteResult::Completed(CargoDocRouteOutput {
                    status,
                    stdout,
                    stderr,
                    phase_markers,
                }));
            }
            Ok(None) => std::thread::sleep(CARGO_DOC_ROUTE_POLL),
            Err(error) => {
                return Err(format!(
                    "route {label} child pid {pid} poll failed: {error}"
                ))
            }
        }
    }
}

fn assert_cargo_doc_phase_order(label: &str, markers: &[String], diagnostics: &str) {
    let expected = [
        "before-wrapper",
        "after-wrapper",
        "before-rustdoc",
        "after-rustdoc",
    ];
    let mut cursor = 0;
    for phase in expected {
        let Some(position) = markers[cursor..].iter().position(|marker| marker == phase) else {
            panic!("route {label} did not reach cargo-doc phase {phase}; markers={markers:?}\n{diagnostics}");
        };
        cursor += position + 1;
    }
}

fn expected_link_shim_path(dir: &Path, tool: &str) -> PathBuf {
    dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX))
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
        let (rustup, cargo, rustc, _rustdoc, _zccache) =
            install_fake_cargo_doc_toolchain(&log_path, &source_path);

        let result = run_cargo_doc_route(CargoDocRoute {
            label,
            args: &args,
            cache_root: &cache_root,
            log_path: &log_path,
            cargo: &cargo,
            rustc: &rustc,
            rustup: &rustup,
            hold_phase: None,
            phase_budget: CARGO_DOC_ROUTE_NORMAL_PHASE_BUDGET,
        })
        .unwrap_or_else(|error| panic!("failed to run soldr doc route {label}: {error}"));
        let CargoDocRouteResult::Completed(output) = result else {
            panic!("normal cargo-doc route {label} unexpectedly timed out");
        };

        assert!(
            output.status.success(),
            "cargo doc route {label} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let log = fs::read_to_string(&log_path).expect("failed to read fake tool log");
        // An absent private marker is enabled by contract; only an explicit
        // `0` disables caching. The wrapped fake-rustc assertion below proves
        // the behavioral path instead of overfitting Windows self-relocation.
        let cargo_doc = log
            .lines()
            .find(|line| line.starts_with("cargo doc wrapper="))
            .unwrap_or_else(|| panic!("cargo doc invocation missing from log: {log}"));
        assert!(
            !cargo_doc.contains("wrapper= ") && !cargo_doc.contains("cache=0"),
            "cargo doc should run with cache enabled and a wrapper: {cargo_doc}"
        );
        assert!(
            log.lines().any(|line| line.starts_with("rustc ")),
            "cargo doc route {label} should reach the wrapped rustc path: {log}"
        );
        assert!(
            log.lines().any(|line| line.starts_with("rustdoc ")),
            "cargo doc route {label} should invoke rustdoc directly: {log}"
        );
        let diagnostics = format!(
            "stdout:\n{}\nstderr:\n{}\nlog:\n{log}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_cargo_doc_phase_order(label, &output.phase_markers, &diagnostics);
    }
}

/// soldr#2944: a held fake-Cargo phase must fail under the harness-owned
/// deadline, not Nextest's generic 120-second ceiling. `run_cargo_doc_route`
/// returns this expected timeout only after merged `terminate_tree` verifies
/// the process tree, reaps the root, and both inherited pipes reach EOF; the
/// background sleep/ping is therefore a concrete no-surviving-descendant
/// proof rather than merely a short wall-clock assertion.
fn assert_cargo_doc_held_phase(
    label: &str,
    args: &[&str],
    hold_phase: &str,
    phase_budget: Duration,
) {
    let cache_root = unique_temp_dir(&format!("cargo-doc-held-phase-{label}"));
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, cargo, rustc, _rustdoc, _zccache) =
        install_fake_cargo_doc_toolchain(&log_path, &source_path);

    let result = run_cargo_doc_route(CargoDocRoute {
        label,
        args,
        cache_root: &cache_root,
        log_path: &log_path,
        cargo: &cargo,
        rustc: &rustc,
        rustup: &rustup,
        hold_phase: Some(hold_phase),
        phase_budget,
    })
    .unwrap_or_else(|error| panic!("held cargo-doc route {label} was not cleaned up: {error}"));
    let CargoDocRouteResult::TimedOut(timeout) = result else {
        panic!("held cargo-doc route {label} unexpectedly completed");
    };

    assert!(
        timeout
            .phase_markers
            .iter()
            .any(|marker| marker.starts_with(hold_phase)),
        "route {label} timed out before its intentional {hold_phase} hold\n{}",
        timeout.diagnostics
    );
    assert!(
        timeout.diagnostics.contains("hold-descendant="),
        "route {label} did not record the held descendant\n{}",
        timeout.diagnostics
    );
    assert!(
        timeout.elapsed
            < CARGO_DOC_ROUTE_STARTUP_BUDGET
                + phase_budget
                + CARGO_DOC_ROUTE_REAP_BUDGET
                + Duration::from_secs(5),
        "route {label} exceeded its startup + held-phase + cleanup budget ({:?})\n{}",
        timeout.elapsed,
        timeout.diagnostics
    );
}

#[test]
fn cargo_doc_route_deadline_kills_wrapper_hold_tree() {
    assert_cargo_doc_held_phase(
        "cargo-doc",
        &["cargo", "doc"],
        "before-wrapper",
        CARGO_DOC_ROUTE_HOLD_PHASE_BUDGET,
    );
}

#[test]
fn bare_doc_route_deadline_kills_rustdoc_hold_tree() {
    assert_cargo_doc_held_phase(
        "bare-doc",
        &["doc"],
        "before-rustdoc",
        CARGO_DOC_ROUTE_NORMAL_PHASE_BUDGET,
    );
}

#[test]
fn cargo_doc_tests_keep_rustc_wrapped_but_rustdoc_direct() {
    let cache_root = unique_temp_dir("cargo-doctest-rustdoc-policy");
    let log_path = cache_root.join("tool.log");
    let source_path = write_rustdoc_source(&cache_root);
    let (rustup, cargo, rustc, _rustdoc, _zccache) =
        install_fake_cargo_doc_toolchain(&log_path, &source_path);

    let output = isolated_soldr_command()
        .args(["cargo", "test", "--doc"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("SOLDR_TEST_RUSTC_BIN", &rustc)
        .env("SOLDR_TEST_RUSTUP_BIN", &rustup)
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
    // See the cargo-doc case above: absence is enabled, `0` is disabled.
    let cargo_doc_test = log
        .lines()
        .find(|line| line.starts_with("cargo test --doc wrapper="))
        .unwrap_or_else(|| panic!("cargo doctest invocation missing from log: {log}"));
    assert!(
        !cargo_doc_test.contains("wrapper= ") && !cargo_doc_test.contains("cache=0"),
        "cargo doctest should run with cache enabled and a wrapper: {cargo_doc_test}"
    );
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
