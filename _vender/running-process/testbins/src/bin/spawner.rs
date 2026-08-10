//! Test binary: spawns N sleeper grandchildren, prints all PIDs, and
//! sleeps forever.
//! Usage: spawner <count> <sleeper_path>
//!
//! Output format (one line per child, then a READY marker):
//!   SPAWNER_PID=<pid>
//!   CHILD_PID=<pid>
//!   READY
//!
//! Used by containment integration tests to verify grandchild cleanup.
//!
//! ## Why two spawn paths (cfg-gated)
//!
//! On Windows, sleepers are launched via `running_process::spawn_daemon`
//! so that `CreateProcessW`'s `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` whitelist
//! restricts inheritance to exactly the three NUL stdio handles. Using
//! bare `std::process::Command::spawn` here leaks the spawner's stdout
//! (the pipe write-end going back to the test) into each sleeper, and
//! the test reader then blocks forever waiting for an EOF that never
//! arrives — see zackees/running-process#115.
//!
//! On Unix, sleepers use bare `std::process::Command::spawn` with NULL
//! stdio. Our sanitized spawn calls `setpgid(0, 0)` in the child, which
//! would remove the sleeper from the spawner's process group; the test's
//! containment cleanup is `killpg(spawner_pgid, SIGKILL)`, and an escaped
//! sleeper would survive the kill and fail the test on macOS (Linux has
//! `PR_SET_PDEATHSIG` as a backstop, macOS does not). Unix doesn't suffer
//! the Windows handle-inheritance pathology in this scenario because we
//! explicitly NULL the sleeper's stdio, so the spawner→test pipe is never
//! copied into the sleeper.

use std::io::Write;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: spawner <count> <sleeper_path>");
        std::process::exit(1);
    }
    let count: usize = args[1].parse().expect("count must be a number");
    let sleeper_path = &args[2];

    println!("SPAWNER_PID={}", std::process::id());
    std::io::stdout().flush().unwrap();

    for _ in 0..count {
        let pid = spawn_sleeper(sleeper_path);
        println!("CHILD_PID={pid}");
        std::io::stdout().flush().unwrap();
    }

    println!("READY");
    std::io::stdout().flush().unwrap();

    // Sleep forever — expect to be killed by the test harness.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(windows)]
fn spawn_sleeper(sleeper_path: &str) -> u32 {
    let mut cmd = Command::new(sleeper_path);
    let child = running_process::spawn_daemon(&mut cmd).expect("failed to spawn sleeper");
    let pid = child.id();
    // `DaemonChild::Drop` just closes our process handle — the sleeper
    // keeps running and is reaped by the test's Job Object when the
    // group is dropped.
    drop(child);
    pid
}

#[cfg(unix)]
fn spawn_sleeper(sleeper_path: &str) -> u32 {
    use std::process::Stdio;

    let child = Command::new(sleeper_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn sleeper");
    let pid = child.id();
    // `std::process::Child::Drop` does not kill on Unix — the sleeper
    // keeps running in the spawner's process group and is reaped by the
    // test's `killpg(spawner_pgid, SIGKILL)` cleanup.
    drop(child);
    pid
}
