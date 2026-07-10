//! Thin binary shim (#1490 Phase 1). The whole CLI — mode detection,
//! clap dispatch, multicall — lives in the library crate
//! (`src/soldr_main.rs`), so the lib and bin no longer compile the
//! same ~40K LOC module tree twice per build.

fn main() {
    soldr_cli::run()
}
