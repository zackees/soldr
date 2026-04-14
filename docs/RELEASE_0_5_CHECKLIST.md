# Release 0.5 Checklist

This document is the concrete execution list for the first attested secure `soldr 0.5` release.

It assumes the current repository state on April 13, 2026:

- the validated release workflow exists in `.github/workflows/release.yml`
- the workflow requires an exact commit SHA on `release`
- publication uses a dedicated GitHub App
- `v*.*.*` tags are protected by repository rulesets
- immutable GitHub Releases are enabled
- `main` and `release` are protected

## 1. Freeze The `0.5` Product Surface

Decide what `0.5` means for the user-facing CLI.

Required decision:

- the `0.5` release line is the front-door build and tool-fetch line
- compilation caching is explicitly out of scope for `0.5`
- `status`, `clean`, `config`, and `cache` must not be presented as complete features unless implemented

Do not ship `0.5` with placeholder commands presented as finished behavior.

## 2. Freeze The `0.5` Security Claim

Resolve the open policy questions in:

- issue [#12](https://github.com/zackees/soldr/issues/12) for verification, SBOM, and reproducibility policy
- issue [#13](https://github.com/zackees/soldr/issues/13) for hermeticity and runtime trust policy

Minimum decisions needed before `0.5`:

- whether checksum plus `gh attestation verify` is the official verification story
- whether SBOM generation is required for `0.5`
- whether reproducible-build claims are in scope for `0.5`
- whether vendoring or mirroring Cargo, toolchain, or OS-package inputs is required now or deferred
- what trust statement applies to third-party binaries fetched by `soldr` at runtime

## 3. Rehearse The Release Path

Before the first public `0.5` tag, run a dry-run rehearsal from `release`.

Inputs to choose:

- candidate version such as `v0.5.0-rc1`
- exact commit SHA on `release`

Required checks:

- the workflow rejects SHAs not reachable from `release`
- the workflow completes lint, test, packaging, and e2e gates for the exact SHA
- the `release` environment approval gate triggers as expected
- the GitHub App token step succeeds
- checksums and provenance attestation steps succeed

Recommended command path:

```bash
gh workflow run release.yml \
  --ref release \
  -f version=v0.5.0-rc1 \
  -f commit_sha=<40-char-sha> \
  -f dry_run=true
```

## 4. Verify The Governance Controls Manually

Before the first real release, verify the controls that live outside the git tree.

```bash
gh api repos/zackees/soldr/rulesets
gh api repos/zackees/soldr/environments
gh api repos/zackees/soldr/branches/main/protection
gh api repos/zackees/soldr/branches/release/protection
gh api repos/zackees/soldr/immutable-releases
```

Success criteria:

- the `v*.*.*` tag ruleset is active
- only the release GitHub App can bypass that ruleset
- `release` still requires approval from `@zackees`
- `main` and `release` still require the expected checks
- immutable releases remain enabled

## 5. Cut A Real Release Candidate

After the dry run passes, create the first real release candidate through the workflow.

Recommended first tag:

- `v0.5.0-rc1`

Required checks after publication:

- the GitHub Release contains all expected platform archives
- the `SHA256SUMS` manifest is present
- `gh attestation verify` succeeds for at least one downloaded archive
- a human cannot create, move, or delete the release tag manually
- the release cannot be mutated after publication because immutable releases are enabled

Recommended verification commands:

```bash
gh release view v0.5.0-rc1 --repo zackees/soldr
gh attestation verify soldr-v0.5.0-rc1-x86_64-unknown-linux-gnu.tar.gz \
  --repo zackees/soldr \
  --signer-workflow zackees/soldr/.github/workflows/release.yml
```

## 6. Audit The First Published Release

After `v0.5.0-rc1`, confirm that reality matches the docs:

- [RELEASE.md](../RELEASE.md)
- [RELEASE_VERIFICATION.md](./RELEASE_VERIFICATION.md)
- [RELEASE_GOVERNANCE_CHECKLIST.md](./RELEASE_GOVERNANCE_CHECKLIST.md)
- [TRUST_BOUNDARIES.md](./TRUST_BOUNDARIES.md)

Anything discovered during the RC must become either:

- a code fix
- a GitHub settings fix
- an explicit documented limitation

## 7. Decide Go Or No-Go For `v0.5.0`

Ship `v0.5.0` only when all of these are true:

- the intended `0.5` CLI surface is clearly scoped and honestly documented
- the release workflow has passed in dry-run and real-release modes
- the first RC was verified with checksums and attestations
- the protected-tag and immutable-release controls behaved as expected
- the remaining policy questions are closed or explicitly deferred in writing

## 8. Gate `1.0.0-rc`

Do not cut `1.0.0-rc` until:

- zccache-style compilation caching is actually integrated
- the wrapper does real cache lookup and store work
- the public cache commands describe real behavior instead of placeholders

If those are not true, stay on the `0.5.x` line.
