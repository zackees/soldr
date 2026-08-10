//! Slice 7c diagnostic test (#551). Doesn't assert that detours
//! fire on a real file API call — that's the ignored test in
//! `interposer_integration_windows.rs`. Instead: spawn a long-
//! running child, inject the interposer, capture stderr, and
//! report which `RPP_HOOK install begin=… / end=…` pair lines
//! appeared. The pattern of which "end=" lines are missing tells
//! us which retour install hung or errored.
//!
//! This test always passes (it's a diagnostic / observational
//! test) but emits its findings via `eprintln!` so they show up
//! in the standard cargo nextest log capture and CI diagnostics.

#![cfg(all(feature = "embed-helper", target_os = "windows"))]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use running_process_probe::inject_into_pid;

fn target_profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .and_then(|p| p.parent())
        .expect("walk up")
        .to_path_buf()
}

fn build_interposer_dll() -> PathBuf {
    let mut cmd = if which::which("soldr").is_ok() {
        let mut c = Command::new("soldr");
        c.arg("cargo");
        c
    } else {
        Command::new("cargo")
    };
    let status = cmd
        .args(["build", "-p", "running-process-probe-interposer-windows"])
        .status()
        .expect("cargo build");
    assert!(status.success(), "cargo build failed: {status:?}");
    let p = target_profile_dir().join("running_process_probe_interposer_windows.dll");
    assert!(p.exists(), "interposer DLL missing at {p:?}");
    p
}

/// Minimal `which` for soldr lookup, avoiding the extra crate dep.
mod which {
    use std::path::PathBuf;
    pub fn which(name: &str) -> Result<PathBuf, ()> {
        let candidates: Vec<String> = if name.ends_with(".exe") {
            vec![name.to_string()]
        } else {
            vec![name.to_string(), format!("{name}.exe")]
        };
        if let Some(path) = std::env::var_os("PATH") {
            for entry in std::env::split_paths(&path) {
                for cand in &candidates {
                    let p = entry.join(cand);
                    if p.is_file() {
                        return Ok(p);
                    }
                }
            }
        }
        Err(())
    }
}

#[test]
fn diagnose_install_progress() {
    let dll = build_interposer_dll();

    // Spawn a long-running child — ping localhost for 8 seconds is
    // enough headroom for all 5 detours to install at the empirical
    // worst-case retour timing (~50 ms each).
    let mut child = Command::new("cmd")
        .args(["/c", "ping -n 8 127.0.0.1 > nul"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ping");
    let pid = child.id();

    // Let cmd start up before we inject.
    std::thread::sleep(Duration::from_millis(200));

    let inject_result = inject_into_pid(pid, &dll);

    // Drain stderr while the child is alive. ping naturally exits
    // ~8 seconds after spawn, which closes stderr and ends the
    // reader thread cleanly.
    let stderr_text: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let stderr_pipe = child.stderr.take().expect("stderr piped");
    let reader_text = Arc::clone(&stderr_text);
    let reader = std::thread::spawn(move || {
        let mut pipe = stderr_pipe;
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut s) = reader_text.lock() {
                        s.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Give detours time to install. Worst-case retour install is
    // <100 ms / detour empirically; budget 3 seconds for the lot.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if stderr_text
            .lock()
            .map(|s| s.contains("install end=MoveFileExW"))
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    let captured = stderr_text.lock().map(|s| s.clone()).unwrap_or_default();
    let hmodule = inject_result.expect("inject_into_pid should succeed");
    assert!(hmodule != 0);

    // Per-detour analysis.
    eprintln!("=== slice 7c install diagnostic ===");
    eprintln!("captured stderr bytes: {}", captured.len());
    eprintln!("---");
    for line in captured.lines() {
        if line.contains("RPP_HOOK") {
            eprintln!("  {line}");
        }
    }
    eprintln!("---");
    for name in [
        "install-thread-start",
        "install begin=CreateFileW",
        "install end=CreateFileW",
        "install begin=WriteFile",
        "install end=WriteFile",
        "install begin=CloseHandle",
        "install end=CloseHandle",
        "install begin=DeleteFileW",
        "install end=DeleteFileW",
        "install begin=MoveFileExW",
        "install end=MoveFileExW",
        "install-thread-done",
    ] {
        let saw = captured.contains(name);
        eprintln!("  [{:^7}] {name}", if saw { "OK" } else { "MISSING" });
    }
    eprintln!("=== end slice 7c install diagnostic ===");
}
