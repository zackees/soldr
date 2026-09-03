# npm Publishing

This repository publishes `soldr` to npm as a thin JavaScript wrapper around the
official GitHub Release binaries. The npm package does not build Rust from
source during install.

## Package Shape

- package name: `@zackees/soldr`
- executable: `soldr`
- install step: downloads the matching GitHub Release archive for the current
  OS/architecture
- verification: checks the downloaded archive against
  `soldr-vX.Y.Z-SHA256SUMS.txt` before installing the binary
- binary sharing: npm installs the same GitHub Release binary that the release
  workflow attests, and PyPI wheels are built from that same per-platform target
  binary before publication

Supported npm install targets match the release workflow:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`

Intel macOS archives and wheels are cross-built on Linux through the blessed
Apple SDK path, then architecture-checked and smoke-tested inside a
dockur/macos x86_64 guest hosted on an `ubuntu-24.04` runner (soldr#3071 --
no GitHub Actions job may run on a `macos-*` runner) before GitHub Release,
PyPI, or npm publication. Apple Silicon (`aarch64-apple-darwin`) artifacts
are cross-built the same way but are not smoke-tested anywhere in CI until
soldr#3071 re-enables execution before release.

## Owner Setup

Before automated npm publication can work, configure npm Trusted Publishing for
the package. The package has already been created manually, so it can now be
connected to GitHub Actions OIDC.

1. Open the npm package settings for `@zackees/soldr`.
2. In the package publishing settings, add this GitHub trusted publisher:
   - repository owner: `zackees`
   - repository name: `soldr`
   - workflow filename: `release-auto.yml`
   - environment: `release`
3. Keep the repository URL in `package.json` pointed at
   `git+https://github.com/zackees/soldr.git`; npm checks that for trusted
   publishing.
4. Optionally set the package to require 2FA and disallow tokens after the
   trusted publisher is configured.

The workflow uses npm Trusted Publishing directly:

- `.github/workflows/release-auto.yml`
- job: `publish-npm`
- environment: `release`
- permissions: `id-token: write`, `contents: read`
- Node: `24`
- npm CLI: `11.12.1`
- idempotency: the job skips `npm publish` when an npm dist-tag already points
  at the exact package version

Do not add `NODE_AUTH_TOKEN` for this job. npm exchanges the GitHub OIDC token
for publish credentials when the trusted publisher configuration matches.

## Release Order

The npm package version must match `Cargo.toml`. The release workflow publishes
npm only after the GitHub Release job succeeds, because the npm postinstall
script downloads artifacts from that release.

For manual validation before publishing:

```bash
node scripts/test-npm-package.js
npm pack --dry-run
```

To test the install script without downloading a release artifact:

```bash
SOLDR_NPM_SKIP_DOWNLOAD=1 npm install
```

Do not publish an npm version until the matching GitHub Release has these files:

- `soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.zst`
- `soldr-vX.Y.Z-aarch64-unknown-linux-gnu.tar.zst`
- `soldr-vX.Y.Z-x86_64-unknown-linux-musl.tar.zst`
- `soldr-vX.Y.Z-aarch64-unknown-linux-musl.tar.zst`
- `soldr-vX.Y.Z-x86_64-apple-darwin.tar.zst`
- `soldr-vX.Y.Z-aarch64-apple-darwin.tar.zst`
- `soldr-vX.Y.Z-x86_64-pc-windows-msvc.tar.zst`
- `soldr-vX.Y.Z-aarch64-pc-windows-msvc.tar.zst`
- `soldr-vX.Y.Z-SHA256SUMS.txt`

Each archive is a `.tar.zst` at zstd compression level 19 and bundles
soldr, soldr-daemon, crgx, cargo-chef, and a `manifest.json` at
the archive root. Windows archives also include soldr's matching PDB
sidecar:

- `soldr` (or `soldr.exe`) — the soldr CLI itself.
- `soldr-daemon` (or `soldr-daemon.exe`) — soldr-owned daemon for the
  embedded cache service.
- Toolchain, clang, and `zccache-soldr` shim names are hardlinks/copies
  of `soldr` created at install time; they are not archive entries.
- `soldr.pdb` or `soldr_cli.pdb` - Windows-only soldr debug symbols
  recorded under `soldr.debug_info` in `manifest.json`.
- `crgx` (or `crgx.exe`) - the matching-target crgx binary.
- `cargo-chef` (or `cargo-chef.exe`) - the pinned cargo-chef binary used
  by `soldr cook`.
- `manifest.json` - schema_version 3 descriptor with soldr / embedded
  zccache / crgx / cargo-chef versions, target triples, soldr
  debug-info sidecars, per-file sha256s, and archive format.
  The expected archive layout is versioned in
  `contracts/zccache-runtime.v1.json`.

The npm install wrapper validates `manifest.json`, unpacks the release binaries
plus any soldr debug-info sidecars into `bin/native/`, and `bin/soldr.js`
exports `SOLDR_CRGX_LOCAL_DIR` and `SOLDR_CARGO_CHEF_LOCAL_DIR` to that dir
before spawning soldr, so the bundled tools are picked up automatically.
