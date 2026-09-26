//! Regression guard: `xz2` must always statically link liblzma.
//!
//! Without the `static` feature, `lzma-sys`'s build.rs dynamically links
//! whatever liblzma pkg-config finds on the build host. On a host with a
//! system liblzma, `pip install .` produces a binary with a `liblzma`
//! `NEEDED` entry; maturin's auditwheel repair then vendors a copy into
//! `soldr.libs/` and rewrites the RPATH to `$ORIGIN/../soldr.libs`. soldr
//! later materializes copies of its own binary as relocated shim images
//! under `~/.soldr*/shims/images/<hash>/...`, where that relative RPATH
//! does not resolve, so every wrapped compile fails with "error while
//! loading shared libraries: liblzma...". Released PyPI artifacts never
//! exhibited this because their build hosts had no system liblzma for
//! pkg-config to find — a difference of build-host luck, not of code.
//! Statically linking liblzma removes the host dependency entirely.

use crate::common;

#[test]
fn xz2_statically_links_lzma() {
    let root = common::workspace_root();
    let manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).expect("read workspace Cargo.toml");

    let mut in_section = false;
    let mut xz2_line: Option<String> = None;
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_section = line == "[workspace.dependencies]";
            continue;
        }
        if in_section {
            if let Some(rest) = line.strip_prefix("xz2") {
                let rest = rest.trim_start();
                if rest.starts_with('=') {
                    xz2_line = Some(line.to_string());
                    break;
                }
            }
        }
    }

    let xz2_line = xz2_line
        .expect("[workspace.dependencies] must declare xz2 (see fetch::archive .tar.xz support)");

    assert!(
        xz2_line.contains("\"static\""),
        "xz2 in [workspace.dependencies] is `{xz2_line}` and does not enable the \"static\" \
         feature. Without it, lzma-sys dynamically links whatever liblzma pkg-config finds on \
         the build host; auditwheel then vendors that into soldr.libs/ with an \
         $ORIGIN/../soldr.libs RPATH that soldr's relocated shim images cannot resolve, and \
         every wrapped compile fails with \"error while loading shared libraries: liblzma...\". \
         Declare it as `xz2 = {{ version = \"0.1\", features = [\"static\"] }}`."
    );
}
