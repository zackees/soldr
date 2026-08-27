//! soldr#2546 slice 2 — GREEN fixture: `soldr --debug cargo build` records
//! the direct cargo child AND at least one live descendant through the
//! running-process observer (`with_observer_and_command`,
//! running-process#1023).
//!
//! The fake cargo spawns a short-lived grandchild (`sleep` / `ping`), so the
//! observed tree is soldr -> fake cargo -> grandchild — the depth the
//! per-OS LaunchedProcessTree backends exist to notice (subreaper+/proc on
//! Linux, Job Object IOCP on Windows, kqueue EVFILT_PROC on macOS).

use crate::common;

use crate::common::*;
use std::fs;
use std::path::{Path, PathBuf};

/// A fake cargo whose only job is to hold a grandchild alive long enough
/// for every descendant backend's polling window (Linux scans at 50ms).
fn write_fixture_workspace(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("fixture src dir");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("fixture manifest");
    fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("fixture main");
}

fn install_grandchild_spawning_cargo(log_path: &Path) -> PathBuf {
    let dir = unique_temp_dir("debug-trace-observer");
    let cargo = fake_script_path(&dir, "cargo");
    let script = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\n\
             echo cargo %*>>\"{0}\"\n\
             ping -n 2 127.0.0.1 >nul\n\
             echo fake-cargo-done\n\
             exit /b 0\n",
            log_path.display()
        )
    } else {
        format!(
            "#!/bin/sh\n\
             echo \"cargo $@\" >> \"{0}\"\n\
             sleep 1\n\
             echo fake-cargo-done\n\
             exit 0\n",
            log_path.display()
        )
    };
    write_fake_script(&cargo, &script);
    cargo
}

#[test]
fn debug_build_records_the_direct_child_and_a_descendant() {
    let cache_root = unique_temp_dir("debug-trace-observed-build");
    let log_path = cache_root.join("tool.log");
    write_fixture_workspace(&cache_root);
    let cargo = install_grandchild_spawning_cargo(&log_path);

    let output = isolated_soldr_command()
        .args(["--debug", "cargo", "build"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("ZCCACHE_DISABLE", "1")
        .output()
        .expect("failed to run soldr --debug cargo build with the fake cargo");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "--debug build failed\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout),
    );

    // Headless --debug builds take the diagnostic capture mode on Unix
    // (post-hoc attach, slice 3); Windows keeps the observed
    // inherited-stdio spawn, whose Job Object discovers descendants.
    let windows = matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    );
    if windows {
        assert!(
            stderr.contains("(cargo, observed)"),
            "Windows --debug must keep the observed spawn: {stderr}"
        );
    } else {
        assert!(
            stderr.contains("(cargo diagnostic capture)"),
            "expected the diagnostic-capture spawn announcement: {stderr}"
        );
    }
    assert!(
        stderr.contains("soldr debug: process timeline ->"),
        "expected the JSONL pointer: {stderr}"
    );
    // ...and the grandchild was noticed by the descendant backend —
    // through the Job Object on Windows's observed spawn, and through
    // the post-hoc attach on the Unix capture path.
    assert!(
        stderr.contains("descendant-started"),
        "expected a descendant-started event for the fake cargo's grandchild: {stderr}"
    );
    // running-process#1025: on Linux/macOS the pid-walk monitors name the
    // grandchild's immediate parent, so the event line must carry a real
    // ppid — 0 is the explicit unknown marker and would mean the parent
    // edge was lost. Windows' Job Object IOCP notification is PID-only by
    // design, so 0 is the documented value there.
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let ppid_ok = stderr.lines().any(|line| {
            line.contains("descendant-started")
                && line
                    .split("ppid=")
                    .nth(1)
                    .and_then(|rest| {
                        rest.split_whitespace()
                            .next()
                            .and_then(|value| value.parse::<u32>().ok())
                    })
                    .is_some_and(|ppid| ppid > 0)
        });
        assert!(
            ppid_ok,
            "expected a descendant-started event carrying a nonzero ppid: {stderr}"
        );
    }

    // The end-of-run summary identifies observed/incomplete descendants
    // (soldr#2546 acceptance: preserve ordering + identify unobserved exits).
    let summary_context = if windows {
        "summary (cargo): descendants started="
    } else {
        "summary (cargo diagnostic capture): descendants started="
    };
    assert!(
        stderr.contains(summary_context),
        "expected the summary line: {stderr}"
    );

    // The fake cargo actually ran (exit-code and stdio passthrough intact).
    let log = fs::read_to_string(&log_path).expect("fake cargo log");
    assert!(
        log.contains("cargo build"),
        "fake cargo saw the verb: {log}"
    );
}

#[test]
fn without_debug_the_observed_path_is_not_used() {
    let cache_root = unique_temp_dir("debug-trace-plain-build");
    let log_path = cache_root.join("tool.log");
    write_fixture_workspace(&cache_root);
    let cargo = install_grandchild_spawning_cargo(&log_path);

    let output = isolated_soldr_command()
        .args(["cargo", "build"])
        .current_dir(&cache_root)
        .env("SOLDR_CACHE_DIR", &cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .env("ZCCACHE_DISABLE", "1")
        .output()
        .expect("failed to run plain soldr cargo build with the fake cargo");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "plain build failed: {stderr}");
    assert!(
        !stderr.contains("soldr debug:"),
        "no debug-trace output without the flag: {stderr}"
    );
}

/// soldr#2546 slice 3: the capture front-door modes own their pipes, so
/// descendant observation attaches to the spawned cargo pid post-hoc
/// (running-process#1026's `observe_launched_tree`). A real cached
/// compile through a private daemon proves the attach end-to-end: rustc
/// shim processes are cargo's descendants and must appear in the
/// capture-mode timeline.
#[test]
fn debug_cached_capture_mode_records_descendants() {
    // The post-hoc attach rides the Unix polling monitors; Windows'
    // discovery is spawn-tied upstream and observes nothing here. The
    // real-compile fixture also needs a resolvable toolchain (soldr#2614
    // hosts lack one — same skip as daemon_restart_warmth).
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Linux
    ) {
        return;
    }
    let workdir = unique_temp_dir("debug-trace-json-capture");
    let cache_dir = workdir.join("cache-root");
    let home_dir = workdir.join("home");
    fs::create_dir_all(&cache_dir).expect("cache dir");
    fs::create_dir_all(&home_dir).expect("home dir");
    let _broker = common::BrokerHomeGuard::new(&cache_dir, &home_dir);
    let crate_dir = workdir.join("fixture");
    write_fixture_workspace(&crate_dir);

    let output = isolated_soldr_command()
        .args(["--debug", "cargo", "check"])
        .current_dir(&crate_dir)
        .env("SOLDR_CACHE_DIR", &cache_dir)
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .env("CARGO_TARGET_DIR", workdir.join("target"))
        .output()
        .expect("run soldr --debug cargo check");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Stop the fixture daemon before asserting so a panic cannot leak it.
    let _ = isolated_soldr_command()
        .args(["daemon", "stop"])
        .env("SOLDR_CACHE_DIR", &cache_dir)
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .output();

    if !output.status.success()
        && stderr.contains("rustup could not choose a version of cargo to run")
    {
        eprintln!("skipping: no default rustup toolchain on this host (soldr#2614)");
        return;
    }
    assert!(
        output.status.success(),
        "--debug cached build failed: {stderr}"
    );
    assert!(
        stderr.contains("(cargo diagnostic capture)"),
        "cache-enabled headless --debug build must take the diagnostic          capture mode: {stderr}"
    );
    assert!(
        stderr.lines().any(|line| {
            line.contains("descendant-started") && line.contains("cargo diagnostic capture")
        }),
        "capture mode must observe cargo's descendants (rustc): {stderr}"
    );
    assert!(
        stderr.contains("summary (cargo diagnostic capture): descendants started="),
        "capture-mode observation must emit its summary: {stderr}"
    );
}
