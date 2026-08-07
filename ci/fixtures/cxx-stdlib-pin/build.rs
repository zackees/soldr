//! soldr#2309 fixture: exercise the C++ ingestion paths a real `-sys`
//! crate uses (clud's vendored whisper-rs-sys shape) so the blessed
//! linux-gnu env must keep cc-rs, CMake, the build.rs stdlib picker,
//! and the final GNU-driver link on ONE C++ stdlib.
//!
//! * cc-rs half: compiles `src/ccrs.cpp`; cc-rs resolves the compiler
//!   from `CXX_<triple>` and the stdlib from `CXXSTDLIB` (both pinned
//!   by soldr for linux-gnu targets).
//! * CMake half: configures/builds `cmakelib/` through the `cmake`
//!   crate — where a clang-family compiler detection would otherwise
//!   pull in `-lc++`.
//! * vendored-picker half: replicates clud's vendored whisper-rs-sys
//!   `get_cpp_link_stdlib` (clud#858) VERBATIM in the load-bearing
//!   parts. Note it does NOT read `CXXSTDLIB` at all: it sniffs zig
//!   signals (`CARGO_ZIGBUILD_RUSTC_VERSION`, `ZIG_COMMAND`, a
//!   `CXX`/`CXX_<triple>` value mentioning zig) and emits
//!   `cargo:rustc-link-lib=dylib=c++` when any fires. On the blessed
//!   path the sniff must come up empty — soldr never invokes zig for
//!   linux-gnu and pins `CXX_<triple>` to the catalogue g++ — so the
//!   picker falls through to stdc++ and the link agrees with the
//!   GNU driver. With a zig signal present the picker emits -lc++ and
//!   the catalogue link MUST fail (the clud#858 mechanism), which
//!   ci/cxx_stdlib_pin_acceptance.sh asserts as its RED half.

use std::env;
use std::ffi::OsStr;

fn main() {
    let target = env::var("TARGET").unwrap();
    println!("cargo:rerun-if-env-changed=CARGO_ZIGBUILD_RUSTC_VERSION");
    println!("cargo:rerun-if-env-changed=ZIG_COMMAND");
    println!("cargo:rerun-if-env-changed=CXX");
    println!(
        "cargo:rerun-if-env-changed={}",
        target_env_key("CXX", &target)
    );

    cc::Build::new()
        .cpp(true)
        .file("src/ccrs.cpp")
        .compile("ccrs_pin");

    let dst = cmake::Config::new("cmakelib").build();
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=pinlib");

    // The vendored whisper-rs-sys stdlib emission (clud#858): decided by
    // target sniffing alone — CXXSTDLIB is never consulted.
    if let Some(cpp_stdlib) = get_cpp_link_stdlib(&target) {
        println!("cargo:rustc-link-lib=dylib={cpp_stdlib}");
    }
}

// Mirrors clud vendor/whisper-rs-sys/build.rs `get_cpp_link_stdlib`.
fn get_cpp_link_stdlib(target: &str) -> Option<&'static str> {
    if target.contains("msvc") {
        None
    } else if target.contains("apple") || target.contains("freebsd") || target.contains("openbsd") {
        Some("c++")
    } else if target.contains("android") {
        Some("c++_shared")
    } else if linux_zig_cxx_toolchain(target) {
        Some("c++")
    } else {
        Some("stdc++")
    }
}

fn linux_zig_cxx_toolchain(target: &str) -> bool {
    if !target.contains("linux") {
        return false;
    }
    env::var_os("CARGO_ZIGBUILD_RUSTC_VERSION").is_some()
        || env::var_os("ZIG_COMMAND").is_some()
        || env_value_mentions_zigcxx("CXX")
        || env_value_mentions_zigcxx(&target_env_key("CXX", target))
}

fn target_env_key(prefix: &str, target: &str) -> String {
    format!("{}_{}", prefix, target.replace('-', "_"))
}

fn env_value_mentions_zigcxx(key: &str) -> bool {
    match env::var_os(key) {
        Some(value) => os_str_mentions_zigcxx(&value),
        None => false,
    }
}

fn os_str_mentions_zigcxx(value: &OsStr) -> bool {
    let value = value.to_string_lossy().to_ascii_lowercase();
    value.contains("zigcxx") || value.contains("zig c++")
}
