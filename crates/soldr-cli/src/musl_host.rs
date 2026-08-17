//! Musl-host prerequisite probes (soldr#2614).
//!
//! Split out of `toolchain.rs` rather than grown inside it: that file sits
//! two lines under the 1,000-line production ceiling, and these probes are a
//! self-contained concern — "can a rustup musl-host toolchain actually run
//! and link on this host, and if not, what does the user install?"
//!
//! Measured on stock `alpine:3.20` with rustup 1.29.0 and
//! `1.95.0-x86_64-unknown-linux-musl`:
//!
//! * stock            -> every toolchain binary dies relocating `_Unwind_GetCFA`;
//! * `libgcc`         -> `rustc`/`cargo` run, the first link fails with
//!                       "error: linker `cc` not found";
//! * `+ gcc musl-dev` -> compiles, and the binary runs.
//!
//! That last line is why the channel itself is not the problem: the rustup
//! musl-host channel ships a complete toolchain, so the earlier "rustc is not
//! installed for the toolchain" reading was a downstream symptom of the
//! missing unwinder.

/// soldr#2614: the rustup musl-host toolchain's binaries dynamically link
/// libgcc's unwinder, which stock Alpine does not ship. Without it every
/// toolchain binary dies with the cryptic
/// `Error relocating .../cargo: _Unwind_GetCFA: symbol not found`, long
/// after the actionable moment. Probe the loader paths once per process
/// and name the remedy up front. Warning-only: a musl host that gets its
/// unwinder some other way (custom LD_LIBRARY_PATH, static toolchain)
/// must not be blocked.
pub(crate) fn warn_when_missing_prerequisites() {
    use std::sync::OnceLock;
    static WARNED: OnceLock<()> = OnceLock::new();
    if crate::platform::host::facts::libc() != crate::platform::host::facts::HostLibc::Musl {
        return;
    }
    WARNED.get_or_init(|| {
        let candidates = [
            std::path::Path::new("/usr/lib/libgcc_s.so.1"),
            std::path::Path::new("/lib/libgcc_s.so.1"),
            std::path::Path::new("/usr/local/lib/libgcc_s.so.1"),
        ];
        for line in
            musl_host_prerequisite_warnings(musl_libgcc_present(&candidates), musl_linker_present())
        {
            eprintln!("{line}");
        }
    });
}

/// Pure core of the probe, path-injectable for tests.
fn musl_libgcc_present(candidates: &[&std::path::Path]) -> bool {
    candidates.iter().any(|path| path.is_file())
}

/// Whether a `cc` linker driver is reachable on PATH.
///
/// Separate from the libgcc probe because the two failures are separate and
/// land at different moments: without libgcc the toolchain's own binaries
/// cannot start at all, while with libgcc but no `cc` they start fine and the
/// first link dies instead.
fn musl_linker_present() -> bool {
    crate::platform::host::facts::os() != crate::platform::host::facts::HostOs::Windows
        && std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|dir| dir.join("cc").is_file()))
            .unwrap_or(false)
}

/// Pure message builder: which musl-host prerequisites are missing, and what
/// to install.
///
/// Measured on stock `alpine:3.20` with rustup 1.29.0 and
/// `1.95.0-x86_64-unknown-linux-musl` (soldr#2614):
///
/// * no libgcc  -> every toolchain binary dies relocating `_Unwind_GetCFA`;
/// * libgcc only -> `rustc --version` works and `cargo build` fails with
///   `error: linker 'cc' not found`;
/// * `libgcc gcc musl-dev` -> compiles and the binary runs.
///
/// That third line matters beyond the message: the rustup musl-host channel
/// *does* ship a complete toolchain, so the earlier "rustc is not installed
/// for the toolchain" reading was a downstream symptom of the missing
/// unwinder, not a gap in the channel.
///
/// Warning-only, both of them: a musl host that gets its unwinder or its
/// linker some other way (custom `LD_LIBRARY_PATH`, `clang` as the linker
/// driver, a static toolchain) must not be blocked by a probe.
fn musl_host_prerequisite_warnings(libgcc: bool, linker: bool) -> Vec<String> {
    let mut warnings = Vec::new();
    if !libgcc {
        warnings.push(
            concat!(
                "soldr: warning: this musl host has no libgcc_s.so.1 in the loader paths. ",
                "The rustup musl-host Rust toolchain dynamically links libgcc's unwinder, ",
                "so its cargo/rustc will fail with ",
                "'Error relocating ...: _Unwind_GetCFA: symbol not found'. ",
                "On Alpine, install it first: apk add libgcc (soldr#2614).",
            )
            .to_string(),
        );
    }
    if !linker {
        warnings.push(
            concat!(
                "soldr: warning: this musl host has no `cc` on PATH. The toolchain will ",
                "start, and then the first link will fail with ",
                "\"error: linker `cc` not found\". ",
                "On Alpine, install it first: apk add gcc musl-dev (soldr#2614).",
            )
            .to_string(),
        );
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    /// soldr#2614: the musl libgcc probe's path-injectable core.
    #[test]
    fn musl_libgcc_probe_finds_any_candidate_and_reports_absence() {
        let dir = std::env::temp_dir().join(format!("libgcc-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("probe dir");
        let missing_a = dir.join("usr-lib-libgcc_s.so.1");
        let missing_b = dir.join("lib-libgcc_s.so.1");
        assert!(!musl_libgcc_present(&[
            missing_a.as_path(),
            missing_b.as_path()
        ]));
        std::fs::write(&missing_b, b"elf").expect("write candidate");
        assert!(musl_libgcc_present(&[
            missing_a.as_path(),
            missing_b.as_path()
        ]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// soldr#2614: both musl-host prerequisites, and their remedies.
    ///
    /// Measured on stock `alpine:3.20` with rustup 1.29.0 and
    /// `1.95.0-x86_64-unknown-linux-musl`: libgcc alone gets `rustc --version`
    /// working but the first link still dies with ``error: linker `cc` not
    /// found``, so a message naming only libgcc sends someone back for a second
    /// round trip.
    #[test]
    fn musl_host_warnings_name_each_missing_prerequisite() {
        assert!(musl_host_prerequisite_warnings(true, true).is_empty());

        let no_libgcc = musl_host_prerequisite_warnings(false, true);
        assert_eq!(no_libgcc.len(), 1);
        assert!(no_libgcc[0].contains("apk add libgcc"));
        assert!(no_libgcc[0].contains("_Unwind_GetCFA"));

        let no_linker = musl_host_prerequisite_warnings(true, false);
        assert_eq!(no_linker.len(), 1);
        assert!(no_linker[0].contains("apk add gcc musl-dev"));
        assert!(no_linker[0].contains("linker `cc` not found"));

        // Stock Alpine: both missing, both named, so one read fixes the host.
        assert_eq!(musl_host_prerequisite_warnings(false, false).len(), 2);
    }

    /// The warnings are user-facing prose, and a `\`-continued Rust string
    /// literal is flattened by rustfmt into runs of indentation spaces. The
    /// shipped libgcc message had exactly that: "in the loader" followed by
    /// eighteen spaces before "paths.".
    #[test]
    fn musl_host_warnings_have_no_ragged_whitespace() {
        for warning in musl_host_prerequisite_warnings(false, false) {
            assert!(
                !warning.contains("  "),
                "warning carries collapsed-continuation whitespace: {warning:?}"
            );
        }
    }
}
