use super::*;

#[test]
fn bare_channel_appends_host_triple() {
    assert_eq!(
        toolchain_dir_name("1.95.0", "x86_64-unknown-linux-gnu"),
        "1.95.0-x86_64-unknown-linux-gnu"
    );
    assert_eq!(
        toolchain_dir_name("nightly-2026-04-16", "aarch64-apple-darwin"),
        "nightly-2026-04-16-aarch64-apple-darwin"
    );
}

#[test]
fn host_qualified_channel_maps_to_itself() {
    assert_eq!(
        toolchain_dir_name("1.95.0-x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc"),
        "1.95.0-x86_64-pc-windows-msvc"
    );
}

#[test]
fn empty_host_keeps_bare_channel() {
    // platform::host::facts::triple() yields "" on an unsupported host;
    // the probe then simply misses and the caller runs the idempotent
    // install rather than constructing a bogus "<channel>-" name.
    assert_eq!(toolchain_dir_name("1.95.0", ""), "1.95.0");
}

#[test]
fn classification_matrix() {
    assert_eq!(classify(true, true), ToolchainReadiness::Ready);
    // rustc present without its directory cannot happen on a real
    // filesystem, but Ready must win any inconsistent read.
    assert_eq!(classify(false, true), ToolchainReadiness::Ready);
    assert_eq!(classify(true, false), ToolchainReadiness::Partial);
    assert_eq!(classify(false, false), ToolchainReadiness::Missing);
}

#[test]
fn probe_reports_missing_partial_and_ready() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path();
    let host = "x86_64-unknown-linux-gnu";

    assert_eq!(
        probe_toolchain_state(home, "1.95.0", host),
        ToolchainReadiness::Missing
    );

    // The wedge signature from soldr#2618: component manifests landed,
    // rustc never did.
    let toolchain = home.join("toolchains").join("1.95.0-x86_64-unknown-linux-gnu");
    std::fs::create_dir_all(toolchain.join("lib").join("rustlib")).expect("mkdir");
    std::fs::write(
        toolchain
            .join("lib")
            .join("rustlib")
            .join("manifest-rust-std-x86_64-unknown-linux-gnu"),
        b"",
    )
    .expect("write manifest");
    assert_eq!(
        probe_toolchain_state(home, "1.95.0", host),
        ToolchainReadiness::Partial
    );

    let bin = toolchain.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir bin");
    let rustc = if crate::platform::host::facts::os()
        == crate::platform::host::facts::HostOs::Windows
    {
        bin.join("rustc.exe")
    } else {
        bin.join("rustc")
    };
    std::fs::write(&rustc, b"").expect("write rustc");
    assert_eq!(
        probe_toolchain_state(home, "1.95.0", host),
        ToolchainReadiness::Ready
    );
}
