# Trust Boundaries

This document describes the current external trust boundaries for `soldr`.

It records both the factual inventory of what the project trusts today and the current repo policy for which of those trust edges are acceptable on the `0.5.x` line.

## Controlled Inputs

The repository directly controls:

- the source tree at the released commit SHA
- the Cargo dependency graph recorded in `Cargo.lock`
- the workflow definitions committed in `.github/workflows/`
- the action revisions pinned by full commit SHA in those workflows
- the exact third-party Git commit used by the current bootstrap e2e workflow

## Release-Time External Dependencies

Even with a validated release workflow, the release path still depends on external systems:

- GitHub-hosted runners
  - The repo trusts the runner images provided by GitHub Actions.
- Rust toolchains from `rustup`
  - The workflows install Rust toolchains during CI and release.
- crates.io
  - The build downloads crate metadata and crate archives from the crates.io ecosystem.
  - Published crate versions are immutable, but the transport and index are still external.
- GitHub APIs and GitHub Releases
  - The release workflow uses GitHub services to create releases, publish assets, and store attestations.
- PyPI
  - Optional hardened wheel publication relies on PyPI's Trusted Publishing and package hosting.
  - The project already has an existing `soldr` PyPI record whose ownership and publisher settings live outside this repository.

## Audit: What The Published Release Artifacts Depend On

Based on the committed release workflow in `.github/workflows/release.yml`:

- the published `soldr` release archives are built from the repository source tree plus Rust dependencies resolved through Cargo
- the workflow does not explicitly download and repackage third-party release binaries into the published `soldr` archives
- the release path still depends on external services and package sources such as GitHub-hosted runners, `rustup`, crates.io, and GitHub APIs
- the live `apt` install and pinned third-party repository checkout are part of release-gating validation, not packaged release contents
- the managed `zccache` download path is runtime behavior in `soldr`, not an input to building the published `soldr` release artifacts

## E2E Validation External Dependencies

The bootstrap e2e jobs currently trust additional external systems:

- Ubuntu package repositories
  - musl validation jobs install `musl-tools` from live `apt` repositories
- third-party source repository hosting
  - the e2e workflow checks out a pinned commit of `zackees/running-process` from GitHub
- third-party Rust toolchain installation
  - the e2e workflow reads the third-party project's `rust-toolchain.toml` and installs that toolchain through `rustup`

These inputs are pinned where possible, but they are not yet mirrored or fully hermetic.

## Runtime Tool-Fetch Trust Boundaries

The current `soldr-fetch` implementation resolves tools using this live network chain:

1. local cache in `~/.soldr/bin`
2. crates.io API lookup for the crate's repository URL
3. GitHub release metadata for the crate's repository
4. GitHub release asset download for the selected platform archive

That means the runtime tool-fetch path currently trusts:

- crates.io metadata for crate existence and repository linkage
- GitHub repository ownership and release contents for the target tool
- HTTPS transport to those services

Integrity enforcement that is in place today:

- SHA-256 is always computed on the downloaded archive and printed to stderr
- user-supplied per-asset SHA-256 pins from `SOLDR_CHECKSUMS_FILE` are enforced as hard errors on mismatch
- `SOLDR_TRUST_MODE=strict` refuses any fetch without a matching pin

It does not yet enforce:

- maintainer allowlists
- repo-committed default checksums for tools shipped in the `known_tools` registry
- upstream signatures
- upstream artifact attestations
- mirrored internal copies of third-party tool binaries

## Current Policy Decisions

Current repo policy is:

- pin what we can inside the git tree
- make external trust edges explicit in documentation
- treat floating workflow refs as unacceptable
- prefer exact commit or version selection where possible
- `0.5.x` does not claim hermetic builds
- the documented release-time dependencies on GitHub-hosted runners, `rustup`, crates.io, GitHub APIs/Releases, live `apt` in the bootstrap e2e path, and the pinned third-party bootstrap repository are acceptable for `0.5.x`
- Cargo vendoring, toolchain mirroring, OS-package mirroring, and third-party bootstrap source mirroring are accepted future hardening work, but they are not blockers for the current `0.5.x` release line
- the pinned third-party bootstrap checkout is acceptable for current validation, but it must not be described as mirrored or hermetic
- release verification for `soldr` covers the published `soldr` artifacts and their provenance, not every external input used during CI

## Current Runtime Fetch Policy

As of the `0.6.x` line, `soldr` enforces integrity on every third-party fetch:

- every downloaded archive has its SHA-256 computed before extraction; the digest is printed to stderr so a human or CI log grep can audit what actually installed
- users can pin per-asset SHA-256 values in a TOML file at the path named by `SOLDR_CHECKSUMS_FILE`; any pin mismatch is a hard error regardless of mode
- `SOLDR_TRUST_MODE=strict` refuses to install any tool that does not have a matching pin
- `SOLDR_TRUST_MODE=permissive` (the default) installs and emits a `trust: unverified` warning when no pin is available; this preserves the convenience/bootstrap path while making the trust state legible
- the managed `zccache` download goes through the same verification path as any other fetch

Example pin file layout:

```toml
[[tool]]
tool = "cargo-nextest"
version = "0.9.100"
asset = "cargo-nextest-0.9.100-x86_64-pc-windows-msvc.zip"
sha256 = "0123...cdef"   # 64-char lowercase hex
```

What is still deferred:

- maintainer allowlists (limiting which crate/repo pairs `soldr` will even attempt to fetch)
- upstream signature or attestation verification
- mirrored internal copies of third-party tool binaries

Those remain acceptable future hardening work and are tracked on the issues linked below.

Open follow-up implementation issues are tracked in:

- [#11](https://github.com/zackees/soldr/issues/11) for repository and release-governance settings
- [#41](https://github.com/zackees/soldr/issues/41) for reducing live external release inputs
- [#42](https://github.com/zackees/soldr/issues/42) for fetched-binary trust enforcement (checksum enforcement landed; allowlist/signature work still open)

## Practical Reading Of This Document

If you are deciding whether to trust a published `soldr` release:

- trust the validated GitHub workflow run and artifact attestation for the released commit
- separately evaluate whether the remaining external build inputs are acceptable for your threat model
- separately evaluate whether `soldr` fetching third-party tool binaries at runtime is acceptable for your threat model, because that remains an upstream trust decision on `0.5.x`

Those are related but distinct trust decisions.
