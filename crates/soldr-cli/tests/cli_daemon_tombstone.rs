//! soldr#2388/#2531 — daemon tombstone end-to-end: an explicit stop plants a
//! fence so an older in-flight launch cannot publish after stop succeeds. A
//! later demand-driven route request clears the fence and starts one new
//! generation; the launcher's post-spawn checks retain the race protection.

mod common;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use soldr_cli::core::SoldrPaths;

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

#[test]
fn daemon_stop_tombstone_fences_old_launch_but_allows_new_demand() {
    let root = common::unique_temp_dir("tombstone-root");
    let home = common::unique_temp_dir("tombstone-home");
    let service_root = home.join("service-definitions");
    let project = root.join("workspace");
    let launch_ready = root.join("launch-ready");
    std::fs::create_dir_all(project.join("src")).expect("create source directory");
    std::fs::write(project.join("src/lib.rs"), "pub fn tombstone() {}\n").expect("write source");

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
    // `daemon stop` is an eligible top-level front door, so it first
    // confirms/resurrects the stable broker before planting the route
    // tombstone. Stop that broker explicitly before starting the
    // instrumented foreground broker below; the tombstone itself remains.
    let broker_stop = Command::new(common::soldr_bin())
        .args(["broker", "stop"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .output()
        .expect("stop broker created by daemon stop front door");
    assert!(
        broker_stop.status.success(),
        "broker stop failed: {}",
        String::from_utf8_lossy(&broker_stop.stderr)
    );

    // 2) Register this root's route and start a broker. Since #2441 the
    //    broker is passive until a client requests a registered route; that
    //    new demand must clear the stop fence and launch one replacement.
    let paths = SoldrPaths::with_root(root.clone());
    let installed =
        soldr_cli::daemon::service_definition::install_service_definition_to_dir_for_paths(
            &service_root,
            &paths,
            &common::soldr_daemon_bin(),
        )
        .expect("install isolated daemon service definition");
    let _service_definition = ServiceDefinitionGuard(installed.path.clone());
    let mut broker_command = Command::new(common::soldr_bin());
    common::scrub_outer_soldr_env(&mut broker_command);
    let child = broker_command
        .args(["broker", "serve"])
        .env("SOLDR_CACHE_DIR", &root)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("RUNNING_PROCESS_SERVICE_DEF_DIR", &service_root)
        .env("SOLDR_TEST_DAEMON_LAUNCH_PAUSE_MS", "3000")
        .env("SOLDR_TEST_DAEMON_LAUNCH_READY_FILE", &launch_ready)
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

    // soldr#2493: 10s was not enough and the reason is now measured rather
    // than guessed. Bringup instrumentation on the failing Linux lane shows
    // secure_directories/tokio_runtime/peer_policy each at 0ms and then no
    // further phase inside the window — the broker spends the whole time in
    // `instance_id`, blake3-hashing its own executable. It is not waiting on
    // the hash lock (no contention line is emitted); the cold hash itself is
    // that slow on a loaded runner. A local probe measured 3.8s for a 60MB
    // image on a warm dev box.
    //
    // This test launches `broker serve` directly, which is the expensive
    // case: a front-door-spawned broker is handed its instance id via
    // `SOLDR_INTERNAL_BROKER_INSTANCE_ID` and never hashes at all. So the
    // window has to cover work production does not do. 60s clears the
    // observed cost with margin and stays well inside the 90s watchdog,
    // which is what actually guards against a real hang.
    let broker_ready = wait_for(
        &out,
        "stable endpoint bound at",
        Instant::now() + Duration::from_secs(60),
    );
    let child_state = match broker.child.try_wait() {
        Ok(Some(status)) => format!("exited {status}"),
        Ok(None) => "still alive".to_string(),
        Err(error) => format!("wait error {error}"),
    };
    assert!(
        broker_ready,
        "broker did not become ready (child {child_state}).\nThe last `bringup phase=` line \
             on stderr names the last phase that COMPLETED; the stall is in the phase after it.\
             \nstdout:\n{}\nstderr:\n{}",
        out.lock().unwrap().join("\n"),
        err.lock().unwrap().join("\n")
    );
    let rustc = Path::new(&std::env::var_os("CARGO").expect("CARGO set by cargo test"))
        .with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    let request_command = || {
        let mut request = common::isolated_soldr_command();
        request
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
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        request
    };
    let first_request = request_command()
        .spawn()
        .expect("request route through compiler wrapper");
    let launch_deadline = Instant::now() + Duration::from_secs(30);
    while !launch_ready.is_file() && Instant::now() < launch_deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        launch_ready.is_file(),
        "daemon launch never reached ready fence"
    );
    // `daemon stop` plants this exact fence before it attempts control IPC.
    // Plant it directly here because invoking the full front door would first
    // serialize behind the launch under test and could not exercise the race.
    soldr_cli::daemon::tombstone::plant(&paths, soldr_cli::daemon::tombstone::TOMBSTONE_DURATION);
    let first_output = first_request
        .wait_with_output()
        .expect("wait for fenced launch request");
    assert!(
        !first_output.status.success(),
        "the pre-stop launch unexpectedly published readiness"
    );

    let _ = std::fs::remove_file(&launch_ready);
    let request_output = request_command()
        .output()
        .expect("new demand after stop tombstone");
    let request_message = format!(
        "{}{}",
        String::from_utf8_lossy(&request_output.stdout),
        String::from_utf8_lossy(&request_output.stderr)
    );
    assert!(request_output.status.success(), "{request_message}");

    // Give the replacement a moment to publish its route claim.
    std::thread::sleep(Duration::from_secs(2));
    let daemon_launched = find_daemon_pid(&root);

    drop(broker);

    let log = format!(
        "broker request: {request_message}\nbroker stdout:\n  {}\nbroker stderr:\n  {}",
        out.lock().unwrap().join("\n  "),
        err.lock().unwrap().join("\n  ")
    );
    assert!(
        daemon_launched.is_some(),
        "new client demand must launch exactly one replacement\n{log}"
    );
}

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
