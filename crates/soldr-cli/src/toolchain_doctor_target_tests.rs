//! soldr#3695: the shared-target probe resolves the target dir through the
//! canonical `cargo_target_dir` resolver, not `cwd/target`.

use super::*;
use crate::core::cargo_target_dir::CargoTargetDirInputs;
use std::fs;

fn tempdir(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("soldr-doctor-3695-{label}-{nanos}"));
    fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn populate(target: &Path) {
    let entry = target.join("debug").join(".fingerprint").join("c-abc");
    fs::create_dir_all(&entry).expect("mkdir fingerprint");
    fs::write(entry.join("invoked.timestamp"), "").expect("seed");
}

/// Hermetic inputs: no env leaks in, CARGO_HOME pinned to an empty dir.
fn inputs(cwd: &Path, cargo_target_dir: Option<&Path>, home: &Path) -> CargoTargetDirInputs {
    CargoTargetDirInputs {
        cwd: cwd.to_path_buf(),
        args: Vec::new(),
        cargo_target_dir: cargo_target_dir.map(|p| p.as_os_str().to_owned()),
        cargo_build_target_dir: None,
        cargo_home: Some(home.to_path_buf()),
    }
}

#[test]
fn honours_cargo_target_dir() {
    let root = tempdir("env");
    let (cwd, target, home) = (root.join("ws"), root.join("elsewhere"), root.join("home"));
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::write(cwd.join("Cargo.toml"), "[package]\nname=\"a\"\n").unwrap();
    populate(&target);

    let probe = probe_shared_target_warning_for(&inputs(&cwd, Some(&target), &home));
    assert_eq!(
        probe.details["would_warn"],
        Value::from(true),
        "{}",
        probe.details
    );
    assert_eq!(
        probe.details["target_dir"],
        Value::from(target.display().to_string())
    );
}

#[test]
fn member_crate_cwd_finds_workspace_root_target() {
    let root = tempdir("member");
    let (ws, home) = (root.join("ws"), root.join("home"));
    let member = ws.join("crates").join("m");
    fs::create_dir_all(&member).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers=[\"crates/m\"]\n",
    )
    .unwrap();
    fs::write(member.join("Cargo.toml"), "[package]\nname=\"m\"\n").unwrap();
    populate(&ws.join("target"));

    let probe = probe_shared_target_warning_for(&inputs(&member, None, &home));
    assert_eq!(
        probe.details["would_warn"],
        Value::from(true),
        "{}",
        probe.details
    );
}

#[test]
fn no_manifest_falls_back_to_cwd_target() {
    let root = tempdir("bare");
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let probe = probe_shared_target_warning_for(&inputs(&root, None, &home));
    assert_eq!(probe.details["reason"], Value::from("no-target-dir"));
    assert_eq!(
        probe.details["target_dir"],
        Value::from(root.join("target").display().to_string())
    );
}
