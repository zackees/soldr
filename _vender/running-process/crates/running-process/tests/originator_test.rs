//! Integration tests for `RUNNING_PROCESS_ORIGINATOR` env var propagation and
//! `find_processes_by_originator` scanner.
//!
//! Fix Wave T4 of #165: `running_process::originator` is gated behind
//! `feature = "originator-scan"`. Gate the entire test binary the same
//! way so single-feature builds (e.g. `--features daemon` alone) skip
//! this file cleanly instead of hitting an unresolved-import error.
#![cfg(feature = "originator-scan")]

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use running_process::originator::find_processes_by_originator;
use running_process::{ContainedProcessGroup, SpawnStdio, SpawnedChild, StdioSource};

/// Build and locate a test binary from the workspace.
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

fn read_until_ready(child: &mut SpawnedChild) -> (Option<u32>, Option<String>) {
    let stdout = child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);

    let mut pid: Option<u32> = None;
    let mut originator: Option<String> = None;

    let start = Instant::now();
    for line in reader.lines() {
        if start.elapsed() > Duration::from_secs(10) {
            panic!("timed out reading env-reporter output");
        }
        let line = line.expect("read line");
        if let Some(val) = line.strip_prefix("PID=") {
            pid = Some(val.trim().parse().expect("parse PID"));
        } else if let Some(val) = line.strip_prefix("ORIGINATOR=") {
            originator = Some(val.trim().to_string());
        } else if line.trim() == "READY" {
            break;
        }
    }

    (pid, originator)
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

#[test]
fn test_originator_env_var_is_set_on_child() {
    let env_reporter = testbin_path("testbin-env-reporter");
    let group = ContainedProcessGroup::with_originator("TESTOOL").expect("create group");

    let mut cmd = Command::new(&env_reporter);
    let mut child = group.spawn(&mut cmd, pipe_stdio()).expect("spawn");

    let (child_pid, originator) = read_until_ready(&mut child);
    assert!(child_pid.is_some(), "should get child PID");

    let originator = originator.expect("should get originator value");
    let expected = format!("TESTOOL:{}", std::process::id());
    assert_eq!(originator, expected);

    drop(group);
    if let Some(pid) = child_pid {
        force_kill(pid);
    }
}

#[test]
fn test_no_originator_env_var_without_originator() {
    let env_reporter = testbin_path("testbin-env-reporter");
    let group = ContainedProcessGroup::new().expect("create group");

    let mut cmd = Command::new(&env_reporter);
    cmd.env("RUNNING_PROCESS_ORIGINATOR", "STALE_PARENT:1");
    let mut child = group.spawn(&mut cmd, pipe_stdio()).expect("spawn");

    let (child_pid, originator) = read_until_ready(&mut child);
    assert!(child_pid.is_some(), "should get child PID");

    let originator = originator.expect("should get originator line");
    assert_eq!(originator, "<not set>");

    drop(group);
    if let Some(pid) = child_pid {
        force_kill(pid);
    }
}

#[test]
fn test_find_processes_by_originator_finds_child() {
    let sleeper = testbin_path("testbin-sleeper");

    let tool_name = format!("TESTFIND{}", std::process::id());
    let group = ContainedProcessGroup::with_originator(&tool_name).expect("create group");

    let mut cmd = Command::new(&sleeper);
    let child = group.spawn(&mut cmd, pipe_stdio()).expect("spawn");
    let child_pid = child.id();

    std::thread::sleep(Duration::from_millis(500));

    let results = find_processes_by_originator(&tool_name);

    let found = results.iter().any(|r| r.pid == child_pid);
    assert!(
        found,
        "should find child PID {child_pid} in scan results; found {} results",
        results.len(),
    );

    for r in &results {
        if r.pid == child_pid {
            assert!(r.parent_alive, "parent should be alive");
            assert_eq!(r.parent_pid, std::process::id());
        }
    }

    drop(group);
    force_kill(child_pid);
}

#[test]
fn test_find_processes_excludes_non_matching_tool() {
    let sleeper = testbin_path("testbin-sleeper");

    let tool_name = format!("EXCL{}", std::process::id());
    let group = ContainedProcessGroup::with_originator(&tool_name).expect("create group");

    let mut cmd = Command::new(&sleeper);
    let child = group.spawn(&mut cmd, pipe_stdio()).expect("spawn");
    let child_pid = child.id();

    std::thread::sleep(Duration::from_millis(500));

    let results = find_processes_by_originator("NONEXISTENT_TOOL_XYZ");
    let found = results.iter().any(|r| r.pid == child_pid);
    assert!(
        !found,
        "should NOT find child PID {child_pid} with wrong tool"
    );

    drop(group);
    force_kill(child_pid);
}
