//! End-to-end tests for the `cleanup verify` artifact reconciliation (#391).

use std::path::PathBuf;

use running_process::cleanup::verify_artifacts::{
    run_with_probes, ArtifactPaths, ArtifactStatus, SocketLocation, EMERGENCY_RESERVE_FILE_NAME,
};

fn temp_root(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rp-cleanup-verify-{label}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn paths_in(root: &std::path::Path) -> ArtifactPaths {
    ArtifactPaths {
        socket: SocketLocation::File(root.join("daemon.sock")),
        pid_file: root.join("daemon.pid"),
        db: root.join("tracked-pids.sqlite3"),
        data_dir: root.to_path_buf(),
        emergency_reserve: root.join(EMERGENCY_RESERVE_FILE_NAME),
        emergency_reserve_bytes: 4096,
        service_definition_dir: root.join("services"),
        shadow_dir: root.join("run"),
    }
}

const ALL_CLASSES: &[&str] = &[
    "socket",
    "pid-file",
    "servicedef",
    "database",
    "logs",
    "emergency-reserve",
    "shadow",
];

#[test]
fn clean_environment_reports_every_class_with_no_findings() {
    let root = temp_root("clean");
    let report = run_with_probes(&paths_in(&root), &|_| false, &|_| {
        Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
    });
    for class in ALL_CLASSES {
        assert!(
            report.checks.iter().any(|check| check.class == *class),
            "missing artifact class {class}"
        );
    }
    assert_eq!(report.finding_count(), 0, "{}", report.render_text());
    assert_eq!(report.exit_code(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn crashed_daemon_residue_is_reported_per_class_and_never_deleted() {
    let root = temp_root("residue");
    let paths = paths_in(&root);

    std::fs::write(root.join("daemon.sock"), b"").unwrap();
    std::fs::write(root.join("daemon.pid"), "999999\n").unwrap();
    std::fs::write(root.join("tracked-pids.sqlite3"), b"db").unwrap();
    std::fs::write(root.join("tracked-pids.sqlite3-wal"), b"wal").unwrap();
    std::fs::write(root.join("daemon.log"), b"log line").unwrap();
    std::fs::write(root.join(EMERGENCY_RESERVE_FILE_NAME), vec![0u8; 100]).unwrap();
    std::fs::create_dir_all(root.join("services")).unwrap();
    std::fs::write(root.join("services").join("svc.servicedef"), b"x").unwrap();
    std::fs::write(root.join("services").join("stray.bak"), b"x").unwrap();
    std::fs::create_dir_all(root.join("run")).unwrap();
    std::fs::write(root.join("run").join("daemon-old"), b"x").unwrap();

    let report = run_with_probes(&paths, &|_| false, &|_| {
        Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
    });

    let status_of = |class: &str, location_contains: &str| {
        report
            .checks
            .iter()
            .find(|check| check.class == class && check.location.contains(location_contains))
            .unwrap_or_else(|| panic!("no {class} check for {location_contains}"))
            .status
    };

    assert_eq!(status_of("socket", "daemon.sock"), ArtifactStatus::Stale);
    assert_eq!(status_of("pid-file", "daemon.pid"), ArtifactStatus::Stale);
    assert_eq!(
        status_of("database", "tracked-pids.sqlite3-wal"),
        ArtifactStatus::Stale
    );
    assert_eq!(status_of("logs", ""), ArtifactStatus::Present);
    assert_eq!(
        status_of("emergency-reserve", EMERGENCY_RESERVE_FILE_NAME),
        ArtifactStatus::Stale
    );
    assert_eq!(
        status_of("servicedef", "stray.bak"),
        ArtifactStatus::Orphaned
    );
    assert_eq!(status_of("shadow", "run"), ArtifactStatus::Present);

    // Read-only contract: every artifact survives verification.
    for leaf in [
        "daemon.sock",
        "daemon.pid",
        "tracked-pids.sqlite3",
        "tracked-pids.sqlite3-wal",
        "daemon.log",
        EMERGENCY_RESERVE_FILE_NAME,
    ] {
        assert!(root.join(leaf).exists(), "{leaf} was deleted");
    }
    assert!(root.join("services").join("stray.bak").exists());
    assert!(root.join("run").join("daemon-old").exists());

    assert!(report.finding_count() >= 5);
    assert_eq!(report.exit_code(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn live_daemon_marks_owned_artifacts_active() {
    let root = temp_root("live");
    let paths = paths_in(&root);
    std::fs::write(root.join("daemon.sock"), b"").unwrap();
    std::fs::write(root.join("daemon.pid"), "1234").unwrap();
    std::fs::write(root.join("tracked-pids.sqlite3"), b"db").unwrap();
    std::fs::write(root.join("tracked-pids.sqlite3-shm"), b"shm").unwrap();

    let report = run_with_probes(&paths, &|pid| pid == 1234, &|_| Ok(()));
    let statuses: Vec<_> = report
        .checks
        .iter()
        .filter(|check| check.status == ArtifactStatus::Active)
        .map(|check| check.class)
        .collect();
    assert!(statuses.contains(&"socket"));
    assert!(statuses.contains(&"pid-file"));
    assert!(statuses.contains(&"database"));
    assert_eq!(report.finding_count(), 0, "{}", report.render_text());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn json_document_is_stable_and_complete() {
    let root = temp_root("json");
    let report = run_with_probes(&paths_in(&root), &|_| false, &|_| {
        Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
    });
    let json = report.to_json_value();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["exit_code"], 0);
    let checks = json["checks"].as_array().unwrap();
    assert_eq!(checks.len(), report.checks.len());
    for check in checks {
        assert!(check["class"].is_string());
        assert!(check["location"].is_string());
        assert!(check["status"].is_string());
        assert!(check["detail"].is_string());
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn from_environment_covers_every_class_without_creating_dirs() {
    let paths = ArtifactPaths::from_environment(Some("feedfacefeedface"));
    assert!(paths
        .pid_file
        .to_string_lossy()
        .contains("feedfacefeedface"));
    assert!(paths.db.to_string_lossy().contains("feedfacefeedface"));
    assert!(paths
        .emergency_reserve
        .ends_with(EMERGENCY_RESERVE_FILE_NAME));
    match &paths.socket {
        SocketLocation::NamedPipe(name) => assert!(cfg!(windows) && name.contains("feedface")),
        SocketLocation::File(path) => {
            assert!(!cfg!(windows) && path.to_string_lossy().contains("feedface"))
        }
    }
}
