//! soldr#2388 — daemon tombstone end-to-end: an explicit `soldr daemon stop`
//! plants a suppression window during which the broker's requested daemon
//! launch (the one implicit-start path post-Step-4) is skipped, guarding
//! against a thundering herd of restarts. `soldr daemon start` lifts it.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use soldr_cli::core::SoldrPaths;
use soldr_cli::timed_test;

struct ServiceDefinitionGuard(PathBuf);

impl Drop for ServiceDefinitionGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct BrokerGuard {
    home: PathBuf,
    root: PathBuf,
}

impl Drop for BrokerGuard {
    fn drop(&mut self) {
        let mut stop = Command::new(common::soldr_bin());
        common::scrub_outer_soldr_env(&mut stop);
        let _ = stop
            .args(["broker", "stop"])
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(pid) = find_daemon_pid(&self.root) {
            let _ = kill_pid(pid);
        }
    }
}

timed_test!(
    daemon_stop_tombstone_suppresses_broker_proactive_launch,
    Duration::from_secs(90),
    {
        let root = common::unique_temp_dir("tombstone-root");
        let home = common::unique_temp_dir("tombstone-home");
        let service_root = home.join("service-definitions");
        let project = root.join("workspace");
        std::fs::create_dir_all(project.join("src")).expect("create source directory");
        std::fs::write(project.join("src/lib.rs"), "pub fn tombstone() {}\n")
            .expect("write source");

        // Register the isolated route before the front door establishes its
        // broker. The service-definition override is part of the detached
        // broker's explicit environment contract.
        let paths = SoldrPaths::with_root(root.clone());
        let installed =
            soldr_cli::daemon::service_definition::install_service_definition_to_dir_for_paths(
                &service_root,
                &paths,
                &common::soldr_daemon_bin(),
            )
            .expect("install isolated daemon service definition");
        let _service_definition = ServiceDefinitionGuard(installed.path.clone());

        // Establish the detached broker through an ordinary front-door command.
        // This also proves the test-only service-definition root survives the
        // broker spawn's sanitized environment boundary.
        let mut version = Command::new(common::soldr_bin());
        common::scrub_outer_soldr_env(&mut version);
        let version_out = version
            .arg("version")
            .env("SOLDR_CACHE_DIR", &root)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("RUNNING_PROCESS_SERVICE_DEF_DIR", &service_root)
            .output()
            .expect("establish isolated broker");
        assert!(
            version_out.status.success(),
            "broker-establishing version command failed: {}",
            String::from_utf8_lossy(&version_out.stderr)
        );
        let _broker = BrokerGuard {
            home: home.clone(),
            root: root.clone(),
        };

        // 1) `soldr daemon stop` on a clean root: the daemon isn't running, but
        //    the tombstone is planted regardless (the suppression window starts
        //    the moment a stop is requested).
        let mut stop = Command::new(common::soldr_bin());
        common::scrub_outer_soldr_env(&mut stop);
        let stop_out = stop
            .args(["daemon", "stop"])
            .env("SOLDR_CACHE_DIR", &root)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("RUNNING_PROCESS_SERVICE_DEF_DIR", &service_root)
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
        // 2) Since #2441 the broker is passive until a client requests a
        // registered route. It must observe the live tombstone and leave no
        // daemon behind.
        let rustc = Path::new(&std::env::var_os("CARGO").expect("CARGO set by cargo test"))
            .with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX));
        let mut request = Command::new(common::soldr_bin());
        common::scrub_outer_soldr_env(&mut request);
        let request_output = request
            .arg(&rustc)
            .args([
                "--edition",
                "2021",
                "--crate-type",
                "lib",
                "--crate-name",
                "soldr_tombstone_probe",
                "--emit=metadata",
                "--out-dir",
                "target",
                "src/lib.rs",
            ])
            .current_dir(&project)
            .env("SOLDR_CACHE_DIR", &root)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("RUNNING_PROCESS_SERVICE_DEF_DIR", &service_root)
            .output()
            .expect("request tombstoned route through compiler wrapper");
        assert!(
            !request_output.status.success(),
            "the broker request must fail while the tombstone is live"
        );
        let request_message = format!(
            "{}{}",
            String::from_utf8_lossy(&request_output.stdout),
            String::from_utf8_lossy(&request_output.stderr)
        );

        // Give any (erroneous) launch a moment to publish its route claim before we
        // assert none exists.
        std::thread::sleep(Duration::from_secs(2));
        let daemon_launched = find_daemon_pid(&root);

        let log = format!("broker request: {request_message}");
        assert!(
            request_message.contains("tombstone active"),
            "broker must report that it skipped the requested launch under a live \
             tombstone\n{log}"
        );
        assert!(
            daemon_launched.is_none(),
            "no daemon may be launched while the tombstone is live (found pid \
             {daemon_launched:?})\n{log}"
        );
    }
);

/// Read the daemon PID from the route's protobuf ownership claim, if any.
fn find_daemon_pid(root: &std::path::Path) -> Option<u32> {
    soldr_cli::daemon::backend_handle_adoption::read_broker_route_claim(&SoldrPaths::with_root(
        root.to_path_buf(),
    ))
    .ok()
    .flatten()
    .map(|claim| claim.pid)
}

fn kill_pid(pid: u32) -> std::io::Result<()> {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output()
            .map(|_| ())
    } else {
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output()
            .map(|_| ())
    }
}
