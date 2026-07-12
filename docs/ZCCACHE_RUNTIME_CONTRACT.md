# Zccache Runtime Contract

`contracts/zccache-runtime.v1.json` is the shared contract for the zccache
runtime soldr ships and wires through setup-soldr and npm.

The contract fixes these cross-runtime expectations:

- Release archives are `.tar.zst` bundles containing `soldr`,
  `soldr-daemon`, `crgx`, `cargo-chef`, and `manifest.json`.
  `soldr-daemon` is a multicall alias of `soldr`. Toolchain,
  clang, and `zccache-soldr` shim names are materialized from `soldr` at
  install time, not shipped as sidecar binaries. Windows archives also carry
  soldr's matching PDB sidecar (`soldr.pdb` or `soldr_cli.pdb`) so crash
  dumps can resolve file/line frames.
- `manifest.json` is the authoritative per-archive descriptor for schema
  version, archive format, soldr target, embedded zccache status, bundled
  binary names, soldr debug-info sidecars, and per-file `sha256` values.
- zccache is embedded into soldr/soldr-daemon; release archives do not bundle
  `zccache`, `zccache-daemon`, or `zccache-fp` binaries.
- Setup-soldr and npm must use the same local crgx env var:
  `SOLDR_CRGX_LOCAL_DIR`.
- Setup-soldr and npm must use the same local cargo-chef env var:
  `SOLDR_CARGO_CHEF_LOCAL_DIR`.
- Rust runtime constants, action helpers, npm scripts, public-action exporter
  fixtures, release workflow manifest generation, and docs are covered by tests
  against the same contract file.

Linux soldr archives are still split by glibc/musl target. The zccache block in
`manifest.json` records the soldr target and `"embedded": true`; there is no
separate Linux zccache target mapping.
