# Release Verification

This document describes how to verify a published `soldr` release today.

It is intentionally limited to what the repository currently implements.

## What The Current Release Flow Guarantees

For a normal `soldr` release:

- the release workflow is started manually with an explicit version and exact commit SHA
- that exact commit must be reachable from the protected `release` branch
- the workflow re-runs lint, workspace build, tests, integration, and all supported e2e bootstrap jobs for that exact commit
- release archives are built from that exact commit
- the version tag is protected by repository rulesets and created through the release workflow path
- the published GitHub Release is immutable once published
- a SHA-256 checksum manifest is published with the release assets
- GitHub build provenance attestations are generated for the published assets

## What It Does Not Guarantee Yet

The current release flow does not yet claim all of the following:

- SBOM publication
- independently reproduced builds by a second builder
- fully hermetic inputs for rustup, crates.io, OS packages, or third-party test inputs

Those follow-up items are tracked in issues [#12](https://github.com/zackees/soldr/issues/12) and [#13](https://github.com/zackees/soldr/issues/13).

## Release Asset Names

Current release assets follow this shape:

- `soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`
- `soldr-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz`
- `soldr-vX.Y.Z-x86_64-apple-darwin.tar.gz`
- `soldr-vX.Y.Z-aarch64-apple-darwin.tar.gz`
- `soldr-vX.Y.Z-x86_64-pc-windows-msvc.zip`
- `soldr-vX.Y.Z-aarch64-pc-windows-msvc.zip`
- `soldr-vX.Y.Z-SHA256SUMS.txt`

## Step 1: Verify The Checksum

Download the release artifact you want and the matching `SHA256SUMS` file.

On Linux or macOS:

```bash
sha256sum -c soldr-vX.Y.Z-SHA256SUMS.txt --ignore-missing
```

On Windows PowerShell:

```powershell
$expected = Select-String -Path soldr-vX.Y.Z-SHA256SUMS.txt -Pattern 'soldr-vX.Y.Z-x86_64-pc-windows-msvc.zip' |
  ForEach-Object { ($_ -split '\s+')[0] }
$actual = (Get-FileHash .\soldr-vX.Y.Z-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash.ToLower()
if ($expected -ne $actual) { throw "checksum mismatch" }
```

The checksum step tells you the file you downloaded matches the checksum manifest attached to the release.

## Step 2: Verify The Artifact Attestation

Use GitHub CLI's attestation support to verify the artifact provenance:

```bash
gh attestation verify soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz \
  --repo zackees/soldr
```

For stricter identity validation, also pin the signer workflow:

```bash
gh attestation verify soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz \
  --repo zackees/soldr \
  --signer-workflow zackees/soldr/.github/workflows/release.yml
```

This validates that GitHub has a matching attestation for the artifact and that the attestation was produced by the expected repository and workflow.

## Step 3: Understand What Was Verified

`gh attestation verify` validates:

- the artifact digest
- the GitHub repository identity
- the workflow identity if you provide `--signer-workflow`
- the provenance attestation type

It does not, by itself, prove that every external input used during the build was mirrored or hermetic. For the current trust boundary inventory, see [TRUST_BOUNDARIES.md](./TRUST_BOUNDARIES.md).

## Optional: Offline Verification

GitHub CLI also supports downloading attestation bundles and verifying them offline.

Relevant commands:

```bash
gh attestation download soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz --repo zackees/soldr
gh attestation trusted-root
```

Offline verification is not yet the primary documented path for `soldr`, but it remains available if you want to archive bundles and trusted roots alongside release artifacts.

## About `gh release verify`

GitHub CLI also has `gh release verify`, which verifies release-level attestations.

Because `soldr` now uses protected release tags and immutable GitHub Releases, `gh release verify` is a reasonable supplementary check.

We still document per-artifact verification as the primary path because it validates the exact archive you downloaded. Today, the strongest explicit verification path for `soldr` is:

1. checksum verification
2. artifact attestation verification with `gh attestation verify`
