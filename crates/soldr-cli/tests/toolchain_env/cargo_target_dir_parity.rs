//! soldr#3203: the shared target-dir resolver agrees with `cargo metadata`.
//!
//! The resolver's rules were measured, not recalled, and this keeps them
//! measured. Each case builds a fresh fixture, asks the real Cargo capability
//! for `target_directory`, and asks `resolve_cargo_target_dir` the same question
//! with the same working directory, arguments and environment.

use std::path::{Path, PathBuf};
use std::process::Command;

use soldr_cli::core::cargo_target_dir::{resolve_cargo_target_dir, CargoTargetDirInputs};

use crate::common;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write fixture file");
}

fn package(dir: &Path, name: &str, extra: &str) {
    write(
        &dir.join("Cargo.toml"),
        &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}"),
    );
    write(&dir.join("src").join("lib.rs"), "pub fn f() {}\n");
}

/// An outer workspace excluding a nested one, a member, and a member reaching
/// its root through `package.workspace`. Carries the repo's toolchain channel
/// so the CI Cargo shim (the source Soldr front door) accepts it.
fn fixture() -> PathBuf {
    let root = std::fs::canonicalize(common::unique_temp_dir("cargo-target-dir-parity"))
        .expect("canonical fixture root");
    // Channel only: the repository pin also lists components, and under CI's
    // Cargo shim the front door would try to add them -- a download the
    // soldr#3195 tripwire rightly refuses. `cargo metadata` needs the toolchain.
    let channel = soldr_cli::core::read_rust_toolchain_manifest(&common::workspace_root())
        .expect("read repository toolchain pin")
        .channel
        .expect("repository pins a channel");
    write(
        &root.join("rust-toolchain.toml"),
        &format!("[toolchain]\nchannel = \"{channel}\"\n"),
    );
    write(
        &root.join("Cargo.toml"),
        "[workspace]\nmembers = []\nexclude = [\"ws\"]\n",
    );
    write(
        &root.join("ws").join("Cargo.toml"),
        "[workspace]\nmembers = [\"m\", \"m/inner\"]\n",
    );
    package(&root.join("ws").join("m"), "m", "");
    package(
        &root.join("ws").join("m").join("inner"),
        "inner",
        "workspace = \"../..\"\n",
    );
    root
}

struct Case {
    name: &'static str,
    cwd: &'static str,
    args: &'static [&'static str],
    target_dir_env: Option<&'static str>,
    build_target_dir_env: Option<&'static str>,
    /// `(path under the fixture root, contents)`.
    files: &'static [(&'static str, &'static str)],
}

const WS_CONFIG: (&str, &str) = (
    "ws/.cargo/config.toml",
    "[build]\ntarget-dir = \"cfg-out\"\n",
);
const MEMBER_CONFIG: (&str, &str) = (
    "ws/m/.cargo/config.toml",
    "[build]\ntarget-dir = \"../member-cfg\"\n",
);

const CASES: &[Case] = &[
    Case {
        name: "member, defaults",
        cwd: "ws/m",
        args: &[],
        target_dir_env: None,
        build_target_dir_env: None,
        files: &[],
    },
    Case {
        name: "workspace root, defaults",
        cwd: "ws",
        args: &[],
        target_dir_env: None,
        build_target_dir_env: None,
        files: &[],
    },
    Case {
        name: "config above the member",
        cwd: "ws/m",
        args: &[],
        target_dir_env: None,
        build_target_dir_env: None,
        files: &[WS_CONFIG],
    },
    Case {
        name: "config is found from cwd, not the manifest",
        cwd: "",
        args: &["--manifest-path", "ws/m/Cargo.toml"],
        target_dir_env: None,
        build_target_dir_env: None,
        files: &[WS_CONFIG],
    },
    Case {
        name: "nearest config wins, unnormalized",
        cwd: "ws/m",
        args: &[],
        target_dir_env: None,
        build_target_dir_env: None,
        files: &[WS_CONFIG, MEMBER_CONFIG],
    },
    Case {
        name: "CARGO_BUILD_TARGET_DIR beats config",
        cwd: "ws/m",
        args: &[],
        target_dir_env: None,
        build_target_dir_env: Some("envb"),
        files: &[WS_CONFIG, MEMBER_CONFIG],
    },
    Case {
        name: "CARGO_TARGET_DIR beats CARGO_BUILD_TARGET_DIR",
        cwd: "ws/m",
        args: &[],
        target_dir_env: Some("envt"),
        build_target_dir_env: Some("envb"),
        files: &[WS_CONFIG],
    },
    Case {
        name: "--config beats CARGO_BUILD_TARGET_DIR",
        cwd: "ws/m",
        args: &["--config", "build.target-dir=\"clicfg\""],
        target_dir_env: None,
        build_target_dir_env: Some("envb"),
        files: &[WS_CONFIG],
    },
    // `--target-dir` is not a `cargo metadata` flag, so its precedence is pinned
    // by the resolver's own unit tests rather than here.
    Case {
        name: "package.workspace pointer",
        cwd: "ws/m/inner",
        args: &[],
        target_dir_env: None,
        build_target_dir_env: None,
        files: &[],
    },
    Case {
        name: "legacy config beside config.toml",
        cwd: "ws/m",
        args: &[],
        target_dir_env: None,
        build_target_dir_env: None,
        files: &[
            WS_CONFIG,
            ("ws/.cargo/config", "[build]\ntarget-dir = \"legacy-out\"\n"),
        ],
    },
];

fn cargo_target_directory(root: &Path, cwd: &Path, case: &Case) -> Result<PathBuf, String> {
    let mut command = Command::new(common::cargo_bin());
    // Under CI the Cargo capability is Soldr's shim; a nested entry from a test
    // process must look like a fresh caller, not unsanctioned re-entrancy.
    common::scrub_outer_soldr_env(&mut command);
    command
        .current_dir(cwd)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .args(case.args)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR");
    if let Some(value) = case.target_dir_env {
        command.env("CARGO_TARGET_DIR", value);
    }
    if let Some(value) = case.build_target_dir_env {
        command.env("CARGO_BUILD_TARGET_DIR", value);
    }
    let output = command.output().expect("run cargo metadata");
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("metadata JSON: {error}"))?;
    metadata["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "no target_directory in cargo metadata".to_string())
}

#[test]
fn the_shared_resolver_matches_cargo_metadata() {
    let mut mismatches = Vec::new();
    for case in CASES {
        let root = fixture();
        for (path, contents) in case.files {
            write(&root.join(path), contents);
        }
        let cwd = if case.cwd.is_empty() {
            root.clone()
        } else {
            root.join(case.cwd)
        };
        let cargo = match cargo_target_directory(&root, &cwd, case) {
            Ok(cargo) => cargo,
            Err(error) => {
                mismatches.push(format!("{}: {error}", case.name));
                continue;
            }
        };
        let inputs = CargoTargetDirInputs {
            cwd: cwd.clone(),
            args: std::iter::once("metadata".to_string())
                .chain(case.args.iter().map(|arg| (*arg).to_string()))
                .collect(),
            cargo_target_dir: case.target_dir_env.map(Into::into),
            cargo_build_target_dir: case.build_target_dir_env.map(Into::into),
            cargo_home: soldr_cli::core::resolve_cargo_home(),
        };
        let resolved = resolve_cargo_target_dir(&inputs);
        if resolved.as_ref() != Some(&cargo) {
            mismatches.push(format!(
                "{}: cargo {} vs resolver {:?}",
                case.name,
                cargo.display(),
                resolved
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "resolve_cargo_target_dir disagrees with cargo metadata:\n{}",
        mismatches.join("\n")
    );
}
