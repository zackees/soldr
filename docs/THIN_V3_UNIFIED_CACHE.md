# Thin-v3 unified Rust cache research and architecture decision

Status: proposed for acceptance by issue #1609 review. This is research, not
a production-profile switch. Thin-v1 and thin-v2 remain unchanged until the
implementation issues pass the acceptance matrix and soak gate.

## Decision

Thin-v3 is a graph of manifests over one shared content-addressed store, not a
third archive of compiler outputs.

| Concern | Authoritative owner | Durable representation |
|---|---|---|
| Fresh/Dirty decision | Rust package manager | fingerprints, dep-info, output existence, mtimes |
| Compiler output bytes | shared zccache CAS | one hash-addressed blob plus metadata |
| Long-lived dependency world | soldr cook | base/delta manifests and CAS pins |
| Checkout-local state | thin-v3 | freshness metadata, build-script state, CAS references |
| Registry/git packages | package home | package-manager-owned cache |
| Toolchain/sysroot | setup-soldr | versioned toolchain cache |

A compiler output may be materialized into `target/` by reflink or copy, but
the uploaded byte sequence has one durable owner. Cook and thin-v3 retain only
the hash, size, target-relative path, class, unit identity, and mtime. They may
not upload another copy of a blob larger than 4 KiB.

This resolves the v1/v2 dead end: deleting `.rlib`/`.rmeta` makes a unit Dirty
because output existence is load-bearing, while archiving them in every layer
restores Fresh at the cost of duplicating the dominant bytes.

## Evidence from C&#97;rgo 1.94

The source snapshot is
[`zackees/rust-docs@9794a7b`](https://github.com/zackees/rust-docs/tree/9794a7b9bce7b732e147f8c119dfa0d3a6fe1d0f/source/1.94.1/c%61rgo).

### Fingerprints and output existence

The package manager stores a fingerprint hash on disk and checks the
filesystem. Its
[fingerprint module](https://github.com/zackees/rust-docs/blob/9794a7b9bce7b732e147f8c119dfa0d3a6fe1d0f/source/1.94.1/c%61rgo/src/c%61rgo/core/compiler/fingerprint/mod.rs)
documents and implements these rules:

- every expected output must exist;
- unit output mtimes are compared with dependency output mtimes;
- source mtimes/checksums are checked through translated dep-info;
- `invoked.timestamp`, fingerprint JSON/hash records, and build-script output
  timestamps participate in freshness;
- `-Zchecksum-freshness` replaces source mtime checks with size/checksum
  checks, but is nightly-only and does not remove compiled-output existence.

Thus metadata-only thin-v2 cannot make dependencies Fresh. The package manager
invokes the wrapper; a zccache hit can avoid rustc work but still pays
scheduling, wrapper IPC, materialization, and downstream mtime propagation.

### `target-dir` versus `build-dir`

The 1.94
[build-cache reference](https://github.com/zackees/rust-docs/blob/9794a7b9bce7b732e147f8c119dfa0d3a6fe1d0f/source/1.94.1/c%61rgo/src/doc/src/reference/build-cache.md)
separates final user artifacts in `target-dir` from intermediate artifacts in
`build-dir`. This is a useful deterministic walking/measurement boundary, but
not a cache protocol:

- the intermediate layout is internal and subject to change;
- dependencies, fingerprints, build scripts, and incremental data share it;
- host and explicit-target units still need separate handling;
- setup-soldr supports older toolchains without the stabilized setting.

Decision: use `build-dir` opportunistically when supported. Correctness uses a
complete JSON closure plus a safe classified fallback walker.

### JSON, build-plan, and SQLite boundaries

`compiler-artifact` supplies primary Rust outputs. `build-script-executed`
supplies `OUT_DIR`, linked paths, cfg, and environment, but not every arbitrary
generated/native file. Interrupted streams, unknown messages, unsupported
versions, and incomplete command coverage set `fallback_walker_used=true`.
Build-plan and unit-graph data remain unstable.

The package home's `.global-cache` SQLite tables track last-use data for
registry index/crate/source and git database/checkout entries for package-cache
garbage collection. They contain no target-unit fingerprints, output paths, or
compiled output identity. This is neither rust-plan state nor a CAS index and
must not be archived with thin-v3.

## Current-system inventory

[`bench/thin_v3_inventory.py`](../bench/thin_v3_inventory.py) walks named
layers, classifies files, hashes contents, computes raw and deterministic
gzip-comparable sizes, and attributes duplicate groups. The
[raw inventory](../bench/results/thin-v3/zccache-windows-2026-07-11.json) and
[run summary](../bench/results/thin-v3/zccache-windows-2026-07-11-summary.json)
are committed.

Fixture: `zackees/zccache@2601a927`, Windows x86-64 MSVC, a locked debug build
of package `zccache`, soldr `49e7ca41`. Cold profiles used isolated empty
target, target-cache, and requested zccache roots.

| Metric | thin-v1 | current thin-v2 | v2 - v1 |
|---|---:|---:|---:|
| Files | 2,110 | 2,129 | +19 |
| Raw bytes | 889,412,013 | 921,715,449 | +32,303,436 |
| Deterministic gzip bytes | 269,867,427 | 280,325,297 | +10,457,870 |
| Cold wall time | 163.9 s | 154.5 s | -9.4 s |
| Fresh-target restore/build | 71.2 s / 64 compile lines | 42.1 s / 0 compile lines | v2 is functional |

The v2 size increase is almost entirely 18 proc-macro/shared-library files
(31,896,576 raw bytes). Both profiles contain 535,846,868 bytes of `.rlib` and
297,182,582 bytes of `.rmeta`; those classes are 90.4% of thin-v2 raw payload.
Across v1/v2 snapshots, 695,894,225 bytes are content-identical.

Thin-v1 needed repeated stabilization: no-op runs compiled 113, then 5, then 0
units. Thin-v2's three valid no-op observations compiled zero. A small archive
and a functional warm cache are different axes.

The requested zccache root contained only plans/logs during this self-host
fixture and no compiler payloads. Target-cache-to-zccache duplicate bytes are
therefore **unknown**, not zero. The implementation runner must reject an
unexpected empty layer.

Reproduce an inventory after generating every layer:

```text
uv run --no-project python bench/thin_v3_inventory.py \
  --layer thin-v1=PATH/TO/V1-BUNDLE \
  --layer thin-v2=PATH/TO/V2-BUNDLE \
  --layer zccache=PATH/TO/ZCCACHE \
  --layer cook-base=PATH/TO/COOK/BASE \
  --layer cook-delta=PATH/TO/COOK/DELTA \
  --output bench/results/thin-v3/result.json
```

`compressed_bytes` excludes provider framing. Full runs additionally record
real archive/actions-cache bytes, container overhead, and upload/download time.

## Artifact ownership

| Artifact class | v3 owner | Manifest representation / fallback |
|---|---|---|
| `.rlib`, `.rmeta` | shared CAS | hash reference, size, path, mtime; miss rebuilds |
| Proc-macro dylib | shared CAS | host-qualified reference; observed v1 rebuild cascade proves need |
| Native object/static/shared library | CAS when captured; otherwise producing cook/thin layer | reference or conservative generated-output manifest |
| Compiled build-script executable | shared CAS | compiler-output reference |
| Build-script `OUT_DIR` | cook for dependency, thin-v3 for workspace | files/CAS references plus complete manifest |
| `output`, `root-output`, rerun state | cook/thin-v3 | bytes plus original mtime |
| Fingerprint hash/JSON/dep-info | cook/thin-v3 | bytes plus original mtime |
| Compiler `.d` dep-info | cook/thin-v3 | bytes; relocatable paths where supported |
| Final workspace binaries/libraries | none by default | rebuild/relink or explicit release artifact |
| Test/bench/example/rustdoc/clippy output | none by default | compiler pieces may hit CAS |
| Incremental state | none in CI v3 | omit |
| PDB/DWO/dSYM | release artifact or CAS when requested | reference; never duplicate |
| Registry/git source/index | package home | package-manager-owned files |
| Toolchain/sysroot | setup-soldr | versioned install |

## Cook interaction and lifetime

Cook is the long-lived dependency *graph owner*, not another byte store. A cook
base/delta manifest records freshness state and references dependency outputs
in the shared CAS. Publishing it atomically creates leases/pins. GC removes a
blob only after cook manifests and normal zccache retention release it.

Restore order:

1. Restore/import the shared CAS.
2. Resolve cook base then delta and materialize dependency outputs, freshness
   state, generated files, and original mtimes.
3. Resolve thin-v3 project state and materialize references.
4. Run the package manager, which alone decides Fresh/Dirty.
5. zccache serves misses; soldr records whether compilers actually execute.

Cook hit plus CAS miss leaves the output absent and reports the reference miss,
so the unit rebuilds. CAS hit plus cook miss can yield wrapper hits, but remains
a cook miss. A missing thin-v3 may rebuild project code while cooked
dependencies stay Fresh. No layer manufactures a false Fresh result.

## Manifest and miss taxonomy

The versioned manifest includes capability versions, closure completeness,
toolchain/target/profile/features/RUSTFLAGS/environment, package/unit IDs,
expected outputs, inline metadata hashes/sizes/paths/mtimes, CAS references and
pins, plus fallback-walker records.

Every post-restore compile emits one primary reason:

```json
{
  "schema_version": 3,
  "package_id": "registry+...#crate@version",
  "unit": "stable unit identity",
  "expected_outputs": ["debug/deps/libcrate-...rlib"],
  "owner": "cook-base|cook-delta|zccache|thin-v3|none",
  "lookup_key": "digest or manifest key",
  "reason": "zccache_artifact_absent",
  "wrapper_invoked": true,
  "compiler_executed": true
}
```

Reason enum: `cache_key_miss`, `cook_base_miss`, `cook_delta_miss`,
`zccache_artifact_absent`, `target_manifest_absent`, `fingerprint_missing`,
`fingerprint_unreadable`, `primary_output_missing`, `dependency_output_newer`,
`source_input_newer`, `build_script_rerun`, `feature_mismatch`,
`profile_mismatch`, `target_mismatch`, `toolchain_mismatch`,
`rustflags_mismatch`, `environment_mismatch`, `path_relocation_mismatch`,
`archive_corrupt`, `schema_capability_mismatch`,
`intentionally_uncached_workspace_output`, `fallback_walker_used`, and
`materialization_failed`.

setup-soldr aggregates these with Fresh/Dirty counts, wrapper invocations,
actual compiler executions, CAS/cook hits and bytes. “Cache restored but build
was cold” is a failing benchmark state.

## Minimality and numeric budgets

Current baselines are 280.3 MB compressed and 42.1 seconds for a functional
fresh-target thin-v2 restore/build on zccache. Thin-v3 is accepted only with:

- zero false-Fresh/stale-output mutation failures;
- zero external-dependency compiler executions after warm fresh-checkout restore;
- no independently uploaded duplicate compiler blob over 4 KiB;
- thin-v3 metadata/reference archive at most 10 MiB compressed on zccache;
- combined cook+CAS+thin-v3 compressed bytes at least 20% below both current systems;
- warm wall time no slower than the best mode and at most 5% median regression
  on every fixture;
- save/hash/compress cost at most 10% of cold build and amortized by restore two;
- at most two project generations and less than 5% unreferenced growth per source commit;
- equivalent-worktree restores on Linux, Windows, and macOS;
- incomplete output coverage produces an explicit safe fallback.

### Required ablation table

Remove each row independently and rerun the full mutation/warm matrix:

| Removed | Failure proving necessity |
|---|---|
| Primary-output references | missing output schedules unit |
| `.rmeta` reference | metadata-only dependency becomes Dirty |
| Proc-macro reference | host proc-macro/downstream users rebuild |
| Fingerprint hash/JSON | fresh-build fingerprint reason |
| translated dep-info | source freshness cannot be proven |
| output/dependency mtimes | dependency-newer cascade or false-Fresh mutation |
| build-script output/rerun state | rerun or generated input missing |
| `OUT_DIR` files | generated/native correctness failure |
| CAS pin | cook manifest outlives blob under GC |
| fallback walker | incomplete JSON silently under-caches |
| incremental state (negative control) | no correctness loss; transfer improves |

Plot every valid configuration as combined compressed bytes versus
restore-plus-build median. Select the smallest Pareto point meeting all budgets.

## Benchmark matrix

Use three measured repetitions (median/range) on zccache, medium Rust-only,
SQLite/native-C, proc-macro-heavy, and generated-code fixtures. Cover
build/check/test-no-run/clippy/rustdoc, debug/release, host/explicit target,
Linux primary measurements, and Windows/macOS smoke tests.

Scenarios: cold; exact no-op; fresh checkout/empty target/all caches warm;
source edit; lockfile, feature, flags/profile/toolchain, and build-script input
changes; each layer absent; relocation; corrupt/incomplete manifest; repeated
source-only commits.

Retain raw bytes/files/compression/duplicates, archive and network timings,
Fresh/Dirty, wrapper/compiler executions, CAS/cook hits, end-to-end time, and
peak disk.

## Rollout and implementation issues

No production v3 code lands in this research PR:

- [soldr #1611](https://github.com/zackees/soldr/issues/1611): manifest,
  materialization, cook pins, diagnostics, acceptance matrix.
- [zccache #1063](https://github.com/zackees/zccache/issues/1063): stable shared
  CAS references, leases, GC, import/export, materialization API.
- [setup-soldr #418](https://github.com/zackees/setup-soldr/issues/418): restore
  order, v3 keys, negotiation, summary, platform matrix.

After budgets pass, setup-soldr bumps keys to `thin-v3`; old payloads are never
interpreted as v3. Then v1/v2 become aliases with verbose diagnostics. A
distinct rollback setting preserves old implementations through the soak.
