//! Test binary: prints its PID and RUNNING_PROCESS_ORIGINATOR, then sleeps.

use std::io::Write;

fn main() {
    let pid = std::process::id();
    println!("PID={pid}");
    match std::env::var(running_process::ORIGINATOR_ENV_VAR) {
        Ok(val) => println!("ORIGINATOR={val}"),
        Err(_) => println!("ORIGINATOR=<not set>"),
    }
    println!("READY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
