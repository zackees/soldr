//! Build script for `soldr-cli`.
//!
//! Compresses the checked-in `embed/manifest.json` into
//! `${OUT_DIR}/manifest.json.zst` for `src/fetch/manifest_v6.rs` to
//! `include_bytes!`.
//!
//! Live asset resolution now uses the soldr-toolchain catalogue at
//! runtime. The build script deliberately avoids network refreshes so
//! local and CI builds do not depend on the retired soldr manifest
//! branch.
//!
//! Re-run conditions:
//! - the JSON source changes
//! - this build script itself changes

use std::io::Write;
use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));

    let src = manifest_dir.join("embed").join("manifest.json");
    let dst = out_dir.join("manifest.json.zst");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", src.display());

    let json_bytes = std::fs::read(&src).unwrap_or_else(|e| panic!("read {}: {e}", src.display()));

    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 19).expect("zstd encoder init");
    encoder.write_all(&json_bytes).expect("zstd compress write");
    let compressed = encoder.finish().expect("zstd compress finish");

    std::fs::write(&dst, &compressed).unwrap_or_else(|e| panic!("write {}: {e}", dst.display()));
}
