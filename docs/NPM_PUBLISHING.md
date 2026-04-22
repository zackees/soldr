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
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`

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

- `soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`
- `soldr-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz`
- `soldr-vX.Y.Z-x86_64-apple-darwin.tar.gz`
- `soldr-vX.Y.Z-aarch64-apple-darwin.tar.gz`
- `soldr-vX.Y.Z-x86_64-pc-windows-msvc.zip`
- `soldr-vX.Y.Z-aarch64-pc-windows-msvc.zip`
- `soldr-vX.Y.Z-SHA256SUMS.txt`
