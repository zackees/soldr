//! soldr#2388 — daemon tombstone end-to-end: an explicit `soldr daemon stop`
//! plants a suppression window during which the broker's proactive daemon
//! launch (the one implicit-start path post-Step-4) is skipped, guarding
//! against a thundering herd of restarts. `soldr daemon start` lifts it.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use soldr_cli::timed_test;

fn unique_program(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("soldr-tombstone-{label}-{:012x}", nanos & 0xFFFF_FFFF_FFFF)
}

fn drain<R: std::io::Read + Send + 'static>(reader: R) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            sink.lock().unwrap().push(line);
        }
    });
    lines
}

fn wait_for(lines: &Arc<Mutex<Vec<String>>>, needle: &str, deadline: Instant) -> bool {
    loop {
        if lines.lock().unwrap().iter().any(|l| l.contains(needle)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

timed_test!(
    daemon_stop_tombstone_suppresses_broker_proactive_launch,
    Duration::from_secs(90),
    {
        let root = common::unique_temp_dir("tombstone-root");
        let program = unique_program("prog");

        // 1) `soldr daemon stop` on a clean root: the daemon isn't running, but
        //    the tombstone is planted regardless (the suppression window starts
        //    the moment a stop is requested).
        let mut stop = Command::new(common::soldr_bin());
        common::scrub_outer_soldr_env(&mut stop);
        let stop_out = stop
            .args(["daemon", "stop"])
            .env("SOLDR_CACHE_DIR", &root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("run soldr daemon stop");
        assert!(
            stop_out.status.success(),
            "daemon stop failed: {}",
            String::from_utf8_lossy(&stop_out.stderr)
        );

        // 2) Start a broker on the same root. Its proactive daemon launch must
        //    observe the live tombstone and skip — logging the skip and leaving
        //    no daemon behind.
        let mut broker = Command::new(common::soldr_bin())
            .args(["broker", "serve", "--program", &program])
            .env("SOLDR_CACHE_DIR", &root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn broker");
        let out = drain(broker.stdout.take().expect("broker stdout"));
        let err = drain(broker.stderr.take().expect("broker stderr"));

        // The broker's daemon-launch thread runs at startup; wait for its skip.
        let skipped = wait_for(
            &err,
            "tombstone active",
            Instant::now() + Duration::from_secs(20),
        );

        // Give any (erroneous) launch a moment to publish a pidfile before we
        // assert none exists.
        std::thread::sleep(Duration::from_secs(2));
        let daemon_launched = find_daemon_pid(&root);

        let _ = broker.kill();
        let _ = broker.wait();
        // Reap any daemon that slipped through so the test never leaks one.
        if let Some(pid) = daemon_launched {
            let _ = kill_pid(pid);
        }

        let log = format!(
            "broker stdout:\n  {}\nbroker stderr:\n  {}",
            out.lock().unwrap().join("\n  "),
            err.lock().unwrap().join("\n  ")
        );
        assert!(
            skipped,
            "broker must log that it skipped the proactive launch under a live \
             tombstone\n{log}"
        );
        assert!(
            daemon_launched.is_none(),
            "no daemon may be launched while the tombstone is live (found pid \
             {daemon_launched:?})\n{log}"
        );
    }
);

/// Find the first `daemon.pid` under `root`, if any.
fn find_daemon_pid(root: &std::path::Path) -> Option<u32> {
    fn walk(dir: &std::path::Path) -> Option<u32> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(pid) = walk(&path) {
                    return Some(pid);
                }
            } else if path.file_name().is_some_and(|n| n == "daemon.pid") {
                if let Some(pid) = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| t.split_whitespace().next().and_then(|s| s.parse().ok()))
                {
                    return Some(pid);
                }
            }
        }
        None
    }
    walk(root)
}

fn kill_pid(pid: u32) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output()
            .map(|_| ())
    }
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output()
            .map(|_| ())
    }
}
