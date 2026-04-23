# Release Guide

This document records the release model `soldr` uses today.

It is written for two audiences:

- the repository owner, who configures the GitHub-side and PyPI-side controls
- a future agent, who needs to understand what the release workflow enforces

## Goal

The release model satisfies these properties:

- a release is triggered by bumping `workspace.package.version` in `Cargo.toml` on `main`
- CI must pass on the bumping commit before any release artifacts are published
- the matching `vX.Y.Z` tag and GitHub Release are minted by the workflow itself
- wheels are published to PyPI through OIDC Trusted Publishing
- no human approval and no GitHub App credentials are required at release time
- published release assets are immutable, attestable, and verifiable

## Current State

These controls are in place:

- `main` is protected with required CI and e2e checks
- immutable GitHub Releases are enabled
- GitHub Actions requires full-SHA pinning for third-party actions
- `.github/workflows/release-auto.yml` is the only release workflow
- PyPI publication uses OIDC Trusted Publishing bound to `release-auto.yml`

crates.io publication is not part of the current release direction. `soldr` is being released as a hardened binary tool, not as a promised Rust library API surface.

## Current Release Model

The release flow is unattended and PyPI-centric:

1. A reviewed PR bumps the workspace version in `Cargo.toml`.
2. That PR is merged to protected `main`.
3. `.github/workflows/release-auto.yml` triggers from that push (it is filtered to `paths: Cargo.toml`).
4. The `prepare` job derives `vX.Y.Z` from `Cargo.toml`, looks up the latest version on PyPI, and sets `should_release=true` only when the Cargo version is strictly greater. It also records `tag_exists` for the catch-up case.
5. The workflow reruns lint, build, tests, integration, and e2e gates on the merged commit.
6. The `build` and `build-pypi` jobs produce the platform archives and hardened wheel set.
7. The `publish` job — gated on `tag_exists == 'false'` — uses the workflow's built-in `GITHUB_TOKEN` to create the `vX.Y.Z` GitHub Release with `SHA256SUMS.txt` and a build provenance attestation.
8. The `publish-pypi` job uploads the wheel set through `pypa/gh-action-pypi-publish` using OIDC.

The intentional authorization step is the reviewed version-bump merge. PyPI is the source of truth for "have we already shipped this," which means a Cargo bump that is ahead of PyPI but behind the latest GitHub tag still publishes to PyPI without re-creating the GitHub Release.

## Owner Setup

These are the one-time controls that make the unattended flow work.

### 1. Keep `main` Protected

Desired branch policy:

- no direct human pushes
- pull-request-only updates
- linear history enabled
- force pushes disabled
- deletions disabled
- required checks enabled

The release workflow assumes that any version bump reaching `main` has already passed the required validation gate.

### 2. Register PyPI Trusted Publisher

Register a GitHub publisher on https://pypi.org/manage/project/soldr/settings/publishing/ as a maintainer of the `soldr` project:

- Owner: `zackees`
- Repository: `soldr`
- Workflow filename: `release-auto.yml`
- Environment: leave blank

There is no environment approval boundary in the unattended model.

### 3. Tag Protection

If `refs/tags/v*.*.*` is protected by a repository ruleset, the built-in `github-actions` integration must be an allowed bypass actor — otherwise the workflow cannot mint the tag with `GITHUB_TOKEN`. If tag protection is not required for this model, the ruleset can be disabled.

The previous GitHub App tag-creation path is not used. Any `RELEASE_APP_ID` variable, `RELEASE_APP_PRIVATE_KEY` secret, and the standalone release App can be removed.

## Future Agent Instructions

Before changing the release workflow:

1. Read this file.
2. Audit the live GitHub-side controls before trusting the checked-in docs.
3. Confirm the PyPI Trusted Publisher for `release-auto.yml` is still registered.
4. Confirm `main` still requires pull requests and the expected validation checks.
5. Confirm tag protection (if any) still permits `github-actions` to mint `v*.*.*` tags.
6. Confirm that `Cargo.toml` workspace version is still the trigger surface — do not reintroduce `workflow_dispatch` or environment approval boundaries without explicit owner instruction.

If the live GitHub-side or PyPI-side controls drift from the documented flow, stop and report the drift instead of assuming the release posture is intact.

When preparing a normal release:

1. Bump `[workspace.package].version` in `Cargo.toml`.
2. Bump `package.json` to the exact same version.
3. Confirm the candidate is not already published on PyPI as `soldr` and not already published on npm as `@zackees/soldr`.
4. Confirm `git ls-remote --tags origin vX.Y.Z` does not find the candidate tag.
5. Open and merge the version-bump PR to `main`; the release workflow creates the `vX.Y.Z` tag and GitHub Release from that merge commit.

If `Autonomous Release` is run manually without an unpublished package version, the prepare job will set `should_release=false` and all build/publish jobs will be skipped. That is expected behavior, not a release failure. For a new release, prepare a new version-bump PR instead of manually rerunning the workflow.

## Verification Checklist

After an autonomous release:

- the `Autonomous Release` run on `main` reports `should_release=true` and the expected `version=vX.Y.Z`
- a non-draft GitHub Release exists at `vX.Y.Z` with the platform archives and `SHA256SUMS.txt`
- `https://pypi.org/project/soldr/X.Y.Z/` shows `Uploaded using Trusted Publishing? Yes`
- `gh attestation verify dist/soldr-vX.Y.Z-*-SHA256SUMS.txt --repo zackees/soldr` succeeds against a downloaded archive

## Related Documents

- [README.md](./README.md)
- [SECURITY.md](./SECURITY.md)
- [docs/PYPI_TRUSTED_PUBLISHING.md](./docs/PYPI_TRUSTED_PUBLISHING.md)
- [docs/RELEASE_VERIFICATION.md](./docs/RELEASE_VERIFICATION.md)
- [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md)
