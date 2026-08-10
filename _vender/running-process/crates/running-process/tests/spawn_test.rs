//! Wrapper-mode spawn invariants (issue #113).
//!
//! 1. test_spawn_inherited_stdio_reaches_parent — Pipe → child writes, parent reads.
//! 2. test_spawn_orphans_still_blocked — second inheritable pipe NOT in stdio
//!    stays in parent.
//! 3. test_spawn_force_killed_parent_reaps_child — intermediate parent exits,
//!    grandchild dies.
//! 4. test_spawn_child_exit_bounded_drain — child writes & exits, parent reads
//!    after delay then EOFs.

use std::io::Read;
#[cfg(not(target_os = "macos"))]
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use running_process::{
    spawn, spawn_daemon_with_stdio, DaemonStdio, DaemonStdioSource, SpawnStdio, StdioSource,
};

fn testbin_path(name: &str) -> PathBuf {
    // Fixtures are built once, before the suite runs (see `ci/test.py`).
    //
    // This used to invoke `cargo build -p testbins` on every call. That takes
    // cargo's build-directory lock, and nextest runs each test in its own
    // process, so a full-suite run had dozens of cargo invocations contending
    // for one lock. `Command::output` waits for EOF with no deadline, and
    // cargo's "Blocking waiting for file lock" note went to inherited stderr
    // the harness only shows on failure — so it presented as an unexplained
    // 30s+ hang. See running-process#747 for the symbolized stack.
    let exe = std::env::current_exe().expect("current exe");
    // .../target/<triple>/<profile>/deps/<test-binary>
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("test binary should live in <profile>/deps/");
    let path = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "test fixture `{name}` is missing at {}.
         Build the fixtures first:  soldr cargo build -p testbins",
        path.display()
    );
    path
}

#[cfg(all(windows, not(target_os = "macos")))]
fn is_pid_alive(pid: u32) -> bool {
    unsafe {
        let handle = winapi::um::processthreadsapi::OpenProcess(
            winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok =
            winapi::um::processthreadsapi::GetExitCodeProcess(handle, &mut exit_code as *mut u32);
        winapi::um::handleapi::CloseHandle(handle);
        if ok == 0 {
            return false;
        }
        exit_code == 259
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn is_pid_alive(pid: u32) -> bool {
    unsafe {
        let mut status: i32 = 0;
        let ret = libc::waitpid(pid as i32, &mut status, libc::WNOHANG);
        if ret == pid as i32 {
            return false;
        }
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(target_os = "macos"))]
fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    while is_pid_alive(pid) {
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

#[cfg(all(windows, not(target_os = "macos")))]
fn force_kill(pid: u32) {
    unsafe {
        let handle = winapi::um::processthreadsapi::OpenProcess(
            winapi::um::winnt::PROCESS_TERMINATE,
            0,
            pid,
        );
        if !handle.is_null() {
            winapi::um::processthreadsapi::TerminateProcess(handle, 1);
            winapi::um::handleapi::CloseHandle(handle);
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn force_kill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(not(target_os = "macos"))]
fn parse_pid_line(line: &str, prefix: &str) -> Option<u32> {
    line.strip_prefix(prefix)
        .and_then(|s| s.trim().parse::<u32>().ok())
}

// ── Pipe helper (mirrors containment_test.rs but local to keep tests self-contained) ──

#[cfg(windows)]
mod pipe_helpers {
    use std::os::windows::io::RawHandle;
    use std::time::Duration;

    use winapi::shared::minwindef::{BOOL, DWORD, FALSE, TRUE};
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
    use winapi::um::namedpipeapi::CreatePipe;
    use winapi::um::winnt::HANDLE;

    pub struct InheritablePipe {
        pub read_end: HANDLE,
        pub write_end: HANDLE,
    }

    pub fn create_inheritable_pipe() -> InheritablePipe {
        let mut sa: SECURITY_ATTRIBUTES = unsafe { std::mem::zeroed() };
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD;
        sa.bInheritHandle = TRUE as BOOL;
        sa.lpSecurityDescriptor = std::ptr::null_mut();

        let mut read_end: HANDLE = std::ptr::null_mut();
        let mut write_end: HANDLE = std::ptr::null_mut();
        let ok = unsafe {
            CreatePipe(
                &mut read_end as *mut HANDLE,
                &mut write_end as *mut HANDLE,
                &mut sa as *mut SECURITY_ATTRIBUTES,
                0,
            )
        };
        assert!(ok != FALSE, "CreatePipe failed");
        InheritablePipe {
            read_end,
            write_end,
        }
    }

    pub fn close(h: HANDLE) {
        if !h.is_null() {
            unsafe {
                CloseHandle(h);
            }
        }
    }

    pub fn read_one_byte_with_timeout(
        read_end: HANDLE,
        timeout: Duration,
    ) -> Result<usize, &'static str> {
        let h = read_end as RawHandle as usize;
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<usize>>();
        std::thread::spawn(move || {
            let h = h as RawHandle;
            let mut buf = [0u8; 1];
            let mut got: DWORD = 0;
            let ok = unsafe {
                winapi::um::fileapi::ReadFile(
                    h as HANDLE,
                    buf.as_mut_ptr().cast(),
                    1,
                    &mut got as *mut DWORD,
                    std::ptr::null_mut(),
                )
            };
            if ok == FALSE {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(109) {
                    let _ = tx.send(Ok(0));
                } else {
                    let _ = tx.send(Err(err));
                }
            } else {
                let _ = tx.send(Ok(got as usize));
            }
        });
        match rx.recv_timeout(timeout) {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(_)) => Err("read failed"),
            Err(_) => Err("timed out"),
        }
    }
}

#[cfg(unix)]
mod pipe_helpers {
    use std::os::fd::RawFd;
    use std::time::Duration;

    pub struct InheritablePipe {
        pub read_end: RawFd,
        pub write_end: RawFd,
    }

    pub fn create_inheritable_pipe() -> InheritablePipe {
        let mut fds = [0 as libc::c_int; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert!(rc == 0, "pipe() failed");
        InheritablePipe {
            read_end: fds[0],
            write_end: fds[1],
        }
    }

    pub fn close(fd: RawFd) {
        unsafe {
            libc::close(fd);
        }
    }

    pub fn read_one_byte_with_timeout(
        read_end: RawFd,
        timeout: Duration,
    ) -> Result<usize, &'static str> {
        let mut pfd = libc::pollfd {
            fd: read_end,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        let n = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, ms) };
        if n == 0 {
            return Err("timed out");
        }
        if n < 0 {
            return Err("poll failed");
        }
        let mut buf = [0u8; 1];
        let r = unsafe { libc::read(read_end, buf.as_mut_ptr().cast(), 1) };
        if r < 0 {
            return Err("read failed");
        }
        Ok(r as usize)
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

fn shell_echo_cmd(text: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(format!("echo {text}"));
        cmd
    }
    #[cfg(unix)]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("echo {text}"));
        cmd
    }
}

/// 1. Pipe stdout reaches the parent.
#[test]
fn test_spawn_inherited_stdio_reaches_parent() {
    let mut cmd = shell_echo_cmd("hello");
    let stdio = SpawnStdio {
        stdin: StdioSource::Null,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Null,
        drain_timeout: Some(Duration::from_secs(2)),
        show_console: false,
    };
    let mut child = spawn(&mut cmd, stdio).expect("spawn");
    let mut stdout = child.stdout.take().expect("stdout");

    let mut buf = Vec::new();
    stdout.read_to_end(&mut buf).expect("read");
    let s = String::from_utf8_lossy(&buf);
    assert!(
        s.contains("hello"),
        "expected stdout to contain 'hello', got {s:?}"
    );

    let _ = child.wait();
}

/// 2. An inheritable pipe NOT passed to `stdio` must not appear in the
///    child — closing the parent's copy of the write-end yields EOF on
///    the read-end promptly.
#[test]
fn test_spawn_orphans_still_blocked() {
    use pipe_helpers::*;

    let sleeper = testbin_path("testbin-sleeper");

    // The orphan pipe: created inheritable, never handed to the child.
    let pipe = create_inheritable_pipe();

    // Spawn a long-lived child via the new sanitized path. Default stdio
    // means stdout/stderr inherit (no pipe slot), stdin = NUL.
    let mut cmd = Command::new(&sleeper);
    let mut child = spawn(&mut cmd, SpawnStdio::default()).expect("spawn");
    let child_pid = child.id();

    // Close our copy of the write-end. If the child inherited a dup,
    // the kernel ref-count stays > 0 and read blocks.
    close(pipe.write_end);

    let start = Instant::now();
    let result = read_one_byte_with_timeout(pipe.read_end, Duration::from_secs(2));
    let elapsed = start.elapsed();
    close(pipe.read_end);

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        matches!(result, Ok(0)),
        "expected EOF on orphan read-end, got {result:?} after {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "EOF should be prompt; took {elapsed:?}"
    );
    let _ = child_pid;
}

/// 3. When an intermediate parent (spawned via `spawn`) dies, the
///    grandchild it spawned (also via `spawn`) dies too.
///
/// Skipped on macOS: `PR_SET_PDEATHSIG` is unavailable there, so the
/// grandchild only dies via the wrapper's explicit `Drop` (which our
/// testbin deliberately skips via `mem::forget` to test the OS-level
/// containment). The Job Object (Windows) and Linux PDEATHSIG paths give
/// kernel-driven reaping; macOS would need a polling getppid() loop in
/// the child, which is out of scope for #113.
#[cfg(not(target_os = "macos"))]
#[test]
fn test_spawn_force_killed_parent_reaps_child() {
    let dies_after = testbin_path("testbin-dies-after-spawn");
    let sleeper = testbin_path("testbin-sleeper");

    let mut cmd = Command::new(&dies_after);
    cmd.env("RUNNING_PROCESS_SPAWN_TARGET", &sleeper);
    let stdio = SpawnStdio {
        stdin: StdioSource::Null,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Parent,
        drain_timeout: Some(Duration::from_secs(2)),
        show_console: false,
    };
    let mut child = spawn(&mut cmd, stdio).expect("spawn intermediate");
    let intermediate_pid = child.id();

    let stdout = child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);

    let mut grandchild_pid: Option<u32> = None;
    let start = Instant::now();
    for line in reader.lines() {
        if start.elapsed() > Duration::from_secs(10) {
            panic!("timed out waiting for READY");
        }
        let line = line.expect("read");
        if let Some(pid) = parse_pid_line(&line, "GRANDCHILD_PID=") {
            grandchild_pid = Some(pid);
        } else if line.trim() == "READY" {
            break;
        }
    }
    let grandchild_pid = grandchild_pid.expect("should get grandchild PID");

    // Wait for intermediate to exit (it does so right after READY).
    let _ = child.wait();
    assert!(
        wait_until_dead(intermediate_pid, Duration::from_secs(5)),
        "intermediate parent should exit promptly"
    );

    // Grandchild was spawned via `spawn` inside the intermediate — its
    // Job Object handle (Windows) was leaked via mem::forget, so when
    // the intermediate process exits, the kernel closes its handles and
    // KILL_ON_JOB_CLOSE fires. On Unix, PR_SET_PDEATHSIG fires.
    let dead = wait_until_dead(grandchild_pid, Duration::from_secs(5));

    if !dead {
        // Cleanup before panic.
        force_kill(grandchild_pid);
    }
    assert!(
        dead,
        "grandchild (PID {grandchild_pid}) should die when intermediate parent exits"
    );
}

/// 4. Child writes some bytes then exits. Parent doesn't read for a while;
///    after the drain timeout the queued bytes are still readable, then
///    the next read returns EOF.
#[test]
fn test_spawn_child_exit_bounded_drain() {
    let mut cmd = shell_echo_cmd("done");
    let stdio = SpawnStdio {
        stdin: StdioSource::Null,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Null,
        drain_timeout: Some(Duration::from_secs(2)),
        show_console: false,
    };
    let mut child = spawn(&mut cmd, stdio).expect("spawn");
    let mut stdout = child.stdout.take().expect("stdout");

    // Wait for the child to exit + drain window + slack.
    let _ = child.wait();
    std::thread::sleep(Duration::from_secs(3));

    let mut buf = Vec::new();
    let n = stdout.read_to_end(&mut buf).expect("read after exit");
    let s = String::from_utf8_lossy(&buf);
    assert!(n > 0, "expected queued bytes to be readable post-exit");
    assert!(s.contains("done"), "expected 'done' in stdout, got {s:?}");

    // Subsequent read should immediately return EOF.
    let mut more = [0u8; 16];
    let n2 = stdout.read(&mut more).expect("eof read");
    assert_eq!(n2, 0, "expected EOF on next read, got {n2} bytes");
}

/// Detached daemons may write to caller-owned files without inheriting parent
/// streams or retaining the caller's handles.
#[test]
fn test_spawn_daemon_file_stdio_is_preserved() {
    #[cfg(unix)]
    use std::os::fd::AsFd;
    #[cfg(windows)]
    use std::os::windows::io::AsHandle;

    let log = tempfile::NamedTempFile::new().expect("temp log");
    let stdout = log.reopen().expect("open stdout log");
    let stderr = log.reopen().expect("open stderr log");
    let mut command = shell_echo_cmd("daemon-file-stdio");
    let stdio = DaemonStdio {
        #[cfg(windows)]
        stdout: DaemonStdioSource::Handle(stdout.as_handle()),
        #[cfg(unix)]
        stdout: DaemonStdioSource::Fd(stdout.as_fd()),
        #[cfg(windows)]
        stderr: DaemonStdioSource::Handle(stderr.as_handle()),
        #[cfg(unix)]
        stderr: DaemonStdioSource::Fd(stderr.as_fd()),
    };

    let mut child = spawn_daemon_with_stdio(&mut command, stdio).expect("spawn daemon");
    drop(stdout);
    drop(stderr);
    assert_eq!(child.wait().expect("wait for daemon"), 0);

    let body = std::fs::read_to_string(log.path()).expect("read daemon log");
    assert!(
        body.contains("daemon-file-stdio"),
        "daemon output did not reach its file: {body:?}"
    );
}
