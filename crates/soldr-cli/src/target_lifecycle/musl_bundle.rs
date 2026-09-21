//! Host-fitness policy for catalogue musl/Linux compiler bundles.
//!
//! Kept separate from the target lifecycle entry point so the host-shape
//! decision remains small, testable, and below the production file ceiling.

/// What to do about the catalogue musl/Linux bundle for one prepare call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MuslBundleDecision {
    /// Fetch the bundle and export its compiler/sysroot environment.
    UseCatalogue,
    /// Refuse before downloading an x86_64 compiler that cannot run here.
    Reject(String),
}

/// Decide whether the catalogue musl/Linux compiler can run on this host.
///
/// Unlike a native GNU build, a glibc host compiler cannot stand in for a musl
/// target: the musl CRT, libc, and target-prefixed binutils come from the
/// catalogue bundle. A host that cannot execute that bundle must fail before
/// Cargo can accidentally apply its sysroot to host build scripts.
pub(crate) fn decide_musl_bundle(
    host_os: crate::platform::host::facts::HostOs,
    host_arch: crate::platform::host::facts::HostArch,
    host_triple: &str,
    target: &str,
    force_runnable: bool,
) -> MuslBundleDecision {
    if force_runnable
        || crate::fetch::musl_linux_toolchain::bundle_host_fitness(host_os, host_arch).is_runnable()
    {
        MuslBundleDecision::UseCatalogue
    } else {
        MuslBundleDecision::Reject(musl_bundle_host_message(target, host_triple))
    }
}

/// Explain why an unsupported host cannot use a musl bundle.
fn musl_bundle_host_message(target: &str, host: &str) -> String {
    format!(
        concat!(
            "cannot prepare `{target}`: the catalogue musl/Linux toolchain ",
            "cannot run on this host. Every bundle is hosted on `{bundle_host}`, ",
            "but this host is `{host}` (soldr#3296). The bundle slug names the ",
            "target shape, not the host shape -- `linux-arm64-musl` is an ",
            "x86_64-hosted cross compiler that emits ARM64, so executing it here ",
            "would fail with `Exec format error (os error 8)`. Unlike GNU/Linux, ",
            "there is no host compiler fallback because the musl CRT and sysroot ",
            "come from the bundle; build from an `{bundle_host}` host."
        ),
        target = target,
        host = host,
        bundle_host = crate::fetch::musl_linux_toolchain::MUSL_LINUX_TOOLCHAIN_HOST_TRIPLE,
    )
}
