//! Slice 7d Linux end-to-end integration test (#551).
//!
//! Mirror of `interposer_integration_windows.rs` but driven by
//! `LD_PRELOAD` instead of `CreateRemoteThread`. Linux doesn't
//! need an injection vehicle — the dynamic linker reads the env
//! var at process startup and loads the named shared library
//! before `main()` runs, so the interposer's symbol shadows
//! (slice 4 of #551) are in effect for every libc call from
//! the spawned process and its descendants.
//!
//! Test scenario:
//!
//! 1. Build the interposer cdylib (slice 4 artifact).
//! 2. Write a probe file in a tempdir.
//! 3. Spawn `sh -c "cat probe.txt"` with the env var
//!    [`inject_env_name`] (resolves to `LD_PRELOAD`) set to
//!    the interposer's path, via [`inject_via_env`] (slice 6e).
//! 4. Capture the child's stderr on a background thread.
//! 5. Assert at least one `RPP_HOOK file-open` line appears with
//!    the probe path.
//!
//! Why we can assert the *specific* path on Linux but not on
//! Windows (slice 7a/7b): `cat`'s implementation goes through
//! glibc's `open(2)` / `openat(2)`, which our slice 4 interposer
//! `dlsym(RTLD_NEXT, ...)`-shadows directly. The Windows
//! equivalent (cmd's `type` builtin) doesn't appear to use
//! `kernel32!CreateFileW` — that's the slice 7c follow-up.

#![cfg(all(feature = "embed-helper", target_os = "linux"))]

use std::ffi::CString;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use running_process_probe::inject_via_env;

unsafe extern "C" {
    fn open(path: *const std::os::raw::c_char, flags: std::os::raw::c_int) -> std::os::raw::c_int;
    fn close(fd: std::os::raw::c_int) -> std::os::raw::c_int;
}

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

/// Build the Linux interposer cdylib on demand. Returns the path
/// to the resulting `.so` artifact.
fn build_and_locate_interposer_so() -> PathBuf {
    if let Some(path) = std::env::var_os("RPO_TEST_INTERPOSER_SO") {
        let so = PathBuf::from(path);
        assert!(so.exists(), "RPO_TEST_INTERPOSER_SO does not exist: {so:?}");
        return so;
    }

    let status = Command::new("cargo")
        .args([
            "build",
            "-p",
            "running-process-probe-interposer-linux",
            "--features",
            "test-seams",
        ])
        .status()
        .expect("spawn cargo to build interposer so");
    assert!(
        status.success(),
        "cargo build of interposer .so failed: {status:?}"
    );

    let so = target_profile_dir().join("librunning_process_probe_interposer_linux.so");
    assert!(
        so.exists(),
        "expected interposer .so at {so:?} after cargo build"
    );
    so
}

#[test]
fn interposer_so_fires_rpp_hook_via_ld_preload() {
    let so = build_and_locate_interposer_so();

    // Probe file the child will `cat` after spawn. Tempdir so we
    // don't litter the source tree, and so parallel test runs
    // don't race on a shared path.
    let tmp = tempfile::tempdir().expect("tempdir");
    let probe_path = tmp.path().join("probe.txt");
    std::fs::write(&probe_path, b"hello from slice 7d\n").expect("write probe");

    // Spawn `sh -c "cat <probe>"` with LD_PRELOAD set to our
    // interposer. The dynamic linker injects the .so at sh's
    // startup; sh then execs `cat`, which inherits the env var
    // and gets the interposer too. cat's `open(2)` call goes
    // through our shadow.
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("cat {}", probe_path.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    inject_via_env(&mut cmd, &so).expect("inject_via_env");

    let mut child = cmd.spawn().expect("spawn sh+cat");

    // Drain stderr on a background thread so the deadline below
    // is enforced even if the child stalls between writes.
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

    // Wait for the probe-path RPP_HOOK or the deadline.
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

    // Let cat finish on its own (it's fast); if it hasn't,
    // tear it down so the reader thread exits.
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();

    let captured = stderr_text.lock().map(|s| s.clone()).unwrap_or_default();
    assert!(
        captured.contains("RPP_HOOK"),
        "expected at least one RPP_HOOK line on the child's stderr; got: {captured:?}"
    );
    // Stronger assertion than Windows can make today: we want to
    // see our specific probe path, proving the detour fires on a
    // real (non-diagnostic) file-open call.
    assert!(
        captured.contains(&probe_marker),
        "expected RPP_HOOK line for our probe path {probe_marker:?}; got: {captured:?}"
    );
}

/// Child entrypoint for [`interposer_hook_does_not_block_when_stderr_is_full`].
///
/// Calling libc directly guarantees that every iteration reaches the
/// interposer's `open` and `close` shadows. The parent deliberately leaves
/// this process's stderr pipe undrained.
#[test]
fn interposer_stderr_saturation_child() {
    if std::env::var_os("RPO_STDERR_SATURATION_CHILD").is_none() {
        return;
    }

    let path = CString::new("/dev/null").expect("static path has no NUL");
    for _ in 0..10_000 {
        // SAFETY: `path` is a valid NUL-terminated string and every successful
        // descriptor is closed exactly once.
        let fd = unsafe { open(path.as_ptr(), 0) };
        assert!(fd >= 0, "open /dev/null failed");
        // SAFETY: `fd` was returned by the successful open immediately above.
        assert_eq!(unsafe { close(fd) }, 0, "close /dev/null failed");
    }
}

#[test]
fn interposer_hook_does_not_block_when_stderr_is_full() {
    // Regression for #605: hook telemetry must be lossy under backpressure.
    let so = build_and_locate_interposer_so();
    let current_test = std::env::current_exe().expect("current test executable");
    let mut cmd = Command::new(current_test);
    cmd.args([
        "--exact",
        "interposer_stderr_saturation_child",
        "--nocapture",
    ])
    .env("RPO_STDERR_SATURATION_CHILD", "1")
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    inject_via_env(&mut cmd, &so).expect("inject_via_env");

    let mut child = cmd.spawn().expect("spawn saturation child");
    // Keep the pipe handle alive but intentionally never read it. The emitted
    // events exceed normal pipe capacity by more than an order of magnitude.
    let _undrained_stderr = child.stderr.take().expect("stderr piped");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("poll saturation child") {
            Some(status) => {
                assert!(status.success(), "saturation child failed: {status}");
                break;
            }
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("interposer blocked a hooked file operation after its stderr pipe filled");
            }
        }
    }
}

#[test]
fn interposer_post_fork_child_progress_entrypoint() {
    let Some(mode) = std::env::var_os("RPO_POST_FORK_CHILD_MODE") else {
        return;
    };
    type HoldFn = unsafe extern "C" fn(std::os::raw::c_int, std::os::raw::c_int);
    let symbol = if mode == "fd" {
        c"rpo_test_hold_fd_table"
    } else {
        c"rpo_test_hold_renameat_resolver_init"
    };
    let raw = unsafe { libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr()) };
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
        if mode == "fd" {
            unsafe { libc::close(-1) };
        } else {
            let missing = c"/rpo-post-fork-missing";
            unsafe {
                libc::renameat(
                    libc::AT_FDCWD,
                    missing.as_ptr(),
                    libc::AT_FDCWD,
                    missing.as_ptr(),
                );
            }
        }
        unsafe { libc::_exit(0) };
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
        "post-fork child blocked in {mode:?} interposer state"
    );
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "post-fork child terminated abnormally in {mode:?}: status={status:#x}"
    );
}

#[test]
fn interposer_post_fork_child_progresses_with_inherited_locked_state() {
    let so = build_and_locate_interposer_so();
    let current_test = std::env::current_exe().expect("current test executable");
    for mode in ["fd", "resolver"] {
        let mut cmd = Command::new(&current_test);
        cmd.args([
            "--exact",
            "interposer_post_fork_child_progress_entrypoint",
            "--nocapture",
        ])
        .env("RPO_POST_FORK_CHILD_MODE", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        inject_via_env(&mut cmd, &so).expect("inject_via_env");
        let status = cmd.status().expect("run post-fork child regression");
        assert!(status.success(), "{mode} inherited-state regression failed");
    }
}
