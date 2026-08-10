use crate::signal_bool::NativeSignalBool;

// ── NativeSignalBool tests (no PyO3 needed) ──

#[test]
fn signal_bool_default_false() {
    let sb = NativeSignalBool::new(false);
    assert!(!sb.load_nolock());
}

#[test]
fn signal_bool_default_true() {
    let sb = NativeSignalBool::new(true);
    assert!(sb.load_nolock());
}

#[test]
fn signal_bool_store_and_load() {
    let sb = NativeSignalBool::new(false);
    sb.store_locked(true);
    assert!(sb.load_nolock());
    sb.store_locked(false);
    assert!(!sb.load_nolock());
}

#[test]
fn signal_bool_compare_and_swap_success() {
    let sb = NativeSignalBool::new(false);
    assert!(sb.compare_and_swap_locked(false, true));
    assert!(sb.load_nolock());
}

#[test]
fn signal_bool_compare_and_swap_failure() {
    let sb = NativeSignalBool::new(false);
    assert!(!sb.compare_and_swap_locked(true, false));
    assert!(!sb.load_nolock());
}

// ── NativeSignalBool additional tests ──

#[test]
fn signal_bool_concurrent_access() {
    let sb = NativeSignalBool::new(false);
    let sb_clone = sb.clone();

    let handle = std::thread::spawn(move || {
        sb_clone.store_locked(true);
    });
    handle.join().unwrap();
    assert!(sb.load_nolock());
}

#[test]
fn signal_bool_new_default_false() {
    assert!(!NativeSignalBool::new(false).load_nolock());
}

#[test]
fn signal_bool_new_true() {
    assert!(NativeSignalBool::new(true).load_nolock());
}

#[test]
fn signal_bool_store_locked_changes_value() {
    let sb = NativeSignalBool::new(false);
    sb.store_locked(true);
    assert!(sb.load_nolock());
}

#[test]
fn signal_bool_compare_and_swap_success_iter3() {
    let sb = NativeSignalBool::new(false);
    assert!(sb.compare_and_swap_locked(false, true));
    assert!(sb.load_nolock());
}
