# Release Guide

This document records the release model `soldr` uses today.

It is written for two audiences:

- the repository owner, who configures the GitHub-side controls
- a future agent, who needs to understand what the release workflow is supposed to enforce

## Goal

The release model should satisfy all of these properties:

- releases are triggered intentionally by a reviewed version-bump merge on `main`
- the machine validates the exact merged commit before publication
- release tags such as `v1.2.3` are minted only by trusted automation
- humans cannot manually create, move, or delete release tags
- published release assets are immutable, attestable, and verifiable

## Current State

These controls are already in place:

- `main` is protected with required CI and e2e checks
- the `release` environment exists and requires approval from `@zackees`
- immutable GitHub Releases are enabled
- GitHub Actions requires full-SHA pinning for third-party actions
- `.github/workflows/release-auto.yml` is the only release workflow
- a dedicated GitHub App is used for release publication
- release tags matching `refs/tags/v*.*.*` are protected by a repository ruleset whose only bypass actor is the release GitHub App

crates.io publication is not part of the current release direction. `soldr` is being released as a hardened binary tool, not as a promised Rust library API surface.

## Current Release Model

The current GitHub-native release model is:

1. A reviewed PR bumps the workspace version in `Cargo.toml`.
2. That PR is merged to protected `main`.
3. `.github/workflows/release-auto.yml` triggers from that push.
4. The workflow confirms the workspace version actually changed.
5. The workflow reruns lint, build, tests, integration, and e2e gates on the merged commit.
6. Final publication pauses in the `release` environment.
7. A dedicated GitHub App creates the `v*.*.*` tag and GitHub Release.
8. The same workflow publishes hardened PyPI wheels through Trusted Publishing.

The intentional authorization step is the reviewed version-bump merge. The `release` environment remains the publish-time credential boundary.

## Why A GitHub App Is Needed

On this personal repository, GitHub would not let the built-in `github-actions` integration be used as the bypass actor for a release-tag ruleset.

That means:

- `GITHUB_TOKEN` is not enough for workflow-only protected tags here
- a personal access token is not acceptable for the target trust model
- a dedicated GitHub App is the recommended machine identity

## Owner Setup Checklist

These are the concrete steps the owner must perform in GitHub.

### 1. Keep `main` Protected

Desired branch policy:

- no direct human pushes
- pull-request-only updates
- linear history enabled
- force pushes disabled
- deletions disabled
- required checks enabled

The release workflow assumes that any version bump reaching `main` has already passed the required validation gate.

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

Store these on the `release` environment, not as broad repository-wide secrets.

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

### 6. Keep The `release` Environment As The Publish Boundary

The owner should remain the required reviewer for the `release` environment.

That keeps the GitHub App credentials and the PyPI Trusted Publisher identity scoped to the final publication stage instead of the entire workflow.

## Current Workflow Behavior

The release workflow now does this:

1. Trigger on pushes to `main` where `Cargo.toml` changed.
2. Derive `vX.Y.Z` directly from `[workspace.package]` in `Cargo.toml`.
3. Stop early if the version did not change from the previous `main` commit.
4. Refuse to continue if the tag or GitHub Release already exists.
5. Rerun the full lint, test, packaging, and e2e gate on the merged commit.
6. Build the signed release archives and hardened wheel set.
7. Pause on the `release` environment.
8. Mint a GitHub App installation token inside the workflow.
9. Use that App token to create the tag and GitHub Release.
10. Attach checksums and build provenance attestations.
11. Publish the wheel set to PyPI through OIDC Trusted Publishing.

The release workflow must not use:

- a PAT for tag creation
- manual tag creation outside automation

## Future Agent Instructions

If a future agent is asked to audit or extend the release flow, the agent should:

1. Read this file.
2. Audit the live GitHub-side controls before trusting the checked-in docs.
3. Confirm whether the GitHub App remains installed and scoped only to this repository.
4. Confirm whether `main` still requires pull requests and the expected validation checks.
5. Confirm whether the `release` environment still exists and gates publication.
6. Confirm whether a tag ruleset still protects `v*.*.*` and only the App may bypass it.
7. Check issues `#12` and `#13` for the remaining `0.5` policy decisions.
8. Only then modify `.github/workflows/release-auto.yml` or the verification docs.

If the live GitHub-side controls drift from the documented trust model, the agent should stop and report the drift instead of assuming the release posture is still intact.

## Verification Checklist

After the final setup is complete, verify all of these:

- a human cannot manually create `v1.2.3`
- a human cannot move `v1.2.3` to a different commit
- the release workflow can create `v1.2.3` after the `release` environment approves
- the workflow refuses duplicate tags and duplicate releases
- published release assets are immutable
- checksums and provenance attestations are attached

## Related Documents

- [README.md](./README.md)
- [SECURITY.md](./SECURITY.md)
- [docs/RELEASE_0_5_CHECKLIST.md](./docs/RELEASE_0_5_CHECKLIST.md)
- [docs/RELEASE_GOVERNANCE_CHECKLIST.md](./docs/RELEASE_GOVERNANCE_CHECKLIST.md)
- [docs/RELEASE_VERIFICATION.md](./docs/RELEASE_VERIFICATION.md)
- [docs/TRUST_BOUNDARIES.md](./docs/TRUST_BOUNDARIES.md)
