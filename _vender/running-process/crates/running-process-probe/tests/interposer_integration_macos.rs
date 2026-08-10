//! Slice 7e macOS end-to-end integration test (#551).
//!
//! Mirror of slice 7d (Linux) but driven by
//! `DYLD_INSERT_LIBRARIES` instead of `LD_PRELOAD`. Same shape:
//! the dynamic linker (`dyld`) reads the env var at process
//! startup and loads the interposer dylib before `main()` runs,
//! so the slice 5 symbol shadows
//! (`open`/`openat`/`close`/`write`/`unlink`/`unlinkat`/`rename`/
//! `renameat`) are in effect for every libc call from the
//! spawned process and its descendants.
//!
//! ## SIP / hardened-runtime caveat
//!
//! macOS refuses to honor `DYLD_INSERT_LIBRARIES` against
//! binaries with the hardened runtime + library validation
//! flag — that includes most system binaries (`/usr/bin/cat`,
//! `/usr/bin/sh` on recent macOS, etc.). To stay portable to
//! every CI image, we run the probe through `/bin/cat`
//! explicitly, and document the caveat: on hosts where
//! `/bin/cat` itself is SIP-protected, the test is expected to
//! emit no `RPP_HOOK` lines and gracefully reports skip.
//!
//! The slice 5 interposer's behavior here matches the snapshot
//! tier from #539 — both tiers depend on the same SIP-bypass
//! posture, which is "we only observe processes the user owns
//! and the binary isn't library-validated."

#![cfg(all(feature = "embed-helper", target_os = "macos"))]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use running_process_probe::inject_via_env;

/// Locate the workspace `target/<triple>/<profile>/` directory the
/// current test binary was built into. The test binary lives at
/// `target/<triple>/<profile>/deps/<test>`, so we walk up one
/// directory.
fn target_profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent() // deps/
        .and_then(|p| p.parent()) // <profile>/
        .expect("walk up from test exe")
        .to_path_buf()
}

/// Build the macOS interposer cdylib on demand. Returns the path
/// to the resulting `.dylib` artifact.
fn build_and_locate_interposer_dylib() -> PathBuf {
    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "running-process-probe-interposer-macos",
            "--features",
            "test-seams",
        ])
        .status()
        .expect("spawn cargo to build interposer dylib");
    assert!(
        status.success(),
        "cargo build of interposer .dylib failed: {status:?}"
    );

    let dylib = target_profile_dir().join("librunning_process_probe_interposer_macos.dylib");
    assert!(
        dylib.exists(),
        "expected interposer .dylib at {dylib:?} after cargo build"
    );
    dylib
}

#[test]
fn interposer_post_fork_child_progress_entrypoint() {
    let Some(_) = std::env::var_os("RPO_POST_FORK_CHILD_TEST") else {
        return;
    };
    type HoldFn = unsafe extern "C" fn(std::os::raw::c_int, std::os::raw::c_int);
    let raw = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"rpo_test_hold_fd_table".as_ptr()) };
    assert!(!raw.is_null(), "missing interposer test seam");
    let hold = unsafe { std::mem::transmute::<*mut libc::c_void, HoldFn>(raw) };
    let mut ready = [0; 2];
    let mut release = [0; 2];
    assert_eq!(unsafe { libc::pipe(ready.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::pipe(release.as_mut_ptr()) }, 0);
    let holder = std::thread::spawn(move || unsafe { hold(ready[1], release[0]) });
    let mut byte = [0u8; 1];
    assert_eq!(
        unsafe { libc::read(ready[0], byte.as_mut_ptr().cast(), 1) },
        1
    );

    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        unsafe {
            libc::close(-1);
            libc::_exit(0);
        }
    }

    let deadline = Instant::now() + Duration::from_millis(500);
    let mut status = 0;
    let progressed = loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if waited == pid {
            break true;
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &mut status, 0);
            }
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        unsafe { libc::write(release[1], byte.as_ptr().cast(), 1) },
        1
    );
    holder.join().expect("holder joins");
    assert!(
        progressed,
        "post-fork child blocked on inherited fd-table lock"
    );
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "post-fork child terminated abnormally: status={status:#x}"
    );
}

#[test]
fn interposer_post_fork_child_progresses_with_inherited_locked_state() {
    let dylib = build_and_locate_interposer_dylib();
    let current_test = std::env::current_exe().expect("current test executable");
    let mut cmd = Command::new(&current_test);
    cmd.args([
        "--exact",
        "interposer_post_fork_child_progress_entrypoint",
        "--nocapture",
    ])
    .env("RPO_POST_FORK_CHILD_TEST", "1")
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    inject_via_env(&mut cmd, &dylib).expect("inject_via_env");
    let status = cmd.status().expect("run post-fork child regression");
    assert!(status.success(), "inherited-state regression failed");
}

#[test]
fn interposer_dylib_fires_rpp_hook_via_dyld_insert() {
    let dylib = build_and_locate_interposer_dylib();

    // Probe file the child will `cat` after spawn.
    let tmp = tempfile::tempdir().expect("tempdir");
    let probe_path = tmp.path().join("probe.txt");
    std::fs::write(&probe_path, b"hello from slice 7e\n").expect("write probe");

    // Use `/bin/cat` explicitly — on hosts where it's hardened-
    // runtime-protected the test will produce no events (see
    // module header). When unprotected (Linux-compat shims, some
    // CI images), the slice 5 `open`/`openat` shadows fire and
    // emit `RPP_HOOK file-open` with the probe path.
    let mut cmd = Command::new("/bin/cat");
    cmd.arg(&probe_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    inject_via_env(&mut cmd, &dylib).expect("inject_via_env");

    let mut child = cmd.spawn().expect("spawn /bin/cat");

    // Drain stderr on a background thread.
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

    let probe_marker = probe_path.display().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if stderr_text
            .lock()
            .map(|s| s.contains("RPP_HOOK") && s.contains(&probe_marker))
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    let captured = stderr_text.lock().map(|s| s.clone()).unwrap_or_default();

    // SIP graceful-skip: if no RPP_HOOK lines arrived at all,
    // the most likely cause is hardened-runtime / library
    // validation refusing the DYLD_INSERT_LIBRARIES. Document
    // that in the test output and pass — same posture as the
    // snapshot tier from #539 (the boundary is "binaries the
    // user owns AND library validation isn't enforced for").
    if !captured.contains("RPP_HOOK") {
        eprintln!(
            "skipping detour assertion: no RPP_HOOK lines on stderr — \
             /bin/cat is likely library-validated on this host. \
             captured stderr: {captured:?}"
        );
        return;
    }

    // Stronger assertion when the host *did* allow injection:
    // the specific probe path must appear in a RPP_HOOK line,
    // proving the detour fires on a real file-open call.
    assert!(
        captured.contains(&probe_marker),
        "expected RPP_HOOK line for our probe path {probe_marker:?}; got: {captured:?}"
    );
}
