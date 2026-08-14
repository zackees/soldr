//! Unit tests for `-C link-self-contained` handling.
//!
//! Split out of `target_lifecycle.rs` (soldr#2493). Converting the retired
//! watchdog-macro call sites to plain `#[test] fn` costs one line per test,
//! which took that
//! file from 992 to 1008 lines -- past the 1000-line production ceiling
//! soldr#2493 itself introduced. The two test modules are the natural seam,
//! and the layout matches the sibling `cargo_front_door/tests.rs`.

use super::*;

use super::supports_link_self_contained;

// A native aarch64 release build failed with
//   error: option `-C link-self-contained` is not supported on this target
// because managed-zig prep injected the flag unconditionally. Only the
// release workflow builds aarch64 natively -- every other lane drives it
// from an x86_64 host -- so nothing else exercised this path.
// The release failure: rustc rejects the flag outright here.
#[test]
fn aarch64_gnu_does_not_get_link_self_contained() {
    assert!(!supports_link_self_contained("aarch64-unknown-linux-gnu"));
}

// The managed musl CRT owns startup objects. Re-injecting the Zig-only
// self-contained override can produce a duplicate `_start` at link time.
#[test]
fn managed_musl_never_gets_the_zig_startup_override() {
    assert!(!supports_link_self_contained("aarch64-unknown-linux-musl"));
    assert!(!supports_link_self_contained("x86_64-unknown-linux-musl"));
}

#[test]
fn x86_64_gnu_still_gets_it() {
    assert!(supports_link_self_contained("x86_64-unknown-linux-gnu"));
}

// Unknown targets must default to *not* passing the flag: passing it
// where unsupported is a hard error, while omitting it merely restores
// the pre-managed-zig linking behaviour.
#[test]
fn an_unknown_target_defaults_to_omitting_the_flag() {
    assert!(!supports_link_self_contained("riscv64gc-unknown-linux-gnu"));
    assert!(!supports_link_self_contained("powerpc64le-unknown-linux"));
}
