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

It does not yet enforce:

- maintainer allowlists
- per-tool checksums committed in this repo
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

Current `0.5.x` policy for runtime-fetched binaries is:

- `soldr` fetching a third-party tool is a convenience/bootstrap path, not a repository-side trust guarantee
- a successful fetch currently means crates.io metadata and GitHub Releases produced a matching archive for the selected target and `soldr` extracted it successfully
- `soldr` does not currently enforce maintainer allowlists, repo-managed checksums, upstream signatures, upstream attestations, or mirrored copies before executing a fetched tool
- the managed `zccache` path is pinned to a repo-chosen version, but it still uses the same direct GitHub Release download model and is not independently checksum- or attestation-verified by `soldr` itself today
- stronger runtime trust enforcement is accepted follow-up work, but it is not part of the current `0.5.x` trust claim

Open follow-up implementation issues are tracked in:

- [#11](https://github.com/zackees/soldr/issues/11) for repository and release-governance settings
- [#41](https://github.com/zackees/soldr/issues/41) for reducing live external release inputs
- [#42](https://github.com/zackees/soldr/issues/42) for fetched-binary trust enforcement

## Practical Reading Of This Document

If you are deciding whether to trust a published `soldr` release:

- trust the validated GitHub workflow run and artifact attestation for the released commit
- separately evaluate whether the remaining external build inputs are acceptable for your threat model
- separately evaluate whether `soldr` fetching third-party tool binaries at runtime is acceptable for your threat model, because that remains an upstream trust decision on `0.5.x`

Those are related but distinct trust decisions.
