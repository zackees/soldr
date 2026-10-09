//! Host-fitness policy for the catalogue GNU/Linux compiler bundle.
//!
//! Split from `target_lifecycle.rs` (soldr#3435) so the host-shape decision
//! sits beside its musl sibling and below the production file ceiling.

use crate::fetch::catalogue_linux_host::{BundleHostFitness, GNU_BUNDLE_HOST};

/// What to do about the catalogue GNU/Linux bundle for one prepare call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GnuBundleDecision {
    /// Fetch the bundle and export its compiler/sysroot environment.
    UseCatalogue,
    /// The bundle cannot execute here, but the host's own GNU toolchain
    /// already targets this triple. Export nothing and let cc-rs and rustc
    /// find the host `cc`, exactly as they would without soldr in the
    /// picture.
    UseHostCompiler,
    /// Refuse, with a message that names both shapes.
    Reject(String),
}

/// Decide, from host shape and target shape, whether the catalogue GNU
/// bundle applies.
///
/// soldr#2874: the same class of bug as soldr#2437, one level down. That
/// guard asks whether the host *OS* can execute a Linux ELF; this asks
/// whether the host *architecture* (and, soldr#3435, the OS libc) can. Both catalogue bundles are
/// x86_64-hosted -- `linux-arm64-gnu` is a cross compiler that emits ARM64,
/// not an ARM64-hosted native one -- so selecting by target shape alone
/// handed a native `ubuntu-24.04-arm` runner an x86_64 compiler, which died
/// with `Exec format error (os error 8)` hundreds of megabytes and one
/// `-sys` crate later.
///
/// `force_runnable` is the existing `SOLDR_WINDOWS_LINUX_CROSS_GUARD=off`
/// test seam, not a second one. Its whole purpose is already "this host is
/// pretending it can run the Linux bundle": `cli_build_fetch_overlap` and
/// `prepare_env_contract_tests` both seed a fake bundle out of `#!/bin/sh`
/// stubs and exercise the catalogue path from hosts that could never execute
/// a real one -- macOS and Windows lanes for the first, and a native ARM64
/// runner for the second, whose fixture deliberately targets the other
/// architecture. That is the same claim this function evaluates, so it takes
/// the same switch rather than growing a parallel one.
///
/// Pure, and taking the host's fitness as an argument rather than reading
/// `cfg!`, because the hosts this has to be correct for -- native ARM64
/// Linux, Alpine -- are not the hosts soldr's tests run on. The impure edge
/// computes `fitness` with
/// [`catalogue_linux_host::bundle_host_fitness`](crate::fetch::catalogue_linux_host::bundle_host_fitness)
/// and [`GNU_BUNDLE_HOST`].
pub(crate) fn decide_gnu_bundle(
    fitness: BundleHostFitness,
    host_triple: &str,
    target: &str,
    base: &str,
    glibc_floor: Option<&str>,
    force_runnable: bool,
) -> GnuBundleDecision {
    if force_runnable || fitness.is_runnable() {
        return GnuBundleDecision::UseCatalogue;
    }
    if let Some(floor) = glibc_floor {
        // An explicit floor is a request for the pinned sysroot
        // specifically. Building against the host's own newer glibc would
        // produce an artifact that claims a floor it does not have, which is
        // worse than refusing.
        return GnuBundleDecision::Reject(gnu_bundle_host_message(
            fitness,
            target,
            host_triple,
            Some(floor),
        ));
    }
    if base == host_triple {
        GnuBundleDecision::UseHostCompiler
    } else {
        // A cross-build that needs the bundle, from a host that cannot run
        // it. Nothing stands in.
        GnuBundleDecision::Reject(gnu_bundle_host_message(fitness, target, host_triple, None))
    }
}

/// Why a host that cannot execute the catalogue GNU bundle is being told no.
///
/// soldr#2874 asks for a *precise* unsupported-host diagnostic rather than
/// the `Exec format error (os error 8)` that surfaces hundreds of megabytes
/// and one `-sys` crate later. Precise here means naming both shapes: the
/// bundle is selected by target shape and constrained by host shape, and the
/// whole defect was those two being conflated. A musl host of the right
/// architecture gets the libc reason instead (soldr#3435).
pub(super) fn gnu_bundle_host_message(
    fitness: BundleHostFitness,
    target: &str,
    host: &str,
    floor: Option<&str>,
) -> String {
    let bundle_host = GNU_BUNDLE_HOST.host;
    if fitness == BundleHostFitness::WrongLibc {
        let floor_note = floor
            .map(|floor| format!(" The glibc {floor} floor needs that bundle's pinned sysroot."))
            .unwrap_or_default();
        return format!(
            "cannot prepare `{target}`: the catalogue GNU/Linux toolchain cannot run on this \
             host: {reason}.{floor_note} Build `{target}` on an `{bundle_host}` host.",
            reason = crate::fetch::catalogue_linux_host::wrong_libc_reason(host),
        );
    }
    let root_cause = format!(
        concat!(
            "the catalogue GNU/Linux toolchain cannot run on this host: every ",
            "bundle is hosted on `{bundle_host}`, and this host is `{host}` ",
            "(soldr#2874). The bundle slug names the target shape, not the ",
            "host shape -- `linux-arm64-gnu` is an x86_64-hosted cross ",
            "compiler that emits ARM64, so executing it here would fail with ",
            "`Exec format error (os error 8)`."
        ),
        bundle_host = bundle_host,
        host = host
    );
    match floor {
        Some(floor) => format!(
            concat!(
                "cannot prepare `{target}`: {root_cause} The glibc {floor} ",
                "floor comes from that bundle's pinned sysroot, so there is no ",
                "host compiler that can stand in for it. Drop the `.{floor}` ",
                "suffix to build natively against this host's glibc, or ",
                "cross-build from an `{bundle_host}` host to keep the floor."
            ),
            target = target,
            root_cause = root_cause,
            floor = floor,
            bundle_host = bundle_host
        ),
        None => format!(
            concat!(
                "cannot prepare `{target}`: {root_cause} Cross-building ",
                "`{target}` needs that bundle, and no host compiler stands in ",
                "for it. Build on an `{bundle_host}` host, or build ",
                "`{host}` natively on this one."
            ),
            target = target,
            root_cause = root_cause,
            bundle_host = bundle_host,
            host = host
        ),
    }
}
