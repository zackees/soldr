//! Which hosts can execute the catalogue Linux compiler bundles.
//!
//! One owner for the host-shape question shared by
//! [`super::gnu_linux_toolchain`] and [`super::musl_linux_toolchain`]
//! (soldr#3435). Both modules used to carry byte-identical copies of the
//! host-triple constant, the fitness enum, and the fitness predicate. Now
//! there is one predicate, [`bundle_host_fitness`], and one
//! [`BundleHostRequirement`] constant per bundle family.
//!
//! The asset slug names a bundle's *target* shape, not the shape of the
//! machine that can execute its compilers: `linux-arm64-gnu` and
//! `linux-arm64-musl` are x86_64-hosted cross compilers that emit ARM64, not
//! ARM64-native ones. Selecting a bundle by target shape alone puts an x86_64
//! ELF on an ARM64 runner, where it dies with `Exec format error (os error 8)`
//! (soldr#2874, soldr#3296).
//!
//! The two families differ in what they need from the host's libc, as
//! measured from the pinned assets for soldr#3435:
//!
//! - the GNU bundle's compilers are dynamically linked against glibc
//!   (`PT_INTERP` [`GLIBC_LOADER`], `NEEDED libc.so.6`), so they cannot start
//!   on a musl-only host such as Alpine;
//! - the musl bundle's compilers (`gcc`, `cc1`, `collect2`, `as`, `ld`) are
//!   static, `musl.cc`-built i386 ELFs with no `PT_INTERP` and no `NEEDED`
//!   entries, so they run on any x86_64 Linux kernel whatever its libc.

use crate::platform::host::facts::{HostArch, HostLibc, HostOs};

/// The ELF interpreter the catalogue GNU bundle's compilers request.
pub const GLIBC_LOADER: &str = "/lib64/ld-linux-x86-64.so.2";

/// What a catalogue Linux bundle's compilers need from the host that runs
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleHostRequirement {
    /// The CPU architecture the compiler executables are built for.
    pub arch: HostArch,
    /// Whether the compiler executables are glibc-dynamic, so the host OS
    /// must provide glibc's loader.
    pub needs_glibc: bool,
    /// The host shape, for diagnostics.
    pub host: &'static str,
}

/// Host requirement of every catalogue GNU/Linux bundle: an x86_64 glibc
/// host. This is the `host_triple: x86_64-unknown-linux-gnu` that
/// `soldr-toolchain` records, and it is literal -- the compilers are
/// glibc-dynamic.
pub const GNU_BUNDLE_HOST: BundleHostRequirement = BundleHostRequirement {
    arch: HostArch::X86_64,
    needs_glibc: true,
    host: "x86_64-unknown-linux-gnu",
};

/// Host requirement of every catalogue musl/Linux bundle: any x86_64 Linux.
///
/// `soldr-toolchain` records `host_triple: x86_64-unknown-linux-gnu` here
/// too, but only the architecture binds: the compilers are static i386 ELFs
/// (see the module docs), so a musl host runs them as well as a glibc one.
pub const MUSL_BUNDLE_HOST: BundleHostRequirement = BundleHostRequirement {
    arch: HostArch::X86_64,
    needs_glibc: false,
    host: "x86_64 Linux",
};

/// Whether this host can execute a catalogue Linux bundle's compilers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleHostFitness {
    /// The bundle's compiler executables run here.
    Runnable,
    /// A Linux host of the wrong architecture -- an x86_64 ELF on ARM64.
    /// This is soldr#2874's `Exec format error (os error 8)`.
    WrongArch,
    /// A Linux host of the right architecture whose OS libc is musl, for a
    /// bundle whose compilers are glibc-dynamic. The kernel finds no
    /// [`GLIBC_LOADER`] and the exec fails with `No such file or directory`
    /// (soldr#3435).
    WrongLibc,
    /// Not a Linux host at all, so a Linux ELF cannot be executed.
    /// soldr#2437 already stops this earlier with its own message; this arm
    /// exists so the fitness question has one answer rather than two owners.
    WrongOs,
}

impl BundleHostFitness {
    pub const fn is_runnable(self) -> bool {
        matches!(self, Self::Runnable)
    }
}

/// Can a bundle with requirement `req` execute on `(os, arch, os_libc)`?
///
/// `os_libc` must be the OS's *runtime* libc
/// (`platform::host::facts::os_libc`), not the compile-time `libc()`: soldr's
/// default Linux build is static musl, so `libc()` says musl on glibc hosts.
///
/// Pure on purpose. The hosts it has to answer for -- native ARM64 Linux,
/// Alpine -- are not the hosts soldr's tests run on, and a `cfg!`-driven
/// answer could only ever be checked by running on the machine with the bug.
pub fn bundle_host_fitness(
    req: BundleHostRequirement,
    os: HostOs,
    arch: HostArch,
    os_libc: HostLibc,
) -> BundleHostFitness {
    if os != HostOs::Linux {
        return BundleHostFitness::WrongOs;
    }
    if arch != req.arch {
        return BundleHostFitness::WrongArch;
    }
    if req.needs_glibc && os_libc == HostLibc::Musl {
        return BundleHostFitness::WrongLibc;
    }
    BundleHostFitness::Runnable
}

/// [`bundle_host_fitness`] for the machine soldr is running on: the one
/// impure edge, probing the OS's runtime libc (`facts::os_libc`).
pub fn current_host_fitness(req: BundleHostRequirement) -> BundleHostFitness {
    use crate::platform::host::facts;
    bundle_host_fitness(req, facts::os(), facts::arch(), facts::os_libc())
}

/// Why a [`BundleHostFitness::WrongLibc`] host cannot run the GNU bundle,
/// naming the host. Shared by every surface that reports it.
pub fn wrong_libc_reason(host: &str) -> String {
    format!(
        "this host (`{host}`) runs musl libc with no glibc, and the catalogue \
         GNU/Linux bundle's compilers are glibc-dynamic executables that need \
         glibc's loader `{GLIBC_LOADER}`, so they cannot start here (soldr#3435)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use BundleHostFitness::{Runnable, WrongArch, WrongLibc, WrongOs};
    use HostArch::{Aarch64, X86_64};
    use HostLibc::{Gnu, Musl};

    #[test]
    fn decision_table() {
        let riscv = HostArch::Unknown("riscv64");
        let cases = [
            (GNU_BUNDLE_HOST, HostOs::Linux, X86_64, Gnu, Runnable),
            (GNU_BUNDLE_HOST, HostOs::Linux, X86_64, Musl, WrongLibc),
            (MUSL_BUNDLE_HOST, HostOs::Linux, X86_64, Gnu, Runnable),
            (MUSL_BUNDLE_HOST, HostOs::Linux, X86_64, Musl, Runnable),
            // Architecture outranks libc: an ARM64 musl host is WrongArch.
            (GNU_BUNDLE_HOST, HostOs::Linux, Aarch64, Gnu, WrongArch),
            (GNU_BUNDLE_HOST, HostOs::Linux, Aarch64, Musl, WrongArch),
            (MUSL_BUNDLE_HOST, HostOs::Linux, Aarch64, Musl, WrongArch),
            // Defaulting an unrecognised arch to "runnable" is how soldr#2874
            // read: anything not explicitly excluded got the x86_64 ELF.
            (GNU_BUNDLE_HOST, HostOs::Linux, riscv, Gnu, WrongArch),
            (MUSL_BUNDLE_HOST, HostOs::Linux, riscv, Gnu, WrongArch),
            (
                GNU_BUNDLE_HOST,
                HostOs::MacOs,
                X86_64,
                HostLibc::None,
                WrongOs,
            ),
            (
                MUSL_BUNDLE_HOST,
                HostOs::MacOs,
                Aarch64,
                HostLibc::None,
                WrongOs,
            ),
            (
                GNU_BUNDLE_HOST,
                HostOs::Windows,
                X86_64,
                HostLibc::None,
                WrongOs,
            ),
            (
                MUSL_BUNDLE_HOST,
                HostOs::Windows,
                X86_64,
                HostLibc::None,
                WrongOs,
            ),
        ];
        for (req, os, arch, libc, expected) in cases {
            let got = bundle_host_fitness(req, os, arch, libc);
            assert_eq!(got, expected, "{req:?} {os:?}/{arch:?}/{libc:?}");
            assert_eq!(got.is_runnable(), expected == Runnable);
        }
    }

    #[test]
    fn the_wrong_libc_reason_names_the_host_and_the_glibc_loader() {
        let reason = wrong_libc_reason("x86_64-unknown-linux-musl");
        assert!(reason.contains("x86_64-unknown-linux-musl"), "{reason}");
        assert!(reason.contains("musl"), "{reason}");
        assert!(reason.contains(GLIBC_LOADER), "{reason}");
    }
}
