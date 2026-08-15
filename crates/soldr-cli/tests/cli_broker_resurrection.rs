//! Process-level correctness coverage for soldr#2476 broker resurrection.

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);

fn stage_incumbent_broker(home: &Path) -> std::path::PathBuf {
    let broker_dir = home.join(".soldr").join("broker");
    std::fs::create_dir_all(&broker_dir).expect("create incumbent broker directory");
    let broker = broker_dir.join(
        if matches!(
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::HostOs::Windows
        ) {
            "soldr-broker.exe"
        } else {
            "soldr-broker"
        },
    );
    std::fs::copy(common::soldr_bin(), &broker).expect("stage incumbent broker image");
    broker
}

fn front_door(home: &Path) -> Command {
    let mut command = front_door_capturing_stderr(home);
    command.stderr(Stdio::null());
    command
}

/// Same front door, but with stderr left capturable — soldr#2549's broker
/// image-mismatch warning is a stderr diagnostic.
fn front_door_capturing_stderr(home: &Path) -> Command {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    command
        .arg("version")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
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

fn broker_status(home: &Path) -> String {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    let output = command
        .args(["broker", "status"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output()
        .expect("query broker status");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn stop_broker(home: &Path) {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    let _ = command
        .args(["broker", "stop"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output();
}

fn remove_broker(home: &Path) -> std::process::Output {
    let mut command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut command);
    command
        .args(["broker", "remove"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .stdin(Stdio::null())
        .output()
        .expect("run broker remove")
}

/// Bring up a broker that reports a same-version-but-different-image identity,
/// running from the production per-home image path (Windows broker identity is
/// path-derived, so launching the build artifact directly would not model it).
fn spawn_simulated_old_image_broker(home: &Path, instance: &str) -> Child {
    let incumbent_broker = stage_incumbent_broker(home);
    let mut incumbent_command = Command::new(incumbent_broker);
    common::scrub_outer_soldr_env(&mut incumbent_command);
    incumbent_command
        .args(["broker", "serve"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("SOLDR_INTERNAL_BROKER_INSTANCE_ID", instance)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn simulated old-image broker")
}

fn wait_for_broker_instance(home: &Path, instance: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut status = String::new();
    while Instant::now() < deadline {
        status = broker_status(home);
        if status.contains(instance) {
            return status;
        }
        std::thread::sleep(POLL);
    }
    status
}

/// soldr#2549: the broker is a stable, long-lived singleton. A same-version
/// image-digest mismatch must produce a loud diagnostic, never a lifecycle
/// action — this is the inversion of the old
/// `issue_2481_same_version_old_image_is_replaced_before_readiness`, which
/// asserted the incumbent was retired automatically.
#[test]
fn issue_2549_same_version_old_image_broker_is_never_replaced_automatically() {
    let home = common::unique_temp_dir("broker-same-version-old-image");
    let old_instance = format!("soldr-{}-{}", env!("CARGO_PKG_VERSION"), "0".repeat(64));
    let mut incumbent = spawn_simulated_old_image_broker(&home, &old_instance);
    let before = wait_for_broker_instance(&home, &old_instance);

    let front_door = front_door_capturing_stderr(&home)
        .stderr(Stdio::piped())
        .output()
        .expect("run front door against old-image broker");
    let stderr = String::from_utf8_lossy(&front_door.stderr).into_owned();

    // The incumbent must still be the bound broker after the front door has
    // fully returned: not stopped, not killed, not staged over.
    let after = broker_status(&home);
    let survived = incumbent.try_wait().expect("inspect incumbent").is_none();

    // Recovery is explicit and operator-driven.
    let removal = remove_broker(&home);
    let removal_output = format!(
        "{}{}",
        String::from_utf8_lossy(&removal.stdout),
        String::from_utf8_lossy(&removal.stderr)
    );
    let incumbent_exit = wait_for_child(&mut incumbent, Instant::now() + Duration::from_secs(15));
    if incumbent_exit.is_none() {
        let _ = incumbent.kill();
        let _ = incumbent.wait();
    }
    let log = spawn_log(&home);
    stop_broker(&home);

    assert!(
        before.contains(&old_instance),
        "simulated old-image broker never became ready; last status:\n{before}"
    );
    assert!(
        front_door.status.success(),
        "the front door must succeed through a mismatched broker; status={:?}\n{log}",
        front_door.status
    );
    assert!(
        survived,
        "soldr#2549: a live broker must never be retired automatically for an identity mismatch"
    );
    assert!(
        after.contains(&old_instance),
        "the incumbent must still own the endpoint after the front door ran; last status:\n{after}\n{log}"
    );
    assert!(
        stderr.contains("soldr broker remove"),
        "the mismatch warning must name the explicit recovery command; stderr:\n{stderr}"
    );
    assert!(
        !log.contains("stable endpoint bound at"),
        "no replacement broker may be staged or spawned for a live incumbent\n{log}"
    );
    assert!(
        incumbent_exit.is_some(),
        "`soldr broker remove` must retire the broker the operator asked it to:\n{removal_output}"
    );
    assert!(
        removal.status.success(),
        "`soldr broker remove` must succeed:\n{removal_output}"
    );
}

#[test]
fn issue_2476_sixty_four_process_stampede_binds_one_broker() {
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

#[test]
fn issue_2476_sigstop_owner_is_fenced_after_lease_expiry() {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
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

    // Cold target-run hosts may spend tens of seconds hashing the first
    // broker image before the lease-ready test seam is reached.
    let ready_deadline = Instant::now() + Duration::from_secs(90);
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
