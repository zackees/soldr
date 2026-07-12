# Thin-v3 unified Rust cache research and architecture decision

Status: **normative architecture policy selected by issue #1609 research**.
This is not yet a production-profile switch. Thin-v1 and thin-v2 remain
unchanged until the implementation issues pass the acceptance matrix and soak
gate.

## Decision

Thin-v3 uses **lifetime-partitioned ownership**. Long-lived cook state owns
external dependency artifacts. Short-lived zccache state owns project-closure
compiler artifacts. Thin-v3 owns project freshness metadata and references,
not another copy of either class.

| Concern | Authoritative owner | Durable representation |
|---|---|---|
| Fresh/Dirty decision | Rust package manager | fingerprints, dep-info, output existence, mtimes |
| External dependency bytes and freshness | soldr cook | self-contained long-lived base/delta artifacts |
| Workspace and local-path compiler bytes | zccache | short-lived content-addressed project cache |
| Checkout-local project freshness | thin-v3 | metadata, build-script state, owner-qualified references |
| Registry/git packages | package home | package-manager-owned cache |
| Toolchain/sysroot | setup-soldr | versioned toolchain cache |

A compiler output may be materialized into `target/` by reflink or copy, but
the uploaded byte sequence has one durable owner. A complete cook hit never
depends on a separately evictable zccache blob. Thin-v3 retains the owner,
hash, size, target-relative path, class, unit identity, and mtime, but never an
independent copy of a blob larger than 4 KiB.

This resolves the v1/v2 dead end: deleting `.rlib`/`.rmeta` makes a unit Dirty
because output existence is load-bearing, while archiving them in every layer
restores Fresh at the cost of duplicating the dominant bytes.

## Three strategies considered

The decision uses six criteria. Correctness is a gate; the weighted score
selects among designs that can be correct. Scores are design evidence from 1
(poor) to 5 (strong), not a substitute for the benchmark acceptance matrix.

| Criterion | Weight | A: universal shared CAS | B: lifetime-partitioned owners | C: one combined archive |
|---|---:|---:|---:|---:|
| Freshness correctness | 25 | 5 | 5 | 5 |
| No duplicate durable bytes | 20 | 5 | 5 | 5 |
| Cache-lifetime alignment | 20 | 2 | 5 | 1 |
| GHA transfer/update efficiency | 15 | 2 | 4 | 1 |
| Failure isolation | 10 | 2 | 5 | 2 |
| Implementation simplicity | 10 | 1 | 3 | 4 |
| **Weighted total / 100** | | **65** | **93** | **64** |

### Strategy A: one universal shared CAS

Cook and thin-v3 would contain references to one zccache-owned blob pool.
Local deduplication is excellent, but lifetime and remote transport are wrong:

- cook is intended to remain useful longer than a project compilation cache;
- an in-process pin cannot prevent GitHub from evicting the remote cache that
  contains the referenced blob;
- GitHub cache entries are immutable, so adding blobs requires a new key and
  another snapshot rather than updating the old pool;
- losing one pool simultaneously degrades cook, thin-v3, and wrapper hits;
- GC and capability negotiation become cross-repository correctness concerns.

This strategy is rejected as the default. A shared local CAS may still be an
implementation detail within one runner, but no durable manifest may require a
blob whose remote lifetime is shorter than the manifest.

The remote constraint is explicit in GitHub's
[dependency-caching reference](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching):
an existing cache's contents cannot be changed; producers must create a new
entry with a new key. GitHub also evicts inactive entries independently of any
soldr/zccache lease.

### Strategy B: lifetime-partitioned ownership — selected

Ownership follows invalidation and retention boundaries:

- **cook owns external registry/git dependencies**, including their compiled
  libraries, proc macros, build-script executables and outputs, native outputs,
  fingerprints, dep-info, and required mtimes;
- **zccache owns workspace members and local/path project closure compiler
  outputs**, which change with source commits and use a shorter-lived key;
- **thin-v3 owns project freshness metadata and owner-qualified references**
  needed to materialize zccache-owned project outputs;
- package source identity, not host/target role, decides ownership. An external
  proc macro is cook-owned; a workspace proc macro is zccache-owned;
- each durable layer may internally content-address and deduplicate its own
  bytes, but references never cross from a longer-lived layer to a shorter one.

This preserves a useful cook cache even after project caches are evicted,
keeps source-sensitive project churn out of the cook archive, and avoids
re-uploading a universal blob snapshot for every new source revision.

### Strategy C: one combined cook/zccache/thin archive

One archive can deduplicate internally and has simple restore ordering. It is
rejected because any source-only change invalidates or re-uploads dependency
bytes, every consumer pays the largest restore, independent layer fallback is
lost, and one corruption/eviction removes all warm paths. GitHub's immutable
cache keys make this especially expensive.

## Normative ownership policy

The words **MUST**, **MUST NOT**, and **SHOULD** below are policy:

1. Every persisted compiled artifact MUST have exactly one durable owner in
   the active ownership mode.
2. With complete cook coverage, external registry/git package artifacts and
   freshness state MUST be cook-owned and MUST NOT be included in durable
   zccache or thin-v3 uploads.
3. Workspace members and local/path project closure compiler outputs MUST be
   zccache-owned. Their freshness metadata MUST be thin-v3-owned. Final release
   deliverables remain explicit workflow artifacts, not cache payloads.
4. Thin-v3 MUST NOT upload compiled output bytes larger than 4 KiB. It stores
   owner-qualified references for zccache-owned project outputs.
5. Runtime zccache MAY temporarily cache dependency compilations locally, but
   a partitioned durable export MUST exclude cook-owned entries.
6. A dependency compiled after a cook miss MUST be admitted to a cook delta or
   reported as uncached. It MUST NOT silently become a second durable zccache
   owner while the matching cook lineage is active.
7. If cook is disabled or its closure is incomplete, the cache key MUST select
   `zccache-all-v1`: zccache owns compiled dependency and project outputs, and
   thin-v3 owns the required freshness metadata. This fallback MUST NOT share
   payloads or keys with `cook-partitioned-v1`.
8. A manifest MUST be self-sufficient for its declared lifetime. A long-lived
   cook artifact MUST NOT reference a separately evictable project-cache blob.
9. Data archives MAY restore in parallel, but materialization order MUST be
   cook dependency state, zccache project outputs, then thin-v3 project
   freshness metadata, followed by the package manager's authoritative check.
10. Missing owner state MUST degrade to an explicit cache miss and rebuild;
    no layer may synthesize Fresh or conceal an absent output.
11. If content hashing finds the same compiled blob in both partitions, the
    longer-lived cook owner wins. The shorter-lived project manifest MAY
    reference that cook-owned digest; the reverse direction is forbidden.
12. Within one ownership lineage, an unchanged digest larger than 4 KiB MUST
    NOT be independently uploaded again in base, delta, or later generations.
    Multiple manifests may reference one uploaded copy.
13. Because GHA entries are immutable, evolving owners MUST use a stable base
    plus true content deltas or content-addressed segments with a bounded
    restore chain. A source-only change MUST NOT resnapshot unchanged external
    dependencies or the complete project store.

Schema and setup keys MUST include the ownership policy identifier
`thin-v3-lifetime-partition-v1` plus the mode (`cook-partitioned-v1` or
`zccache-all-v1`). An implementation that cannot prove package ownership or
closure completeness MUST use the conservative fallback mode.

The same contract is machine-readable in
[`thin_v3_policy.v1.json`](thin_v3_policy.v1.json). Implementations and setup
key tests SHOULD consume or mirror that file and fail on an unknown policy ID.

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
| External `.rlib`, `.rmeta` | cook | self-contained cook bytes, size, path, mtime |
| Workspace/path `.rlib`, `.rmeta` | zccache | thin-v3 owner-qualified reference; miss rebuilds |
| Proc-macro dylib | owner selected by package source | external in cook; project in zccache |
| Native object/static/shared library | owner selected by producing package | external in cook; project ingested by zccache or explicitly rebuilt |
| Compiled build-script executable | owner selected by package source | external in cook; project reference to zccache |
| Build-script `OUT_DIR` | owner selected by package source | files/internal references plus complete manifest |
| `output`, `root-output`, rerun state | cook for external, thin-v3 for project | bytes plus original mtime |
| Fingerprint hash/JSON/dep-info | cook for external, thin-v3 for project | bytes plus original mtime |
| Compiler `.d` dep-info | cook for external, thin-v3 for project | bytes; relocatable paths where supported |
| Final workspace binaries/libraries | none by default | rebuild/relink or explicit release artifact |
| Test/bench/example/rustdoc/clippy output | none by default | compiler pieces may hit CAS |
| Incremental state | none in CI v3 | omit |
| PDB/DWO/dSYM | release artifact or package-selected owner | never duplicate across owners |
| Registry/git source/index | package home | package-manager-owned files |
| Toolchain/sysroot | setup-soldr | versioned install |

## Cook interaction and lifetime

Cook is the long-lived external-dependency graph and byte owner. Its base is
self-contained; deltas may reference that base but cannot duplicate unchanged
content. Cook may use an internal per-file CAS or content-addressed segments to
avoid duplicate uploads between generations, but it does not depend on the
project zccache lineage.

Restore order:

1. Restore cook, zccache, and thin-v3 archives in parallel into disjoint paths.
2. Resolve cook base then delta and materialize external dependency outputs,
   freshness state, generated files, and original mtimes.
3. Materialize zccache-owned workspace/path outputs.
4. Materialize thin-v3 project freshness metadata and mtimes.
5. Run the package manager, which alone decides Fresh/Dirty.
6. zccache serves misses; soldr records whether compilers actually execute.

A complete cook hit remains useful even if all project zccache/thin-v3 state is
gone. A cook miss rebuilds the affected dependency and updates cook delta when
that lineage is writable. A zccache miss rebuilds project code without
invalidating cooked dependencies. A missing thin-v3 may reschedule project
units while cooked dependencies stay Fresh. No layer manufactures a false
Fresh result.

## Manifest and miss taxonomy

The versioned manifest includes capability versions, closure completeness,
toolchain/target/profile/features/RUSTFLAGS/environment, package/unit IDs,
expected outputs, inline metadata hashes/sizes/paths/mtimes, owner-qualified
references, ownership mode, plus fallback-walker records.

Every post-restore compile emits one primary reason:

```json
{
  "schema_version": 3,
  "package_id": "registry+...#crate@version",
  "unit": "stable unit identity",
  "expected_outputs": ["debug/deps/libcrate-...rlib"],
  "ownership_mode": "cook-partitioned-v1",
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
`materialization_failed`. Ownership classification failures use
`ownership_unknown` and force `zccache-all-v1` rather than guessing.

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
- combined cook+zccache+thin-v3 compressed bytes at least 20% below both current systems;
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
| ownership filter/mode key | duplicate upload or cross-mode cache collision |
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

- [soldr #1611](https://github.com/zackees/soldr/issues/1611): ownership
  classifier, partitioned manifests/materialization, cook delta, diagnostics,
  acceptance matrix.
- [zccache #1063](https://github.com/zackees/zccache/issues/1063): durable
  export filtering, project-output references, import/export, materialization
  API.
- [setup-soldr #418](https://github.com/zackees/setup-soldr/issues/418): restore
  order, v3 keys, negotiation, summary, platform matrix.

After budgets pass, setup-soldr bumps keys to `thin-v3`; old payloads are never
interpreted as v3. Then v1/v2 become aliases with verbose diagnostics. A
distinct rollback setting preserves old implementations through the soak.
