//! Cargo-authority regressions for the retired workspace trampoline (#1528).
//!
//! A broken Cargo executable proves that soldr invokes Cargo instead of
//! returning a false Fresh result from its former workspace sidecar.

use crate::common;

use crate::common::*;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn make_project(label: &str) -> PathBuf {
    let dir = unique_temp_dir(label);
    fs::create_dir_all(dir.join("src")).expect("create src");
    fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "workspace_trampoline_demo"
version = "0.1.0"
edition = "2021"

[lib]
path = "src/lib.rs"

[[bin]]
name = "workspace_trampoline_demo"
path = "src/main.rs"
"#,
    )
    .expect("write manifest");
    fs::write(
        dir.join("src/lib.rs"),
        "pub fn name() -> &'static str { \"workspace\" }\n",
    )
    .expect("write lib");
    fs::write(
        dir.join("src/main.rs"),
        "fn main() { println!(\"{}\", workspace_trampoline_demo::name()); }\n",
    )
    .expect("write main");
    dir
}

fn broken_cargo_stub(dir: &Path) -> PathBuf {
    let path = fake_script_path(dir, "broken-cargo");
    let body = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\necho broken cargo invoked 1>&2\nexit /b 99\n"
    } else {
        "#!/bin/sh\necho 'broken cargo invoked' >&2\nexit 99\n"
    };
    write_fake_script(&path, body);
    path
}

fn run_soldr<I, S>(project: &Path, env: &[(&str, &str)], args: I) -> std::process::Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(common::soldr_bin());
    command.current_dir(project).args(args);
    command.env("SOLDR_CACHE_DIR", project.join(".soldr-cache"));
    // This legacy test-only switch used to force the unsafe path. Keep it set
    // to prove no hidden opt-in can restore the retired freshness oracle.
    command.env("SOLDR_TEST_FORCE_WORKSPACE_TRAMPOLINE", "1");
    command.env_remove("CARGO_TARGET_DIR");
    command.env_remove("SOLDR_TARGET_CACHE_MODE");
    command.env_remove("SOLDR_BUILD_CACHE_MODE");
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("spawn soldr")
}

fn seed(project: &Path, verb: &str) -> bool {
    let out = run_soldr(project, &[], ["--no-cache", "cargo", verb]);
    if !out.status.success() {
        eprintln!(
            "seed {verb} unavailable: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.status.success()
}

fn built_binary(project: &Path) -> PathBuf {
    let name = format!("workspace_trampoline_demo{}", std::env::consts::EXE_SUFFIX);
    let direct = project.join("target/debug").join(&name);
    if direct.is_file() {
        return direct;
    }
    fs::read_dir(project.join("target"))
        .expect("read target")
        .flatten()
        .map(|entry| entry.path().join("debug").join(&name))
        .find(|path| path.is_file())
        .expect("built workspace binary")
}

fn rewrite_preserving_stat(path: &Path, transform: impl FnOnce(Vec<u8>) -> Vec<u8>) {
    let metadata = fs::metadata(path).expect("metadata before rewrite");
    let modified = metadata.modified().expect("mtime before rewrite");
    let rewritten = transform(fs::read(path).expect("read before rewrite"));
    assert_eq!(rewritten.len(), metadata.len() as usize);
    fs::write(path, rewritten).expect("rewrite file");
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(modified))
        .expect("restore mtime");
}

fn assert_invokes_cargo(project: &Path, args: &[&str], context: &str) {
    let stub_dir = unique_temp_dir("ws-cargo-authority-stub");
    let broken = broken_cargo_stub(&stub_dir);
    let broken = broken.to_string_lossy().into_owned();
    let out = run_soldr(
        project,
        &[("SOLDR_TEST_CARGO_BIN", &broken)],
        args.iter().copied(),
    );
    assert_eq!(
        out.status.code(),
        Some(99),
        "{context}: soldr did not delegate freshness to Cargo\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn same_size_same_mtime_source_mutation_invokes_cargo() {
    let project = make_project("ws-authority-source");
    assert!(seed(&project, "build"));
    rewrite_preserving_stat(&project.join("src/lib.rs"), |bytes| {
        String::from_utf8(bytes)
            .expect("utf8 source")
            .replace("workspace", "different")
            .into_bytes()
    });
    assert_invokes_cargo(
        &project,
        &["--no-cache", "cargo", "build"],
        "same-stat source mutation",
    );
}

#[test]
fn same_size_same_mtime_output_mutation_invokes_cargo() {
    let project = make_project("ws-authority-output");
    assert!(seed(&project, "build"));
    let output = built_binary(&project);
    rewrite_preserving_stat(&output, |mut bytes| {
        bytes[0] ^= 0xff;
        bytes
    });
    assert_invokes_cargo(
        &project,
        &["--no-cache", "cargo", "build"],
        "same-stat output mutation",
    );
}

#[test]
fn same_size_same_mtime_manifest_mutation_invokes_cargo() {
    let project = make_project("ws-authority-manifest");
    assert!(seed(&project, "build"));
    rewrite_preserving_stat(&project.join("Cargo.toml"), |bytes| {
        String::from_utf8(bytes)
            .expect("utf8 manifest")
            .replace("version = \"0.1.0\"", "version = \"0.1.1\"")
            .into_bytes()
    });
    assert_invokes_cargo(
        &project,
        &["--no-cache", "cargo", "build"],
        "same-stat manifest mutation",
    );
}

#[test]
fn same_size_same_mtime_lockfile_mutation_invokes_cargo() {
    let project = make_project("ws-authority-lockfile");
    assert!(seed(&project, "build"));
    rewrite_preserving_stat(&project.join("Cargo.lock"), |bytes| {
        String::from_utf8(bytes)
            .expect("utf8 lockfile")
            .replace("version = \"0.1.0\"", "version = \"0.1.1\"")
            .into_bytes()
    });
    assert_invokes_cargo(
        &project,
        &["--no-cache", "cargo", "build"],
        "same-stat lockfile mutation",
    );
}

#[test]
fn changed_clippy_policy_invokes_cargo() {
    let project = make_project("ws-authority-clippy-policy");
    if !seed(&project, "clippy") {
        return;
    }
    assert_invokes_cargo(
        &project,
        &["--no-cache", "cargo", "clippy", "--", "-D", "warnings"],
        "changed clippy trailing policy",
    );
}

#[test]
fn legacy_no_trampoline_flag_is_not_forwarded_to_cargo() {
    let project = make_project("ws-authority-legacy-flag");
    assert!(seed(&project, "build"));
    assert_invokes_cargo(
        &project,
        &["--no-cache", "cargo", "build", "--no-trampoline"],
        "legacy opt-out cleanup",
    );
}
