# Release Guide

This document memorializes the intended high-security release flow for `soldr`.

It is written for two audiences:

- the repository owner, who must configure the GitHub-side controls
- a future agent, who must understand what the release workflow is supposed to enforce

## Goal

The release model should satisfy all of these properties:

- releases are triggered intentionally by the owner
- the machine validates the exact release commit before publication
- release tags such as `v1.2.3` are minted only by trusted automation
- humans cannot manually create, move, or delete release tags
- published release assets are immutable, attestable, and verifiable

## Current State

These controls are already in place:

- `main` is protected with required CI and e2e checks
- `release` is protected with the same required validation gates
- the `release` environment exists and requires approval from `@zackees`
- immutable GitHub Releases are enabled
- GitHub Actions requires full-SHA pinning for third-party actions
- the validated release workflow exists in `.github/workflows/release.yml`
- a dedicated GitHub App is used for release publication
- release tags matching `refs/tags/v*.*.*` are protected by a repository ruleset whose only bypass actor is the release GitHub App

The remaining work before the attested secure `0.5` release is operational rather than architectural:

- exercise the full release path with a rehearsal and first release candidate
- verify that humans cannot mint, move, or delete release tags in practice
- finish policy decisions around SBOMs, reproducibility, and hermeticity
- register the existing `soldr` PyPI project for Trusted Publishing if hardened wheel upload is in scope for `0.5.0`

`1.0.0-rc` remains intentionally reserved for broader release hardening and bootstrap validation beyond the `0.5.x` built-in zccache release line.

crates.io publication is not part of the current release direction. `soldr` is being released as a hardened binary tool, not as a promised Rust library API surface.

## Recommended Final Model

The strongest GitHub-native model for this repository is:

1. A human starts the release intentionally.
2. The workflow validates an exact commit on the protected `release` branch.
3. The workflow reruns lint, build, tests, integration, and e2e gates.
4. The owner approves the `release` environment.
5. A dedicated GitHub App creates the `v*.*.*` tag and GitHub Release.
6. Humans are blocked from minting or retargeting release tags directly.

`workflow_dispatch` is the release authorization step.

The GitHub App is the machine identity that performs the irreversible tag-creation step.

## Why A GitHub App Is Needed

On this personal repository, GitHub would not let the built-in `github-actions` integration be used as the bypass actor for a release-tag ruleset.

That means:

- `GITHUB_TOKEN` is not enough for workflow-only protected tags here
- a personal access token is not acceptable for the target trust model
- a dedicated GitHub App is the recommended machine identity

## Owner Setup Checklist

These are the concrete steps the owner must perform in GitHub.

### 1. Create A Protected `release` Branch

The branch should be the only branch from which release commits are promoted.

Desired branch policy:

- no direct human pushes
- pull-request or automation-only updates
- linear history enabled
- force pushes disabled
- deletions disabled
- required checks enabled

The release workflow should later validate that the target SHA is on `release`.

### 2. Create A Dedicated GitHub App

Create a private GitHub App under the owner account and install it only on `zackees/soldr`.

Use it only for release automation.

Start with minimal permissions:

- `Metadata: Read`
- `Contents: Read and write`

If GitHub Release creation needs an additional narrow permission during implementation, add only that permission and document it.

### 3. Generate And Store App Credentials

Create a private key for the GitHub App and store the App identity in GitHub configuration.

Suggested configuration:

- secret: `RELEASE_APP_PRIVATE_KEY`
- variable: `RELEASE_APP_ID`

Store these on the `release` environment if possible, not as broad repository-wide secrets.

### 4. Install The App On This Repository

Install the App only on `zackees/soldr`.

Avoid broad installation scope.

### 5. Create A Tag Ruleset For Release Tags

Protect `refs/tags/v*.*.*` with a ruleset that blocks:

- tag creation
- tag update
- tag deletion

The only bypass actor should be the dedicated GitHub App.

Do not use:

- a maintainer-user bypass
- a PAT-backed workflow
- a generic human admin exception and call it equivalent

Those weaken the trust model.

### 6. Keep The `release` Environment As The Human Approval Gate

The owner should remain the required reviewer for the `release` environment.

That means the human decides when a release is authorized, while the machine identity performs tag issuance and publication.

## Current Workflow Behavior

The release workflow now does this:

1. Accept a version and exact commit SHA.
2. Verify the commit SHA is on `release`.
3. Rerun the full release gate on that exact commit.
4. Pause on the `release` environment for approval.
5. Mint a GitHub App installation token inside the workflow.
6. Use that App token to create the tag.
7. Use that same App token to create the GitHub Release.
8. Attach checksums and build provenance attestations.
9. Optionally build hardened platform wheels and publish them to PyPI through OIDC Trusted Publishing.

The release workflow must not use:

- a PAT for tag creation
- manual tag creation outside automation

## Future Agent Instructions

If a future agent is asked to audit or extend the release flow, the agent should:

1. Read this file.
2. Audit the live GitHub-side controls before trusting the checked-in docs.
3. Confirm whether the GitHub App remains installed and scoped only to this repository.
4. Confirm whether `release` still exists and is protected.
5. Confirm whether a tag ruleset still protects `v*.*.*` and only the App may bypass it.
6. Check issues `#12` and `#13` for the remaining `0.5` policy decisions.
7. Only then modify `.github/workflows/release.yml` or the verification docs.

If the live GitHub-side controls drift from the documented trust model, the agent should stop and report the drift instead of assuming the release posture is still intact.

## Verification Checklist

After the final setup is complete, verify all of these:

- a human cannot manually create `v1.2.3`
- a human cannot move `v1.2.3` to a different commit
- the release workflow can create `v1.2.3` after approval
- the workflow refuses SHAs not on `release`
- published release assets are immutable
- checksums and provenance attestations are attached

## Related Documents

- [README.md](./README.md)
- [SECURITY.md](./SECURITY.md)
- [docs/RELEASE_0_5_CHECKLIST.md](./docs/RELEASE_0_5_CHECKLIST.md)
- [docs/RELEASE_GOVERNANCE_CHECKLIST.md](./docs/RELEASE_GOVERNANCE_CHECKLIST.md)
- [docs/RELEASE_VERIFICATION.md](./docs/RELEASE_VERIFICATION.md)
- [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md)
