//! Compiles the vendored pprof schema (#644).
//!
//! Mirrors `crates/running-process/build.rs`: protox parses without needing a
//! `protoc` on PATH, and prost-build generates the Rust types. The schema is
//! vendored rather than pulled from the `pprof` crate, which carries an open
//! RUSTSEC unsoundness advisory — all that is wanted from it is a wire format.

const PROTOS: &[&str] = &["proto/pprof/profile.proto"];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for proto in PROTOS {
        println!("cargo:rerun-if-changed={proto}");
    }
    println!("cargo:rerun-if-changed=build.rs");

    let fds = protox::compile(PROTOS, ["proto/"])?;
    prost_build::compile_fds(fds)?;
    Ok(())
}
