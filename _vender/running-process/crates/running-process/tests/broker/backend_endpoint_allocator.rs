#![cfg(feature = "client")]

use running_process::broker::lifecycle::names::backend_pipe;
use running_process::broker::server::{
    BackendEndpointAllocator, BackendEndpointAllocatorError, DEFAULT_BACKEND_ENDPOINT_ATTEMPTS,
};

const USER_HASH: &str = "deadbeefdeadbeef";

#[test]
fn allocator_returns_endpoint_in_requested_namespace() {
    let mut allocator = BackendEndpointAllocator::new(USER_HASH, "shared");

    let endpoint = allocator
        .allocate_with_random128(|| Ok([0xAB_u8; 16]))
        .unwrap();

    assert_eq!(endpoint.namespace_id, "shared");
    assert_eq!(
        endpoint.path,
        pick_one(&backend_pipe(USER_HASH, &[0xAB_u8; 16]).unwrap())
    );
}

#[test]
fn allocator_reserves_allocated_paths() {
    let mut allocator = BackendEndpointAllocator::new(USER_HASH, "shared");
    let mut values = [[0_u8; 16], [1_u8; 16]].into_iter();

    let first = allocator
        .allocate_with_random128(|| Ok(values.next().unwrap()))
        .unwrap();
    let second = allocator
        .allocate_with_random128(|| Ok(values.next().unwrap()))
        .unwrap();

    assert_ne!(first.path, second.path);
}

#[test]
fn allocator_retries_reserved_path_collision() {
    let collision = pick_one(&backend_pipe(USER_HASH, &[0_u8; 16]).unwrap());
    let expected = pick_one(&backend_pipe(USER_HASH, &[1_u8; 16]).unwrap());
    let mut allocator = BackendEndpointAllocator::new(USER_HASH, "shared");
    allocator.reserve_path(collision);
    let mut values = [[0_u8; 16], [1_u8; 16]].into_iter();

    let endpoint = allocator
        .allocate_with_random128(|| Ok(values.next().unwrap()))
        .unwrap();

    assert_eq!(endpoint.path, expected);
}

#[test]
fn allocator_errors_after_collision_budget_is_exhausted() {
    let collision = pick_one(&backend_pipe(USER_HASH, &[0_u8; 16]).unwrap());
    let mut allocator = BackendEndpointAllocator::new(USER_HASH, "shared").with_max_attempts(2);
    allocator.reserve_path(collision);

    let err = allocator
        .allocate_with_random128(|| Ok([0_u8; 16]))
        .unwrap_err();

    assert!(matches!(
        err,
        BackendEndpointAllocatorError::CollisionExhausted { attempts: 2 }
    ));
}

#[test]
fn allocator_uses_default_retry_budget() {
    assert_eq!(DEFAULT_BACKEND_ENDPOINT_ATTEMPTS, 16);
}

#[test]
fn allocator_rejects_invalid_user_hash() {
    let mut allocator = BackendEndpointAllocator::new("not-16-chars", "shared");

    let err = allocator
        .allocate_with_random128(|| Ok([0_u8; 16]))
        .unwrap_err();

    assert!(matches!(err, BackendEndpointAllocatorError::PipePath(_)));
}

fn pick_one(p: &running_process::broker::lifecycle::names::PipePath) -> String {
    match (&p.windows, &p.unix) {
        (Some(w), None) => w.clone(),
        (None, Some(u)) => u.to_string_lossy().into_owned(),
        _ => panic!("exactly one of windows/unix must be Some"),
    }
}
