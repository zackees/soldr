//! Build script for `soldr-cli`.
//!
//! Compresses `embed/manifest.json` into `${OUT_DIR}/manifest.json.zst`
//! at compile time so `src/fetch/manifest_v6.rs` can `include_bytes!`
//! the result without checking a binary blob into source control.
//!
//! The JSON itself is the source of truth — it is checked in at
//! `crates/soldr-cli/embed/manifest.json`. For THIS phase of issue #861
//! the JSON is an empty schema-v6 envelope (`{"schema_version":6,
//! "tools":{}}`). A follow-up sub-issue under #853 will wire the
//! materialization pipeline that pulls the latest snapshot off the
//! `manifest` branch at release time.
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

    // zstd level 19 matches the level used elsewhere in the crate
    // (cache_lib::cook_archive::COOK_ZSTD_LEVEL, archive_cmd::ARCHIVE_ZSTD_LEVEL).
    // Decompression speed is what matters at runtime — the compression
    // ratio at 19 vs. 22 is negligible for sub-MB JSON.
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 19).expect("zstd encoder init");
    encoder.write_all(&json_bytes).expect("zstd compress write");
    let compressed = encoder.finish().expect("zstd compress finish");

    std::fs::write(&dst, &compressed).unwrap_or_else(|e| panic!("write {}: {e}", dst.display()));

    println!(
        "cargo:warning=soldr-cli: embedded manifest.json.zst built ({} bytes -> {} bytes)",
        json_bytes.len(),
        compressed.len()
    );
}
