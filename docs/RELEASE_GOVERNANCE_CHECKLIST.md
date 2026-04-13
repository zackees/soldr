# Release Governance Checklist

This checklist covers the GitHub-side controls that matter for `soldr` releases but do not live entirely in the git tree.

Use it when auditing the repository settings before the first real public release and periodically afterward.

## Why This Exists

The repository now contains a validated release workflow, but some of the strongest release controls are still repository settings rather than committed files.

If those settings drift, the workflow may remain correct while the operational release posture becomes weaker than intended.

## Target State

The intended release-governance state is:

- release tags are protected by rulesets
- the normal release path goes through the validated workflow
- the `release` environment exists and requires explicit approval
- immutable GitHub Releases are enabled
- maintainers can audit these settings without guessing where they live

## Current Observed Repository State

Observed on April 13, 2026 via GitHub API:

- no repository rulesets
- no GitHub environments
- `main` branch is not protected
- no published GitHub Releases yet

Those observations were produced with:

```bash
gh api repos/zackees/soldr/rulesets
gh api repos/zackees/soldr/environments
gh api repos/zackees/soldr/branches/main/protection
gh api repos/zackees/soldr/releases?per_page=5
```

The branch-protection query currently returns `404 Branch not protected`, which is itself the signal that branch protection is absent.

## Audit Steps

### 1. Check Rulesets

Confirm that release tags are governed by repository rulesets:

```bash
gh api repos/zackees/soldr/rulesets
```

What to look for:

- a ruleset that applies to release tag patterns such as `v*`
- protection against moving or deleting release tags
- restrictions that make the validated workflow the normal release path

### 2. Check The Release Environment

Confirm that the `release` environment exists:

```bash
gh api repos/zackees/soldr/environments
```

What to look for:

- an environment named `release`
- documented approval policy for release promotion

Some approval details may need to be verified in the GitHub UI if they are not exposed cleanly by the API surface available to maintainers.

### 3. Check Branch Protection Or Equivalent Rulesets

Confirm that the default branch is protected either by branch protection or a ruleset-based equivalent:

```bash
gh api repos/zackees/soldr/branches/main/protection
```

What to look for:

- protection is present, or
- an equivalent ruleset is enforcing the same control path

### 4. Check Published Releases

Confirm that releases are being created through the validated workflow path:

```bash
gh api repos/zackees/soldr/releases?per_page=5
```

What to look for:

- release assets and checksum manifest are present
- releases correspond to expected version tags
- the release inventory matches the workflow outputs

### 5. Check Immutable Releases

Immutable releases are configured in repository or organization settings rather than in this repository's source tree.

What to verify in the GitHub UI:

- immutable releases are enabled for the repository or governing organization
- maintainers understand how draft release creation and publication interact with immutability

Until immutable releases are enabled, treat artifact attestations and checksum verification as stronger than release-page mutability guarantees.

## Operational Rule

If any of the settings above are absent or weaker than intended, do not treat the repository as fully matching the planned release-governance model even if the workflows continue to pass.

## Related Documents

- [../SECURITY.md](../SECURITY.md)
- [RELEASE_VERIFICATION.md](./RELEASE_VERIFICATION.md)
- [TRUST_BOUNDARIES.md](./TRUST_BOUNDARIES.md)
