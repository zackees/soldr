//! Stable `soldr broker routes --json` schema coverage for soldr#2476.

use crate::common;

use std::process::Stdio;
use std::time::{Duration, Instant};

#[test]
fn issue_2476_routes_json_has_stable_schema_when_broker_is_live() {
    use std::io::{BufRead as _, BufReader};

    let home = common::unique_temp_dir("broker-routes-home");
    let mut broker = common::isolated_soldr_command()
        .args(["broker", "serve"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn broker");
    let stdout = broker.stdout.take().expect("broker stdout");
    let bound = std::thread::spawn(move || {
        BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains("stable endpoint bound at"))
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while !bound.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        bound.is_finished(),
        "broker did not bind its stable endpoint"
    );
    assert!(bound.join().expect("readiness reader"));

    let output = common::isolated_soldr_command()
        .args(["broker", "routes", "--json"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .output()
        .expect("query routes");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("routes JSON: {error}; output={output:?}"));

    let _ = common::isolated_soldr_command()
        .args(["broker", "stop"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .output();
    let _ = broker.wait();

    assert!(output.status.success());
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["running"], true);
    assert!(json["endpoint"]
        .as_str()
        .is_some_and(|value| !value.is_empty()));
    assert!(json["routes"].is_array());
}

#[test]
fn issue_2476_routes_json_is_machine_readable_when_broker_is_absent() {
    let home = common::unique_temp_dir("broker-routes-absent-home");
    let output = common::isolated_soldr_command()
        .args(["broker", "routes", "--json"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .output()
        .expect("query absent routes");
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("absent routes JSON");
    assert!(output.status.success());
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["running"], false);
    assert_eq!(json["routes"], serde_json::json!([]));
}
