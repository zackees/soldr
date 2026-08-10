//! Integration tests for `ContainedProcessGroup`.
//!
//! These tests spawn real processes and verify that containment works:
//! - Contained children die when the group is dropped (and when the
//!   returned `SpawnedChild` is dropped).
//! - Grandchildren (spawned by children) also die.
//! - Daemon children survive the group being dropped.
//! - Daemon spawns do not duplicate orphaned inheritable handles from the
//!   parent into the child (issue #110).
//!
//! Note on the v4 API: there's no longer a "default leaky spawn" path to
//! use as a control test — every API entry point (`spawn`, `spawn_daemon`)
//! goes through the sanitized handle-list machinery. The previous
//! `test_default_spawn_leaks_inheritable_handles_control` test has been
//! removed because the property it guarded (that the sanitized test was
//! meaningful) is now structurally guaranteed by the API surface.
//!
//! Run with `--test-threads=1` on Windows to avoid Job Object conflicts.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use running_process::{ContainedProcessGroup, SpawnStdio, StdioSource};

/// Build (if needed) and locate a test binary from the workspace.
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

#[cfg(windows)]
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

#[cfg(unix)]
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

fn wait_until_dead(pid: u32, timeout: Duration) {
    let start = Instant::now();
    while is_pid_alive(pid) {
        if start.elapsed() > timeout {
            panic!("PID {pid} still alive after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
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

#[cfg(unix)]
fn force_kill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

fn parse_pid_line(line: &str, prefix: &str) -> Option<u32> {
    line.strip_prefix(prefix)
        .and_then(|s| s.trim().parse::<u32>().ok())
}

fn pipe_stdio() -> SpawnStdio<'static> {
    SpawnStdio {
        stdin: StdioSource::Null,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Parent,
        drain_timeout: Some(Duration::from_secs(5)),
        show_console: false,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn test_contained_group_kills_on_drop() {
    let sleeper = testbin_path("testbin-sleeper");
    let group = ContainedProcessGroup::new().expect("create group");

    let mut cmd1 = Command::new(&sleeper);
    let child1 = group.spawn(&mut cmd1, pipe_stdio()).expect("spawn 1");
    let pid1 = child1.id();

    let mut cmd2 = Command::new(&sleeper);
    let child2 = group.spawn(&mut cmd2, pipe_stdio()).expect("spawn 2");
    let pid2 = child2.id();

    std::thread::sleep(Duration::from_millis(200));

    assert!(
        is_pid_alive(pid1),
        "child 1 (PID {pid1}) should be alive after spawn"
    );
    assert!(
        is_pid_alive(pid2),
        "child 2 (PID {pid2}) should be alive after spawn"
    );

    // Drop the children — each has its own Job Object (Windows) /
    // process group (Unix), so dropping kills them.
    drop(child1);
    drop(child2);
    drop(group);

    let timeout = Duration::from_secs(10);
    wait_until_dead(pid1, timeout);
    wait_until_dead(pid2, timeout);
}

#[test]
fn test_contained_group_kills_grandchildren() {
    let _watchdog = test_watchdog::install(
        Duration::from_secs(30),
        "test_contained_group_kills_grandchildren appears to be hung",
        None,
    );

    let sleeper = testbin_path("testbin-sleeper");
    let spawner = testbin_path("testbin-spawner");
    let group = ContainedProcessGroup::new().expect("create group");

    let mut cmd = Command::new(&spawner);
    cmd.arg("2").arg(&sleeper);
    let mut child = group.spawn(&mut cmd, pipe_stdio()).expect("spawn spawner");

    // Read on a worker thread + channel + recv_timeout so a regression
    // that reintroduces the pipe inheritance leak or the
    // sync-pipe-on-alertable-read footgun (#115) panics with a useful
    // message instead of hanging forever inside `ReadFile`.
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<String>>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let stop = line.is_err();
            if tx.send(line).is_err() || stop {
                break;
            }
        }
    });

    let mut spawner_pid: Option<u32> = None;
    let mut grandchild_pids: Vec<u32> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = match rx.recv_timeout(remaining) {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => panic!("error reading spawner stdout: {e}"),
            Err(_) => panic!(
                "timed out reading spawner output (no READY within 10s) — \
                 likely an orphaned pipe write-end keeping stdout open \
                 (issue #115)"
            ),
        };
        if let Some(pid) = parse_pid_line(&line, "SPAWNER_PID=") {
            spawner_pid = Some(pid);
        } else if let Some(pid) = parse_pid_line(&line, "CHILD_PID=") {
            grandchild_pids.push(pid);
        } else if line.trim() == "READY" {
            break;
        }
    }

    let spawner_pid = spawner_pid.expect("spawner should print its PID");
    assert_eq!(
        grandchild_pids.len(),
        2,
        "spawner should have spawned 2 children"
    );

    assert!(is_pid_alive(spawner_pid), "spawner should be alive");
    for &pid in &grandchild_pids {
        assert!(is_pid_alive(pid), "grandchild {pid} should be alive");
    }

    drop(child);
    drop(group);

    let timeout = Duration::from_secs(10);
    wait_until_dead(spawner_pid, timeout);
    for &pid in &grandchild_pids {
        wait_until_dead(pid, timeout);
    }
}

#[test]
fn test_local_kill_tree_kills_root_and_grandchildren() {
    let sleeper = testbin_path("testbin-sleeper");
    let spawner = testbin_path("testbin-spawner");

    let mut child = Command::new(&spawner)
        .arg("2")
        .arg(&sleeper)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn raw process-tree fixture");
    let stdout = child.stdout.take().expect("spawner stdout");
    let reader = BufReader::new(stdout);

    let mut root_pid = None;
    let mut descendant_pids = Vec::new();
    for line in reader.lines() {
        let line = line.expect("read spawner output");
        if let Some(pid) = parse_pid_line(&line, "SPAWNER_PID=") {
            root_pid = Some(pid);
        } else if let Some(pid) = parse_pid_line(&line, "CHILD_PID=") {
            descendant_pids.push(pid);
        } else if line.trim() == "READY" {
            break;
        }
    }

    let root_pid = root_pid.expect("spawner PID");
    assert_eq!(descendant_pids.len(), 2);
    let killed = running_process::process_tree::kill_tree(root_pid, Duration::from_secs(5))
        .expect("kill local process tree");
    assert!(killed >= 3, "expected root + 2 descendants, got {killed}");

    let _ = child.wait();
    wait_until_dead(root_pid, Duration::from_secs(5));
    for pid in descendant_pids {
        wait_until_dead(pid, Duration::from_secs(5));
    }
}

#[test]
fn test_auto_contained_spawn_inherits_parent_environment() {
    let env_dump = testbin_path("testbin-env-dump");
    let temp = tempfile::tempdir().expect("tempdir");
    let output = temp.path().join("contained-env.txt");
    let key = format!("RUNNING_PROCESS_TEST_PARENT_ONLY_{}", std::process::id());

    std::env::set_var(&key, "inherited");
    let mut command = Command::new(env_dump);
    command.arg(&output);
    let mut child =
        running_process::spawn(&mut command, SpawnStdio::default()).expect("spawn contained");
    let exit = child.wait().expect("wait contained");
    std::env::remove_var(&key);

    assert_eq!(exit, 0);
    let env = std::fs::read_to_string(output).expect("read child environment");
    assert!(
        env.lines().any(|line| line == format!("{key}=inherited")),
        "Auto contained spawn should inherit the parent environment"
    );
}

#[test]
fn test_auto_daemon_spawn_uses_user_baseline_environment() {
    let env_dump = testbin_path("testbin-env-dump");
    let temp = tempfile::tempdir().expect("tempdir");
    let output = temp.path().join("daemon-env.txt");
    let key = format!("RUNNING_PROCESS_TEST_PARENT_ONLY_{}", std::process::id());

    std::env::set_var(&key, "must-not-leak");
    let mut command = Command::new(env_dump);
    command
        .arg(&output)
        .env("EXPLICIT_DAEMON_VALUE", "preserved");
    let mut child = running_process::spawn_daemon(&mut command).expect("spawn daemon");
    let exit = child.wait().expect("wait daemon");
    std::env::remove_var(&key);

    assert_eq!(exit, 0);
    let env = std::fs::read_to_string(output).expect("read daemon environment");
    assert!(
        !env.lines().any(|line| line.starts_with(&format!("{key}="))),
        "daemon inherited a parent-only environment variable"
    );
    assert!(
        env.lines()
            .any(|line| line == "EXPLICIT_DAEMON_VALUE=preserved"),
        "explicit Command::env override was lost"
    );
    #[cfg(windows)]
    {
        assert!(
            env.lines()
                .any(|line| line.to_ascii_uppercase().starts_with("SYSTEMROOT=")),
            "user baseline should contain SystemRoot"
        );
        assert!(
            env.lines()
                .any(|line| line.to_ascii_uppercase().starts_with("USERPROFILE=")),
            "user baseline should contain USERPROFILE"
        );
    }
    #[cfg(unix)]
    {
        let child_env: std::collections::HashMap<_, _> = env
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        let expected: std::collections::HashMap<_, _> =
            running_process::environment::user_baseline_environment()
                .expect("build expected user baseline")
                .into_iter()
                .filter_map(|(key, value)| {
                    Some((key.into_string().ok()?, value.into_string().ok()?))
                })
                .collect();

        for key in ["USER", "LOGNAME", "HOME", "PATH"] {
            let actual = child_env
                .get(key)
                .unwrap_or_else(|| panic!("spawned daemon baseline should contain {key}"));
            assert!(!actual.is_empty(), "{key} should be non-empty");
            assert_eq!(
                Some(*actual),
                expected.get(key).map(String::as_str),
                "spawned daemon {key} should match the Unix login baseline"
            );
        }
        assert_eq!(
            child_env.get("USER"),
            child_env.get("LOGNAME"),
            "Unix baseline USER and LOGNAME should identify the same login"
        );
    }
}

#[test]
fn test_user_baseline_explicit_overrides_and_removals_win() {
    let env_dump = testbin_path("testbin-env-dump");
    let temp = tempfile::tempdir().expect("tempdir");
    let output = temp.path().join("baseline-overrides-env.txt");
    #[cfg(unix)]
    let removed_key = "HOME";
    #[cfg(windows)]
    let removed_key = "USERPROFILE";

    let mut command = Command::new(env_dump);
    command
        .arg(&output)
        .env("PATH", "explicit-path-wins")
        .env_remove(removed_key);
    let mut child = running_process::spawn_daemon_with_env_policy(
        &mut command,
        running_process::EnvironmentPolicy::UserBaseline,
    )
    .expect("spawn daemon with explicit UserBaseline");
    let exit = child.wait().expect("wait daemon");

    assert_eq!(exit, 0);
    let env = std::fs::read_to_string(output).expect("read daemon environment");
    assert!(
        env.lines().any(|line| {
            line.split_once('=').is_some_and(|(key, value)| {
                key.eq_ignore_ascii_case("PATH") && value == "explicit-path-wins"
            })
        }),
        "explicit PATH should replace the user baseline PATH"
    );
    assert!(
        !env.lines().any(|line| {
            line.split_once('=')
                .is_some_and(|(key, _)| key.eq_ignore_ascii_case(removed_key))
        }),
        "explicit env_remove({removed_key}) should remove the baseline value"
    );
}

#[test]
fn test_explicit_inherit_daemon_preserves_parent_environment() {
    let env_dump = testbin_path("testbin-env-dump");
    let temp = tempfile::tempdir().expect("tempdir");
    let output = temp.path().join("inherit-env.txt");
    let key = format!(
        "RUNNING_PROCESS_TEST_EXPLICIT_INHERIT_{}",
        std::process::id()
    );

    std::env::set_var(&key, "inherited");
    let mut command = Command::new(env_dump);
    command.arg(&output);
    let mut child = running_process::spawn_daemon_with_env_policy(
        &mut command,
        running_process::EnvironmentPolicy::Inherit,
    )
    .expect("spawn daemon with explicit Inherit");
    let exit = child.wait().expect("wait daemon");
    std::env::remove_var(&key);

    assert_eq!(exit, 0);
    let env = std::fs::read_to_string(output).expect("read daemon environment");
    assert!(
        env.lines().any(|line| line == format!("{key}=inherited")),
        "explicit Inherit should preserve the parent environment"
    );
}

#[test]
fn test_clear_daemon_keeps_explicit_environment_only() {
    let env_dump = testbin_path("testbin-env-dump");
    let temp = tempfile::tempdir().expect("tempdir");
    let output = temp.path().join("clear-env.txt");
    let key = format!("RUNNING_PROCESS_TEST_CLEAR_PARENT_{}", std::process::id());

    std::env::set_var(&key, "must-not-leak");
    let mut command = Command::new(env_dump);
    command
        .arg(&output)
        .env("EXPLICIT_CLEAR_VALUE", "preserved");
    let mut child = running_process::spawn_daemon_with_env_policy(
        &mut command,
        running_process::EnvironmentPolicy::Clear,
    )
    .expect("spawn daemon with Clear");
    let exit = child.wait().expect("wait daemon");
    std::env::remove_var(&key);

    assert_eq!(exit, 0);
    let env = std::fs::read_to_string(output).expect("read daemon environment");
    assert!(
        !env.lines().any(|line| line.starts_with(&format!("{key}="))),
        "Clear should discard the parent environment"
    );
    assert!(
        env.lines()
            .any(|line| line == "EXPLICIT_CLEAR_VALUE=preserved"),
        "Clear should preserve explicit Command::env additions"
    );
}

#[test]
fn test_daemon_survives_group_drop() {
    let sleeper = testbin_path("testbin-sleeper");
    let group = ContainedProcessGroup::new().expect("create group");

    // Spawn a contained child (dies when SpawnedChild drops).
    let mut cmd_contained = Command::new(&sleeper);
    let contained = group
        .spawn(&mut cmd_contained, pipe_stdio())
        .expect("spawn contained");
    let contained_pid = contained.id();

    // Spawn a daemon child (survives group drop).
    let mut cmd_daemon = Command::new(&sleeper);
    let mut daemon = group.spawn_daemon(&mut cmd_daemon).expect("spawn daemon");
    let daemon_pid = daemon.id();

    std::thread::sleep(Duration::from_millis(200));

    assert!(
        is_pid_alive(contained_pid),
        "contained child should be alive"
    );
    assert!(is_pid_alive(daemon_pid), "daemon child should be alive");

    drop(contained);
    drop(group);

    // Contained child should die.
    wait_until_dead(contained_pid, Duration::from_secs(10));

    // Daemon child should still be alive.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        is_pid_alive(daemon_pid),
        "daemon child (PID {daemon_pid}) should survive group drop"
    );

    // Clean up.
    let _ = daemon.kill();
    wait_until_dead(daemon_pid, Duration::from_secs(5));
}

// ── Sanitized-spawn handle-leak tests (issue #110) ──────────────────────────

#[cfg(windows)]
mod sanitized_pipe_helpers {
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
mod sanitized_pipe_helpers {
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

/// `spawn_daemon` must not duplicate orphaned inheritable handles from the
/// parent's table into the child.
///
/// Issue: zackees/running-process#110.
#[test]
fn test_spawn_daemon_does_not_leak_inheritable_handles() {
    use sanitized_pipe_helpers::*;

    let sleeper = testbin_path("testbin-sleeper");
    let group = ContainedProcessGroup::new().expect("create group");

    let pipe = create_inheritable_pipe();

    let mut cmd = Command::new(&sleeper);
    let mut child = group.spawn_daemon(&mut cmd).expect("spawn daemon");
    let child_pid = child.id();
    assert!(child_pid != 0, "child PID should be non-zero");

    close(pipe.write_end);

    let start = Instant::now();
    let result = read_one_byte_with_timeout(pipe.read_end, Duration::from_secs(2));
    let elapsed = start.elapsed();
    close(pipe.read_end);

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        matches!(result, Ok(0)),
        "expected EOF on read end, got {result:?} after {elapsed:?} — \
         child likely inherited an orphaned copy of the pipe write-end"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "EOF should be prompt; took {elapsed:?}"
    );

    // Suppress dead-code warning for force_kill on platforms where the
    // test happens to take the kill() path above.
    let _ = force_kill;
}
