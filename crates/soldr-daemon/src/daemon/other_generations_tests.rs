use super::*;
use crate::daemon::backend_handle_adoption::BROKER_ROUTE_CLAIM_FILE;
use tempfile::TempDir;

fn write_claim(
    dir: &std::path::Path,
    daemon: &running_process::broker::backend_handle::DaemonProcess,
) {
    use prost::Message as _;
    std::fs::create_dir_all(dir).expect("claim dir");
    let mut encoded = Vec::new();
    daemon
        .to_proto()
        .encode(&mut encoded)
        .expect("encode claim");
    std::fs::write(dir.join(BROKER_ROUTE_CLAIM_FILE), encoded).expect("write claim");
}

#[test]
fn another_generations_claim_is_found_and_named() {
    let temp = TempDir::new().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    let daemon = crate::daemon::backend_handle_adoption::current_daemon_process(&paths, Some(30))
        .expect("daemon identity");
    let daemon_dir = crate::cache_lib::soldr_daemon_dir(&paths);
    write_claim(
        &daemon_dir
            .join("generations")
            .join("soldr-daemon-0.9.25-abc"),
        &daemon,
    );

    let owners = recorded_generation_owners(&paths);
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].generation, "soldr-daemon-0.9.25-abc");
    assert_eq!(owners[0].pid, daemon.pid);

    let root = paths.root.display();
    let text = describe_unrecorded_owner(&root, &owners, |_| true);
    assert!(text.contains(&format!("PID {}", daemon.pid)), "{text}");
    assert!(text.contains("soldr-daemon-0.9.25-abc"), "{text}");
    assert!(text.contains("SOLDR_CACHE_DIR"), "{text}");
    assert!(text.contains("soldr broker remove"), "{text}");
}

#[test]
fn dead_or_absent_claims_fall_back_to_the_hedged_message() {
    let temp = TempDir::new().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    assert!(recorded_generation_owners(&paths).is_empty());

    let root = paths.root.display();
    let none = describe_unrecorded_owner(&root, &[], |_| true);
    assert!(none.contains("no daemon route claim"), "{none}");
    for remedy in ["SOLDR_CACHE_DIR", "soldr broker remove", "soldr status"] {
        assert!(none.contains(remedy), "{remedy}: {none}");
    }

    let dead = [GenerationOwner {
        generation: "g".into(),
        pid: 4_000_000,
        exe: "/x".into(),
    }];
    let text = describe_unrecorded_owner(&root, &dead, |_| false);
    assert!(text.contains("no daemon route claim"), "{text}");
}
