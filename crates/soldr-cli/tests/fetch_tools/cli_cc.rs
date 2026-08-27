//! Linux acceptance coverage for the standalone compiler surface (soldr#2335).

use crate::common;
use std::process::{Command, Output};

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "network-touching; opt in with SOLDR_TEST_NETWORK=1 soldr cargo test -p soldr-cli --test fetch_tools -- --ignored cli_cc::"]
fn standalone_cc_compiles_direct_and_cmake_projects() {
    if std::env::var_os("SOLDR_TEST_NETWORK").is_none() {
        eprintln!("skipping: SOLDR_TEST_NETWORK not set");
        return;
    }
    if !matches!(
        (
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::arch()
        ),
        (
            soldr_platform::host::facts::HostOs::Linux,
            soldr_platform::host::facts::HostArch::X86_64
        )
    ) {
        eprintln!("skipping standalone compiler acceptance test outside x86_64 Linux");
        return;
    }

    let soldr = common::soldr_bin();
    let fixture = common::workspace_root().join("ci/fixtures/soldr-cc-cmake");
    let temp = tempfile::tempdir().expect("create standalone compiler test directory");
    let direct_binary = temp.path().join("soldr-cc-hello");

    let direct = Command::new(&soldr)
        .args(["cc", "--target", "x86_64-linux-gnu.2.17"])
        .arg(fixture.join("hello.c"))
        .arg("-o")
        .arg(&direct_binary)
        .output()
        .expect("run soldr cc");
    assert_success("direct soldr cc compile", &direct);

    let direct_run = Command::new(&direct_binary)
        .output()
        .expect("run direct C executable");
    assert_success("direct C executable", &direct_run);
    assert_eq!(
        String::from_utf8_lossy(&direct_run.stdout).trim(),
        "hello from soldr cc"
    );

    let build = temp.path().join("cmake-build");
    let cc = format!("{} cc --target x86_64-linux-gnu.2.17", soldr.display());
    let cxx = format!("{} c++ --target x86_64-linux-gnu.2.17", soldr.display());
    let configure = Command::new("cmake")
        .args(["-S", fixture.to_str().unwrap(), "-B"])
        .arg(&build)
        .env("CC", cc)
        .env("CXX", cxx)
        .output()
        .expect("run CMake configure");
    assert_success("CMake configure through soldr", &configure);

    let cmake_build = Command::new("cmake")
        .arg("--build")
        .arg(&build)
        .output()
        .expect("run CMake build");
    assert_success("CMake build through soldr", &cmake_build);

    let cmake_run = Command::new(build.join("soldr-cc-cmake"))
        .output()
        .expect("run CMake C++ executable");
    assert_success("CMake C++ executable", &cmake_run);
    assert_eq!(
        String::from_utf8_lossy(&cmake_run.stdout).trim(),
        "hello from soldr cc"
    );
}
