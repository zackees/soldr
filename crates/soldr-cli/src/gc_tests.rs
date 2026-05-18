//! Unit tests for [`crate::gc`]. Lives in a sibling file referenced via
//! `#[path]` so `gc.rs` stays as close as possible to the original
//! production-only line count.

use super::*;

#[test]
fn gc_purge_prompt_defaults_enter_to_yes() {
    for input in ["", "\n", "y", "Y", "yes", " YES "] {
        assert!(parse_gc_purge_answer(input), "expected {input:?} to accept");
    }
    for input in ["n", "no", "anything else"] {
        assert!(!parse_gc_purge_answer(input), "expected {input:?} to skip");
    }
}

#[test]
fn gc_purge_worker_count_is_bounded() {
    assert_eq!(gc_purge_worker_count_for(0), 1);
    assert_eq!(gc_purge_worker_count_for(1), 1);
    assert_eq!(gc_purge_worker_count_for(2), 2);
    assert_eq!(gc_purge_worker_count_for(16), 4);
}
