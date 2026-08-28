//! Integration test: bootstrap crgx via soldr-fetch.
//!
//! This proves the full chain:
//!   soldr resolves the active project target → queries crates.io →
//!   downloads from GitHub Releases → extracts → binary runs.
//!
//! Requires network access. Run with:
//!   soldr cargo test -p soldr-cli --test fetch_tools fetch_crgx::

use soldr_cli::fetch::{fetch_tool, VersionSpec};

#[tokio::test]
async fn fetch_crgx_and_run() {
    const CRGX_VERSION: &str = "0.1.0";

    // #692: upstream crgx 0.1.0 publishes Linux x64/aarch64/musl,
    // macOS x64/aarch64, and Windows x64 assets -- but NOT a Windows
    // ARM64 (aarch64-pc-windows-msvc) asset. The failing run logs the
    // full asset list. Skip on Windows ARM64 until upstream ships one;
    // the rest of the matrix still exercises the fetch chain.
    if matches!(
        (
            soldr_platform::host::facts::os(),
            soldr_platform::host::facts::arch()
        ),
        (
            soldr_platform::host::facts::HostOs::Windows,
            soldr_platform::host::facts::HostArch::Aarch64
        )
    ) {
        eprintln!(
            "skipping fetch_crgx_and_run on aarch64-pc-windows-msvc: \
             upstream crgx {CRGX_VERSION} has no Windows ARM64 asset (see #692)"
        );
        return;
    }

    // Fetch a pinned crgx release for the current platform.
    let result = fetch_tool("crgx", &VersionSpec::Exact(CRGX_VERSION.into()))
        .await
        .expect("failed to fetch crgx");

    println!("binary: {}", result.binary_path.display());
    println!("version: {}", result.version);
    println!("cached: {}", result.cached);

    assert!(
        result.binary_path.exists(),
        "binary not found at {}",
        result.binary_path.display()
    );

    // Run it to prove it's a valid binary for this platform
    let output = std::process::Command::new(&result.binary_path)
        .arg("--help")
        .output()
        .expect("failed to execute crgx");

    assert!(
        output.status.success(),
        "crgx --help failed with status {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("crgx") || stdout.contains("crate"),
        "unexpected --help output: {stdout}"
    );

    // Second fetch should hit cache
    let cached = fetch_tool("crgx", &VersionSpec::Exact(result.version.clone()))
        .await
        .expect("second fetch failed");

    assert!(cached.cached, "second fetch should have been cached");
    assert_eq!(cached.binary_path, result.binary_path);
}
