//! Integration test: bootstrap crgx via soldr-fetch.
//!
//! This proves the full chain:
//!   soldr detects target (MSVC on Windows) → queries crates.io →
//!   downloads from GitHub Releases → extracts → binary runs.
//!
//! Requires network access. Run with:
//!   cargo test -p soldr-fetch --test fetch_crgx

use soldr_fetch::{fetch_tool, VersionSpec};

#[tokio::test]
async fn fetch_crgx_and_run() {
    const CRGX_VERSION: &str = "0.1.0";

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
