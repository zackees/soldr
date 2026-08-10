//! Compiles the `running_process.probe_diag.v1` schema with protox +
//! prost-build, mirroring `crates/running-process/build.rs`.
//!
//! Every `.proto` must appear in BOTH lists below. A file listed only in
//! `compile` still builds, but edits to it won't retrigger the build; a file
//! listed only in `rerun-if-changed` isn't compiled at all.

const PROTOS: &[&str] = &[
    "proto/probe_diag_v1/probe_diag_v1_common.proto",
    "proto/probe_diag_v1/probe_diag_v1_registration.proto",
    "proto/probe_diag_v1/probe_diag_v1_capture.proto",
    "proto/probe_diag_v1/probe_diag_v1_profile.proto",
    "proto/probe_diag_v1/probe_diag_v1_symbols.proto",
    "proto/probe_diag_v1/probe_diag_v1_query.proto",
    "proto/probe_diag_v1/probe_diag_v1_envelope.proto",
    "proto/probe_diag_v1/probe_diag_v1_crash.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for proto in PROTOS {
        println!("cargo:rerun-if-changed={proto}");
    }
    println!("cargo:rerun-if-changed=build.rs");

    let fds = protox::compile(PROTOS, ["proto/"])?;
    prost_build::compile_fds(fds)?;
    Ok(())
}
