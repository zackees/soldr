//! Slice 7a end-to-end integration test (#551).
//!
//! Wires the slice 6 pieces together: build the interposer DLL,
//! spawn a real child process, inject the DLL via the slice 6d
//! injection vehicle, then assert that the detours installed in
//! `DllMain` actually fire — `RPP_HOOK …` lines must appear on
//! the child's stderr after the inject returns.
//!
//! This is the first test that exercises the full pipeline
//! end-to-end. Each prior slice has its own unit tests for the
//! pieces; slice 7 is the contract:
//!
//! 1. Building the interposer crate produces a loadable DLL.
//! 2. The injector loads it into a target.
//! 3. The detours installed in `DllMain` actually intercept
//!    subsequent file APIs in the target's address space.
//! 4. The intercepted events arrive on the child's stderr in the
//!    documented `RPP_HOOK` format.

#![cfg(all(feature = "embed-helper", target_os = "windows"))]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use running_process_probe::inject_into_pid;

/// Locate the workspace `target/<triple>/<profile>/` directory the
/// current test binary was built into. The test binary lives at
/// `target/<triple>/<profile>/deps/<test>.exe`, so we walk up one
/// directory.
fn target_profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent() // deps/
        .and_then(|p| p.parent()) // <profile>/
        .expect("walk up from test exe")
        .to_path_buf()
}

fn target_triple_for_profile_dir(profile_dir: &Path) -> Option<String> {
    let triple_dir = profile_dir.parent()?;
    let triple = triple_dir.file_name()?.to_str()?;
    if triple.contains("-pc-windows-") {
        Some(triple.to_string())
    } else {
        None
    }
}

fn add_current_test_target_flags(cmd: &mut Command, profile_dir: &Path) {
    if profile_dir
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s == "release")
    {
        cmd.arg("--release");
    }
    if let Some(triple) = target_triple_for_profile_dir(profile_dir) {
        cmd.arg("--target").arg(triple);
    }
}

fn cargo_command() -> Command {
    if let Some(soldr) = which("soldr") {
        if let Ok(output) = Command::new(soldr)
            .args(["rustup", "which", "cargo"])
            .output()
        {
            if output.status.success() {
                let cargo = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !cargo.is_empty() {
                    return Command::new(cargo);
                }
            }
        }
    }
    Command::new("cargo")
}

/// Build the Windows interposer DLL on demand. Returns its path.
///
/// Shells out to `cargo build -p running-process-probe-
/// interposer-windows`. The first run rebuilds; subsequent runs
/// are no-ops because cargo's incremental checks find no changes.
/// Either way we end up with the DLL at
/// `<target>/<profile>/running_process_probe_interposer_windows.dll`.
fn build_and_locate_interposer_dll() -> PathBuf {
    let profile_dir = target_profile_dir();
    let mut cmd = cargo_command();
    cmd.args(["build", "-p", "running-process-probe-interposer-windows"]);
    add_current_test_target_flags(&mut cmd, &profile_dir);
    let status = cmd.status().expect("spawn cargo to build interposer dll");
    assert!(
        status.success(),
        "cargo build of interposer DLL failed: {status:?}"
    );

    let dll = profile_dir.join("running_process_probe_interposer_windows.dll");
    assert!(
        dll.exists(),
        "expected interposer DLL at {dll:?} after cargo build"
    );
    dll
}

fn build_createfilew_probe() {
    let profile_dir = target_profile_dir();
    let mut cmd = cargo_command();
    cmd.args([
        "build",
        "-p",
        "testbins",
        "--bin",
        "testbin-createfilew-probe",
    ]);
    add_current_test_target_flags(&mut cmd, &profile_dir);
    let status = cmd
        .status()
        .expect("spawn cargo to build testbin-createfilew-probe");
    assert!(
        status.success(),
        "cargo build of testbin-createfilew-probe failed: {status:?}"
    );
}

/// Lightweight `which` replacement for Windows. Returns the first
/// matching path in `PATH` if `name` (or `name.exe`) resolves.
fn which(name: &str) -> Option<PathBuf> {
    let candidates: Vec<String> = if name.ends_with(".exe") {
        vec![name.to_string()]
    } else {
        vec![name.to_string(), format!("{name}.exe")]
    };
    let path = std::env::var_os("PATH")?;
    for entry in std::env::split_paths(&path) {
        for cand in &candidates {
            let p = entry.join(cand);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

// Slice 7c (resolved): the slice 6b detour fires correctly on
// `kernel32!CreateFileW`. The earlier hang was a self-inflicted
// assertion bug — the interposer formats paths via `{:?}` which
// debug-escapes backslashes, so `contains(probe_path)` against
// the raw path never matched and the 10-second deadline ran to
// completion. Fix: assert on the unambiguous tail substring
// `probe.txt` instead, and use the same substring as the
// deadline-poll exit condition so the test exits early on
// success.
#[test]
fn interposer_dll_fires_rpp_hook_after_inject() {
    let dll = build_and_locate_interposer_dll();
    build_createfilew_probe();

    // Probe file the slice 7c fixture (testbin-createfilew-probe)
    // will explicitly open via kernel32!CreateFileW — the exact
    // entry point our slice 6b detour patches.
    let tmp = tempfile::tempdir().expect("tempdir");
    let probe_path = tmp.path().join("probe.txt");
    std::fs::write(&probe_path, b"hello from slice 7\n").expect("write probe");

    // Locate the testbin in the same target/<triple>/<profile>/
    // directory as our test executable.
    let fixture = target_profile_dir().join("testbin-createfilew-probe.exe");
    assert!(
        fixture.exists(),
        "testbin-createfilew-probe not built — \
         run `cargo build -p testbins --bin testbin-createfilew-probe` \
         (or rely on a workspace-wide `cargo build` having done so). \
         expected at {fixture:?}"
    );

    // 2000 ms delay so the inject + worker-thread install (~200 ms
    // in practice) lands comfortably before the fixture calls
    // CreateFileW. The slice 6b detour intercepts that call and
    // emits `RPP_HOOK file-open path=<probe_path> ...` on stderr.
    let mut child = Command::new(&fixture)
        .arg("2000")
        .arg(&probe_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn testbin-createfilew-probe");
    let pid = child.id();

    // Give cmd a moment to start its first child (the ping). We
    // want to inject WHILE ping is sleeping, so the subsequent
    // `type` runs with the interposer already attached.
    std::thread::sleep(Duration::from_millis(200));

    let inject_result = inject_into_pid(pid, &dll);

    // Slice 7b: DllMain returns immediately, then a worker thread
    // installs the retour detours off the loader lock. Give that
    // thread time to finish before we expect any RPP_HOOK output.
    // 200 ms is comfortably more than the empirical worst case
    // (retour install measures ~30 ms per detour × 5 detours, with
    // VirtualProtect overhead).
    std::thread::sleep(Duration::from_millis(200));

    // Drain stderr on a background thread so the main thread can
    // enforce a wall-clock deadline. `Child::stderr.read()` is
    // synchronous + blocks until the child writes or closes; if we
    // ran it inline the deadline check would only fire between
    // reads, and a stuck child would block us until the next byte
    // arrives.
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

    // Wait until we observe a `RPP_HOOK file-open` line for our
    // probe file OR the deadline hits. The interposer formats paths
    // via `{:?}` which doubles backslashes, so we can't match on
    // the raw probe path. Match on the unambiguous tail substring
    // `file-open path=` + `probe.txt` instead.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if stderr_text
            .lock()
            .map(|s| s.contains("RPP_HOOK file-open") && s.contains("probe.txt"))
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Tear the child down so the reader thread exits.
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    let hmodule = inject_result.expect("inject_into_pid should succeed");
    assert!(hmodule != 0, "remote LoadLibraryW returned NULL");

    // The interposer's detours emit one or more `RPP_HOOK ...`
    // lines from inside cmd's address space (its CreateFileW
    // calls for the `type` builtin, plus any incidental file I/O
    // cmd.exe does itself). We just need to see one.
    let captured = stderr_text.lock().map(|s| s.clone()).unwrap_or_default();
    assert!(
        captured.contains("RPP_HOOK"),
        "expected at least one RPP_HOOK line on the child's stderr; \
         got: {captured:?}"
    );

    // Slice 7c: stronger assertion than the original 7a/7b shape.
    // The testbin-createfilew-probe fixture calls
    // kernel32!CreateFileW directly with our probe path, so the
    // slice 6b detour fires and produces an `RPP_HOOK file-open
    // path=...probe.txt... ...` line. We assert both the
    // `file-open` event kind and the unambiguous `probe.txt`
    // basename appear — proves the detour intercepts real file
    // APIs (not just the install-thread diagnostic sentinels).
    //
    // We don't match on the full probe path because the interposer
    // formats via `{:?}` which doubles backslashes. The basename
    // alone is unambiguous since the fixture only ever opens that
    // file.
    assert!(
        captured.contains("RPP_HOOK file-open") && captured.contains("probe.txt"),
        "expected `RPP_HOOK file-open path=...probe.txt...` after \
         the detoured CreateFileW call; got: {captured:?}"
    );
}
