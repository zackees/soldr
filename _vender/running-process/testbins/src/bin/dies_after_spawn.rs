//! Test binary: spawns a long-lived grandchild via running_process::spawn,
//! prints the grandchild PID, prints READY, then exits 0.
//!
//! Reads the path to the sleeper binary from RUNNING_PROCESS_SPAWN_TARGET.
//!
//! Used by `spawn_test::test_spawn_force_killed_parent_reaps_child` to verify
//! that a contained grandchild dies when this intermediate parent exits.

use std::io::Write;
use std::process::Command;

use running_process::{spawn, SpawnStdio};

const SPAWN_TARGET_ENV: &str = "RUNNING_PROCESS_SPAWN_TARGET";

fn main() {
    let target = std::env::var(SPAWN_TARGET_ENV).expect("RUNNING_PROCESS_SPAWN_TARGET must be set");

    let mut cmd = Command::new(&target);
    let child = spawn(&mut cmd, SpawnStdio::default()).expect("spawn grandchild");

    println!("GRANDCHILD_PID={}", child.id());
    std::io::stdout().flush().unwrap();
    println!("READY");
    std::io::stdout().flush().unwrap();

    // Intentionally leak the SpawnedChild so that its Drop doesn't fire
    // before we exit — the test wants to observe the Job Object / process
    // group containment killing the grandchild on parent death, not the
    // explicit Drop.
    std::mem::forget(child);

    std::process::exit(0);
}
