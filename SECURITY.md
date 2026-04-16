# Security

## Scope

`soldr` is a binary bootstrap/build tool. Security for this repo is therefore about:

- release integrity
- supply-chain trust boundaries
- workflow and dependency immutability
- user verification of published artifacts

This document describes the current hardening posture for the `0.5.x` line and the remaining policy work before the first attested secure release.

Related documentation:

- [docs/RELEASE_VERIFICATION.md](./docs/RELEASE_VERIFICATION.md)
- [docs/RELEASE_GOVERNANCE_CHECKLIST.md](./docs/RELEASE_GOVERNANCE_CHECKLIST.md)
- [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md)

## Current hardening

The repository currently enforces several baseline controls:

- Rust dependencies are locked in `Cargo.lock`.
- CI and release builds use `cargo ... --locked`.
- CI now enforces `cargo fmt --check` and `cargo clippy -D warnings`.
- No Cargo `git` dependencies are currently used; dependencies resolve from crates.io.
- Third-party GitHub Actions in the repository workflows are pinned to full commit SHAs.
- The e2e third-party source input is pinned to an exact Git commit in the workflow inputs.
- Releases are promoted by `workflow_dispatch` for an exact commit SHA instead of publishing immediately on tag push.
- Release assets are attested in GitHub Actions before publication.
- Release publication uses a dedicated GitHub App instead of `GITHUB_TOKEN` or a PAT.
- Release tags matching `v*.*.*` are protected by a repository ruleset.
- Immutable GitHub Releases are enabled.

These controls reduce drift, but they do not make the full release pipeline hermetic.

The current `0.5.x` line does not claim hermetic builds, and it does not claim that third-party binaries fetched later by `soldr` are repository-verified just because `soldr` downloaded them.

## What is pinned

The repo aims to pin security-relevant inputs wherever practical:

- Cargo dependency graph: pinned by `Cargo.lock`
- GitHub Actions: pinned by full commit SHA
- E2E third-party test source: pinned by exact commit SHA
- Rust toolchain action implementation: pinned by full commit SHA

For release integrity, pinning by floating tag or branch is treated as insufficient for third-party workflow code.

## What is still external or mutable

Even with the current pinning, some inputs still come from external systems at build or runtime:

- rustup toolchain downloads during CI/release
- crates.io index and crate downloads unless dependencies are vendored or mirrored
- OS package repositories such as `apt` in musl jobs
- GitHub Releases as the publication surface, even though immutable releases are enabled
- third-party binary artifacts fetched by soldr at runtime

These boundaries are intentional or transitional. They must be documented so users understand what is and is not covered by our own release assurances.

## Versioning and update policy

Security-relevant versioning and pinning should follow these rules:

- Cargo dependency changes must update `Cargo.lock`.
- Workflow action upgrades should be explicit pull requests that update SHAs, not floating major tags.
- Third-party test inputs should remain pinned to exact commits.
- Release toolchain changes should be explicit and reviewed.

When a pinned dependency, action, or external input is updated, the change should be visible in git history and reviewed like code.

## Release trust model

Current state:

- releases are validated in GitHub Actions from an exact commit SHA
- the release workflow re-runs lint, build, test, integration, and e2e checks before publishing
- the release commit must be reachable from the protected `release` branch
- release tags are created through a GitHub App-backed workflow path
- release assets are published to GitHub Releases with a generated checksum manifest
- release assets are attested in GitHub Actions prior to publication
- the release workflow can also build hardened PyPI wheels, but enabling PyPI upload still requires Trusted Publisher registration on the existing `soldr` PyPI project
- immutable releases and protected tag settings still depend on repository configuration outside the git tree
- current user-facing verification guidance is checksum verification plus `gh attestation verify`

Current verification policy:

- checksum verification plus `gh attestation verify` is the official user-facing verification story
- GitHub CLI is the primary documented attestation-verification tool
- offline attestation bundles may be archived, but separate Sigstore tooling is not required for the normal verification path
- SBOM publication is not currently required for the release line
- reproducible-build claims are not currently made for `soldr`
- no extra signed release metadata is currently published beyond `SHA256SUMS` and GitHub provenance attestations

Current hermeticity and runtime-trust policy:

- `0.5.x` release verification covers published `soldr` artifacts, not every external input used during CI
- `0.5.x` does not claim hermetic builds; documented dependencies on GitHub-hosted runners, `rustup`, crates.io, GitHub APIs/Releases, live `apt`, and the pinned bootstrap test repository remain acceptable for this release line
- Cargo/toolchain/package mirroring or vendoring is deferred hardening tracked in issue [#41](https://github.com/zackees/soldr/issues/41)
- runtime third-party binary fetches are a convenience/bootstrap path on `0.5.x`, not a repository-side trust guarantee
- stronger trust enforcement for fetched binaries is tracked in issue [#42](https://github.com/zackees/soldr/issues/42)

Planned state, tracked in issue `#7`:

Remaining follow-up decisions before the attested secure `0.5` release, tracked in issues `#12` and `#13`:

- whether `0.5` should publish SBOMs in addition to provenance attestations
- whether `0.5` should claim reproducible builds and how that would be validated
- whether release-time dependencies should be vendored or mirrored
- what trust policy should apply to third-party binaries fetched by `soldr` at runtime

`1.0.0-rc` remains gated on actual compilation-cache integration rather than release hardening alone.

## Reporting a security issue

If you discover a vulnerability or supply-chain concern, open a GitHub issue unless the issue is sensitive enough that public disclosure would create immediate risk.

For high-risk undisclosed issues, contact the maintainers privately before opening a public issue.
