//! Process-level correctness coverage for soldr#2476 broker resurrection.

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);

fn front_door(home: &Path) -> Command {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    command
        .arg("version")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn wait_for_child(child: &mut Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().expect("inspect front door") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

fn spawn_log(home: &Path) -> String {
    std::fs::read_to_string(home.join(".soldr/broker/broker-spawn.log")).unwrap_or_default()
}

fn stop_broker(home: &Path) {
    let _ = Command::new(common::soldr_bin())
        .args(["broker", "stop"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output();
}

soldr_cli::timed_test!(
    issue_2476_sixty_four_process_stampede_binds_one_broker,
    Duration::from_secs(120),
    {
        let home = common::unique_temp_dir("broker-64-process-stampede");
        let mut children: Vec<_> = (0..64)
            .map(|_| {
                let mut command = front_door(&home);
                // Keep the lease winner in the fenced section briefly so all
                // 64 independently-started hosts compete for the same row.
                command.env("SOLDR_TEST_BROKER_LEASE_PAUSE_MS", "500");
                command.spawn().expect("spawn front door contender")
            })
            .collect();

        let deadline = Instant::now() + Duration::from_secs(60);
        let mut failures = Vec::new();
        for child in &mut children {
            match wait_for_child(child, deadline) {
                Some(status) if status.success() => {}
                Some(status) => failures.push(format!("pid {} exited {status}", child.id())),
                None => {
                    failures.push(format!("pid {} did not exit", child.id()));
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }

        let log = spawn_log(&home);
        stop_broker(&home);
        assert!(
            failures.is_empty(),
            "front-door failures: {failures:?}\n{log}"
        );
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("stable endpoint bound at"))
                .count(),
            1,
            "64 contenders must produce exactly one bound broker\n{log}"
        );
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("binding stable endpoint"))
                .count(),
            1,
            "only the lease winner may spawn a broker candidate\n{log}"
        );
    }
);

#[cfg(unix)]
soldr_cli::timed_test!(
    issue_2476_sigstop_owner_is_fenced_after_lease_expiry,
    Duration::from_secs(90),
    {
        let home = common::unique_temp_dir("broker-sigstop-takeover");
        let ready = home.join("lease-acquired");
        let stopped_owner_stderr = home.join("stopped-owner.stderr");
        let mut stopped_owner = front_door(&home)
            // Longer than the five-second lease, but short enough that the
            // resumed owner reaches its fence check promptly even when the OS
            // restarts an interrupted sleep with its remaining duration.
            .env("SOLDR_TEST_BROKER_LEASE_PAUSE_MS", "6000")
            .env("SOLDR_TEST_BROKER_LEASE_READY_FILE", &ready)
            .stderr(
                std::fs::File::create(&stopped_owner_stderr)
                    .expect("create stopped-owner diagnostic log"),
            )
            .spawn()
            .expect("spawn lease owner");

        let ready_deadline = Instant::now() + Duration::from_secs(20);
        while !ready.exists() && Instant::now() < ready_deadline {
            std::thread::sleep(POLL);
        }
        assert!(ready.exists(), "first host never acquired the lease");
        let pid = stopped_owner.id().to_string();
        let stop = Command::new("kill")
            .args(["-STOP", &pid])
            .status()
            .expect("SIGSTOP lease owner");
        assert!(stop.success(), "could not SIGSTOP lease owner");

        let mut replacement = front_door(&home)
            .spawn()
            .expect("spawn replacement contender");
        let replacement_status =
            wait_for_child(&mut replacement, Instant::now() + Duration::from_secs(30));

        let _ = Command::new("kill").args(["-CONT", &pid]).status();
        let resumed_status =
            wait_for_child(&mut stopped_owner, Instant::now() + Duration::from_secs(15));
        if resumed_status.is_none() {
            let _ = stopped_owner.kill();
            let _ = stopped_owner.wait();
        }
        let log = spawn_log(&home);
        let stopped_owner_diagnostic =
            std::fs::read_to_string(&stopped_owner_stderr).unwrap_or_default();
        stop_broker(&home);

        assert!(
            replacement_status.is_some_and(|status| status.success()),
            "a contender must recover after the stopped owner's five-second lease expires\n{log}"
        );
        assert!(
            resumed_status.is_some_and(|status| status.success()),
            "the resumed former holder must observe its fence and exit without staging/spawning; \
             status={resumed_status:?}\nstderr:\n{stopped_owner_diagnostic}\n{log}"
        );
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("stable endpoint bound at"))
                .count(),
            1,
            "the fenced owner must not spawn a second broker after resume\n{log}"
        );
    }
);
