//! `running-process-probe-agent` — sidecar binary for #551.
//!
//! Slice 1 scaffold: prints a version banner and exits cleanly. Proves
//! the workspace plumbing — this binary needs to compile on Windows
//! (where it will eventually host the DLL-injection vehicle), Linux
//! (LD_PRELOAD shim launcher), and macOS (DYLD_INSERT_LIBRARIES shim
//! launcher).
//!
//! Slice 2 of #551 makes this binary the embed-and-extract target:
//! `include_bytes!`-wrapped into the library, extracted to a per-user
//! cache directory at first use, signed by the consumer's cert, then
//! spawned as a child of the target process.
//!
//! Slices 4–6 fill in the per-OS injector payload. Until then this is
//! deliberately inert — a one-line banner — so the static-AV-surface
//! contract (no injection symbols in the embedded blob) is trivially
//! upheld.

fn main() {
    println!(
        "running-process-probe-agent {} — slice 1 of #551 (inert scaffold)",
        env!("CARGO_PKG_VERSION")
    );
}
