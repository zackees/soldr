# Lock-scoped registry bundle audit (#1533)

## Result

The current registry cache is already keyed by the complete lockfile hash,
while its payload stays in the package manager's native layout. Do not replace
it with a primary-only bundle without measurements showing that selection and
reconstruction reduce total restore plus offline-build time.

## Current behavior

- The soldr action derives the registry key from the first 16 hex characters of
  the lockfile SHA-256, so different lockfiles do not share registry bundles.
- The registry saver archives only `registry/index`, `registry/cache`, and
  `git/db` trees. It excludes credentials, package-manager config, extracted
  source trees, and unrelated home-directory state.
- The non-native action path restores the same registry/cache and git-db
  directories directly; the native path restores the equivalent archive.
- Preserving the native layout retains registry separation and checksum/source
  resolution behavior. The key prevents stale state from being selected for a
  different lockfile, but it does not claim every archived file is needed.

## Why narrowing is not enabled

An exact lock closure would need to resolve registry identities and versions,
alternate registries, git revisions, and submodule/object dependencies before
archiving. Restore would then need to rebuild the expected index/cache/git
layout before offline metadata and fetch operations. A smaller archive alone
does not establish a win if selection and reconstruction cost more than
restoring the existing bundle.

The safe experiment needs registry, alternate-registry, git/submodule, and
native fixtures, with three repetitions of archive size/file count, restore,
offline metadata/fetch, reconstruction, and first-build timings. Until those
measurements exist, the current lock-keyed native-layout bundle is the safer
baseline.

## Reopening criteria

Reopen only with measured total-time improvement and tests proving checksums,
source identity, exact git OIDs/submodules, registry separation, credential
exclusion, and checkpoint-safe global-cache handling. Archive-size-only
improvements are insufficient.
