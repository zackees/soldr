use super::*;
use crate::cache_lib::soldr_daemon_dir;
use tempfile::TempDir;

#[test]
fn prune_if_unchanged_keeps_a_claim_replaced_after_the_read() {
    let temp = TempDir::new().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("root"));
    std::fs::create_dir_all(soldr_daemon_dir(&paths)).expect("daemon dir");
    let claim_path = broker_route_claim_path(&paths);
    std::fs::write(&claim_path, b"claim A").expect("claim A");
    let read_a = std::fs::read(&claim_path).expect("read A");
    std::fs::write(&claim_path, b"claim B").expect("claim B");

    let pruned = prune_broker_route_claim_if_unchanged(&paths, &read_a).expect("prune");
    assert!(
        !pruned,
        "a claim replaced after the read must not be pruned"
    );
    assert_eq!(std::fs::read(&claim_path).expect("B survives"), b"claim B");
    let leftovers: Vec<_> = std::fs::read_dir(claim_path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains("tombstone"))
        .collect();
    assert!(leftovers.is_empty(), "tombstone must be cleaned up");

    let read_b = std::fs::read(&claim_path).expect("read B");
    assert!(prune_broker_route_claim_if_unchanged(&paths, &read_b).expect("prune B"));
    assert!(!claim_path.exists());
}
