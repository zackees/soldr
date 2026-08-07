//! soldr#2309 fixture: exercise BOTH C++ ingestion paths a real `-sys`
//! crate uses (whisper-rs-sys pattern) so the blessed linux-gnu env must
//! keep cc-rs, CMake, and the final GNU-driver link on one C++ stdlib.
//!
//! * cc-rs half: compiles `src/ccrs.cpp` and emits the stdlib link line
//!   from its `CXXSTDLIB` resolution (the knob soldr now pins).
//! * CMake half: configures/builds `cmakelib/` through the `cmake` crate,
//!   which is where a clang-family compiler detection can otherwise pull
//!   in `-lc++`.

fn main() {
    cc::Build::new()
        .cpp(true)
        .file("src/ccrs.cpp")
        .compile("ccrs_pin");

    let dst = cmake::Config::new("cmakelib").build();
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=pinlib");
}
