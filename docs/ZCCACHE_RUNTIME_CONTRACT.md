# Zccache Runtime Contract

`contracts/zccache-runtime.v1.json` is the shared contract for the zccache
runtime soldr ships and wires through setup-soldr and npm.

The contract fixes these cross-runtime expectations:

- Release archives are `.tar.zst` bundles containing `soldr`, `zccache`,
  `zccache-daemon`, `zccache-fp`, `crgx`, `cargo-chef`, and `manifest.json`.
- `manifest.json` is the authoritative per-archive descriptor for schema
  version, archive format, soldr target, zccache target, bundled binary names,
  and per-binary `sha256` values.
- Setup-soldr and npm install must stage the same zccache trio and export
  `SOLDR_ZCCACHE_LOCAL_DIR` only when that trio is present.
- Setup-soldr and npm must use the same local crgx env var:
  `SOLDR_CRGX_LOCAL_DIR`.
- Setup-soldr and npm must use the same local cargo-chef env var:
  `SOLDR_CARGO_CHEF_LOCAL_DIR`.
- Rust runtime constants, action helpers, npm scripts, public-action exporter
  fixtures, release workflow manifest generation, and docs are covered by tests
  against the same contract file.

Linux soldr archives are still split by glibc/musl target. The bundled zccache
target recorded in `manifest.json` is musl for Linux because upstream zccache
ships the static musl variant used by both Linux archive families.
