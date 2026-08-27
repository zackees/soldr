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
}
