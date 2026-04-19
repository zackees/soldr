# Release Verification

This document describes how to verify a published `soldr` release today.

It is intentionally limited to what the repository currently implements.

## Verification Policy

The current `soldr` verification policy is:

- the official user-facing verification path is checksum verification plus `gh attestation verify`
- GitHub CLI is the primary documented tool for attestation verification
- Sigstore-compatible offline verification remains possible through downloaded attestation bundles, but it is not the primary documented path
- `soldr` does not currently require or publish SBOMs for the release line
- `soldr` does not currently claim independently reproducible builds
- `soldr` does not currently publish extra signed metadata beyond the checksum manifest and GitHub provenance attestations

Those positions are deliberate. They may be revisited later, but they are the current release policy rather than open questions.

## What The Current Release Flow Guarantees

For a normal `soldr` release:

- a reviewed version bump is merged to protected `main`
- `.github/workflows/release-auto.yml` derives the tag directly from `Cargo.toml` at that merged commit
- the workflow re-runs lint, workspace build, tests, integration, and all supported e2e bootstrap jobs for that exact commit
- release archives are built from that exact commit
- final publication happens in the `release` environment
- the version tag is protected by repository rulesets and created through the release workflow path
- the published GitHub Release is immutable once published
- a SHA-256 checksum manifest is published with the release assets
- GitHub build provenance attestations are generated for the published assets

## What It Does Not Guarantee Yet

The current release flow does not yet claim all of the following:

- SBOM publication
- independently reproduced builds by a second builder
- fully hermetic inputs for rustup, crates.io, OS packages, or third-party test inputs

The release-governance and hermetic-input follow-up items remain tracked in issues [#11](https://github.com/zackees/soldr/issues/11), [#41](https://github.com/zackees/soldr/issues/41), and [#42](https://github.com/zackees/soldr/issues/42). SBOM publication and independently reproduced builds are intentionally outside the current release claim.

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

Use GitHub CLI's attestation support to verify the artifact provenance.

This is the primary documented verification path for `soldr`:

```bash
gh attestation verify soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz \
  --repo zackees/soldr
```

For stricter identity validation, also pin the signer workflow:

```bash
gh attestation verify soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz \
  --repo zackees/soldr \
  --signer-workflow zackees/soldr/.github/workflows/release-auto.yml
```

This validates that GitHub has a matching attestation for the artifact and that the attestation was produced by the expected repository and workflow.

## Step 3: Understand What Was Verified

`gh attestation verify` validates:

- the artifact digest
- the GitHub repository identity
- the workflow identity if you provide `--signer-workflow`
- the provenance attestation type

It does not, by itself, prove that every external input used during the build was mirrored or hermetic. For the current trust boundary inventory, see [TRUST_BOUNDARIES.md](./TRUST_BOUNDARIES.md).

## Optional: Offline Verification And Sigstore-Compatible Bundles

GitHub CLI also supports downloading attestation bundles and verifying them offline.

Relevant commands:

```bash
gh attestation download soldr-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz --repo zackees/soldr
gh attestation trusted-root
```

Offline verification is not the primary documented path for `soldr`, but it remains available if you want to archive bundles and trusted roots alongside release artifacts.

This is also the nearest current equivalent to a Sigstore-style workflow for this repository. We do not require users to install separate Sigstore tooling as part of the normal `soldr` verification story.

## About `gh release verify`

GitHub CLI also has `gh release verify`, which verifies release-level attestations.

We do not currently document that as the primary verification path for `soldr` because immutable releases and the surrounding release-governance settings are still tracked separately. Today, the repository's official verification path is:

1. checksum verification
2. artifact attestation verification with `gh attestation verify`
