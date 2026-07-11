# Cross-workspace build-script reuse audit (#1545)

## Result

The proposed cross-workspace build-script bundle is not safe to enable in the
current rust-plan design. Keep workspace-specific rust-plan reuse disabled and
close #1545 as unsafe pending a Cargo-authoritative completeness contract.

## Existing boundaries

- `build_rust_artifact_plan` records the normalized workspace root, target
  directory, lockfile hash, Cargo config hash, and workspace manifest hashes.
- The vendored zccache rust-plan identity includes both `workspace_root` and
  `target_dir` in its cache key. Plans from unrelated workspaces therefore do
  not alias.
- Soldr's warm-restore shortcut separately requires the same plan input hash
  and target directory. A workspace change falls through to the normal restore
  path instead of reusing the previous sentinel.
- The plan retains build-script metadata and output, but an arbitrary build
  script may observe undeclared environment, host, filesystem, toolchain, or
  network state. Cargo's `rerun-if-changed` output is not a complete declaration
  of those inputs.

Relaxing the workspace portion of the key would therefore trade a measurable
cache hit for a correctness hole: stale generated Rust, native outputs, or
Cargo dirty-propagation state could be restored into an unrelated workspace.

## Validation

The existing rust-plan test suite covers the relevant safety boundary:

```text
soldr cargo test -p soldr-cli --lib rust_plan_tests::warm_restore
```

In particular, the warm-restore tests verify exact-match skipping and reject
mismatched target directories or run identity. The plan builder also hashes
workspace manifests, lockfiles, and Cargo configuration. This is sufficient
for the current safe behavior, but not sufficient to prove complete,
cross-workspace build-script input closure.

## Reopening criteria

Revisit this issue only after a Cargo-authoritative per-unit input contract can
prove all of the following for a selected build script: declared file and
environment inputs, compiler/toolchain/target ABI identity, native linker
inputs, generated output identity, and exact dirty propagation. The default
must remain workspace-specific until that proof exists.
