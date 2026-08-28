//! The CLI entry point must not collect argv with `std::env::args()`
//! (soldr#2658 item 2).
//!
//! `std::env::args` **panics** mid-iteration on an argument that is not valid
//! Unicode. On Unix that is reachable with ordinary input, because paths are
//! bytes: a `--manifest-path` naming a non-UTF-8 file was enough. Measured on
//! the Docker Linux runner against the pre-fix binary:
//!
//! ```text
//! $ soldr $'--\xff\xfe-not-utf8'
//! thread 'soldr-cli' panicked at library/std/src/env.rs:864:51:
//! called `Result::unwrap()` on an `Err` value: "--\xFF\xFE-not-utf8"
//! note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
//! ```
//!
//! A raw panic with a backtrace note, from a std path, never naming soldr.
//! After the fix the same invocation exits 1 with a two-line diagnostic that
//! names the argument's position and shows its bytes.
//!
//! Why a source guard rather than a behavioral test: constructing a non-UTF-8
//! `OsString` requires `std::os::unix::ffi::OsStringExt` (or an unpaired
//! surrogate via `std::os::windows::ffi`), and `platform_cfg_boundary_ratchet`
//! forbids both host `#[cfg]` and native `std::os::*` markers outside
//! `crates/soldr-platform`'s three concrete trees — with no allowlist. The
//! namespaces in that crate are fixed at five by soldr#2493, and "make me an
//! invalid `OsString`" is not one of them, so the invariant is worth more than
//! the convenience. This guard is fully portable and catches the actual
//! regression: someone reaching for `env::args()` again without knowing why it
//! was replaced.
//!
//! Scoped to the entry point deliberately. Other `env::args()` call sites run
//! after dispatch, on paths whose arguments soldr has already seen; this is the
//! one that must survive whatever the OS hands it.

use crate::common;

/// The one file that must never reintroduce the panicking form.
const ENTRY_POINT: &str = "crates/soldr-cli/src/soldr_main.rs";

#[test]
fn the_cli_entry_point_does_not_use_env_args() {
    // `common::workspace_root()` resolves at *runtime*. `CARGO_MANIFEST_DIR`
    // would bake this machine's path into the binary, which breaks the
    // nextest-archive replay on the target-run lanes (they remap the workspace
    // on a different host). `test_archived_source_tests_use_only_runtime_
    // workspace_resolution` enforces that, and caught this file.
    let path = common::workspace_root().join(ENTRY_POINT);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    // Comments quote `std::env::args` while explaining the fix, so only
    // non-comment lines count. A crude strip is enough: this file has no
    // block comments containing the token.
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !code.contains("env::args()"),
        "{ENTRY_POINT} must collect argv with `env::args_os()` and reject \
         non-UTF-8 explicitly. `std::env::args()` panics mid-iteration on a \
         non-Unicode argument, which on Unix is reachable with an ordinary \
         non-UTF-8 path (soldr#2658)."
    );
    assert!(
        code.contains("env::args_os()"),
        "{ENTRY_POINT} should collect argv via `env::args_os()`; if the entry \
         point moved, move this guard with it rather than deleting it."
    );
}
