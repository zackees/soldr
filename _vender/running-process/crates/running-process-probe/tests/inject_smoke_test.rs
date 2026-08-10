//! Slice 6d smoke test: prove `inject_into_pid` works end-to-end
//! against a real child process, by injecting a benign system DLL
//! and verifying `LoadLibraryW` returned a non-zero HMODULE.
//!
//! We deliberately inject `version.dll` (a small system library
//! every Windows host ships) instead of the interposer DLL — the
//! interposer's path depends on the cargo target dir layout, which
//! is awkward to discover from a test. Using a system DLL keeps
//! the test self-contained and exercises the same code path
//! (`OpenProcess` → `VirtualAllocEx` → `WriteProcessMemory` →
//! `CreateRemoteThread(LoadLibraryW, …)` → wait + collect exit
//! code).
//!
//! Slice 7's integration tests will additionally exercise the
//! interposer DLL itself by capturing the child's stderr and
//! asserting `RPP_HOOK file-open …` lines fire.

#![cfg(all(feature = "embed-helper", target_os = "windows"))]

use std::path::PathBuf;
use std::process::Command;

use running_process_probe::inject_into_pid;

/// Resolve the absolute path to `version.dll` in `C:\Windows\System32`.
/// Returns `None` if `SystemRoot` isn't set or the file isn't there
/// (no Windows host should hit either of those — guard anyway so
/// the test skips cleanly instead of false-failing on a stripped
/// container).
fn system32_dll(name: &str) -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")?;
    let p = PathBuf::from(root).join("System32").join(name);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[test]
fn inject_into_long_running_child_succeeds() {
    let Some(version_dll) = system32_dll("version.dll") else {
        eprintln!("skipping: SystemRoot/version.dll not found");
        return;
    };

    // Spawn a long-running benign target: `ping -n 60 127.0.0.1`.
    // Console host with no extra deps; PID lives long enough for
    // the injection to complete.
    let mut child = Command::new("cmd")
        .args(["/c", "ping", "-n", "60", "127.0.0.1"])
        .spawn()
        .expect("spawn cmd ping");
    let pid = child.id();

    // The injection itself. Returns the remote HMODULE — nonzero
    // means LoadLibraryW succeeded.
    let result = inject_into_pid(pid, &version_dll);

    // Kill the child regardless of outcome so we don't leak it.
    let _ = child.kill();
    let _ = child.wait();

    let hmodule = result.expect("inject_into_pid should succeed");
    assert!(
        hmodule != 0,
        "remote LoadLibraryW returned NULL — injection failed"
    );
}
