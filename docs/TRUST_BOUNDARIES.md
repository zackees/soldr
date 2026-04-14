# Trust Boundaries

This document describes the current external trust boundaries for `soldr`.

It is a factual inventory of what the project trusts today, not a claim that every trust edge is already ideal.

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

## Current Policy Direction

Current repo policy is:

- pin what we can inside the git tree
- make external trust edges explicit in documentation
- treat floating workflow refs as unacceptable
- prefer exact commit or version selection where possible

Open follow-up decisions are tracked in:

- [#11](https://github.com/zackees/soldr/issues/11) for repository and release-governance settings
- [#13](https://github.com/zackees/soldr/issues/13) for hermeticity and runtime trust hardening

## Practical Reading Of This Document

If you are deciding whether to trust a published `soldr` release:

- trust the validated GitHub workflow run and artifact attestation for the released commit
- separately evaluate whether the remaining external build inputs are acceptable for your threat model
- separately evaluate whether `soldr` fetching third-party tool binaries at runtime is acceptable for your threat model

Those are related but distinct trust decisions.
