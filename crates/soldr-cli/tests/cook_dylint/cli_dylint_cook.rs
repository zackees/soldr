use crate::common;

use std::process::Command;

#[test]
fn dylint_cook_help_is_first_class_and_check_shaped() {
    let output = Command::new(common::soldr_bin())
        .args(["dylint", "cook", "--help"])
        .output()
        .expect("run help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("soldr dylint cook"));
    assert!(stdout.contains("check-shaped"));
    assert!(stdout.contains("--plan-only"));
    assert!(stdout.contains("--toolchain"));
    // soldr#3042: the tests-tree surface is only discoverable through help,
    // and CI's driver (`.github/scripts/cook_dylint_tests_tree.py`) depends
    // on both flags existing.
    assert!(stdout.contains("--tree"));
    assert!(stdout.contains("--target-root"));
}
