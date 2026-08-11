//! soldr#2388 — daemon tombstone end-to-end: an explicit `soldr daemon stop`
//! plants a suppression window during which the broker's requested daemon
//! launch (the one implicit-start path post-Step-4) is skipped, guarding
//! against a thundering herd of restarts. `soldr daemon start` lifts it.

mod common;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use running_process::broker::protocol_v2::service_definition_dir_v2;
use soldr_cli::core::SoldrPaths;
use soldr_cli::daemon::backend_handle_adoption::SOLDR_DAEMON_SERVICE_VERSION;
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

struct ServiceDefinitionGuard(PathBuf);

impl Drop for ServiceDefinitionGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct BrokerGuard {
    child: Child,
    root: PathBuf,
}

impl Drop for BrokerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

        // 2) Register this root's route and start a broker. Since #2441 the
        //    broker is passive until a client requests a registered route; that
        //    requested launch must observe the live tombstone and leave no
        //    daemon behind.
        let paths = SoldrPaths::with_root(root.clone());
        let installed =
            soldr_cli::daemon::service_definition::install_service_definition_to_dir_for_paths(
                service_definition_dir_v2(),
                &paths,
                &common::soldr_daemon_bin(),
            )
            .expect("install isolated daemon service definition");
        let _service_definition = ServiceDefinitionGuard(installed.path.clone());
        let broker_executable = common::soldr_bin();
        let mut broker_command = Command::new(&broker_executable);
        common::scrub_outer_soldr_env(&mut broker_command);
        let child = broker_command
            .args(["broker", "serve", "--program", &program])
            .env("SOLDR_CACHE_DIR", &root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn broker");
        let mut broker = BrokerGuard {
            child,
            root: root.clone(),
        };
        let out = drain(broker.child.stdout.take().expect("broker stdout"));
        let err = drain(broker.child.stderr.take().expect("broker stderr"));

        let request_deadline = Instant::now() + Duration::from_secs(5);
        let request_error = loop {
            let result =
                running_process::broker::client_v2::connect_service_for_broker_path_with_deadline(
                    &program,
                    &broker_executable,
                    &installed.definition.service_name,
                    SOLDR_DAEMON_SERVICE_VERSION,
                    Duration::from_secs(2),
                );
            let error =
                result.expect_err("the broker request must fail while the tombstone is live");
            if error.to_string().contains("tombstone active") || Instant::now() >= request_deadline
            {
                break error;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let request_message = request_error.to_string();

        // Give any (erroneous) launch a moment to publish a pidfile before we
        // assert none exists.
        std::thread::sleep(Duration::from_secs(2));
        let daemon_launched = find_daemon_pid(&root);

        drop(broker);

        let log = format!(
            "broker request: {request_error:?}\nbroker stdout:\n  {}\nbroker stderr:\n  {}",
            out.lock().unwrap().join("\n  "),
            err.lock().unwrap().join("\n  ")
        );
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
