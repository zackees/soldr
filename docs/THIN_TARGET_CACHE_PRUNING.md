# Thin Target-Cache Fingerprint-Aware Pruning

Status: design proposal. Refs zackees/soldr#237.

This document proposes a **fingerprint-aware** restructuring of the
`target-cache-mode: thin` slice that `setup-soldr` currently saves and restores
through zccache. The motivation is concrete: a CI run on `setup-soldr@v4` was
observed restoring a thin slice into `target/` and then running out of disk
during the build. That means the slice we are dragging through the GitHub
Actions cache is carrying weight that does not actually help cargo skip work.

The goal is to ship a thin cache that is **<300 MB on a typical workspace** and
preserves cargo's "no work to do" verdict on the second build of the same
inputs. Today the slice is multi-GB and includes content that cargo never
consults to make that decision.

> **Implementation note (soldr#1530 / #1546):** Cargo's freshness check also
> requires the primary outputs to exist. A metadata-only slice therefore
> causes Cargo to rebuild every missing unit after restore. The implementation
> now captures Cargo's JSON `compiler-artifact` and `build-script-executed`
> messages, derives the associated fingerprint/build-output closure, and
> retains those verified outputs for thin-v2. If the message stream is
> incomplete or contains an unknown layout, zccache falls back to the
> conservative target walk. Cargo remains the freshness authority; the JSON
> stream only narrows the save candidates.

## 1. What the thin slice actually contains today

The set of files saved by `target-cache-mode: thin` is determined in two
places:

1. **soldr-cli** generates a `RustArtifactPlan` and hands it to
   `zccache rust-plan {restore,save}`. The allowlist is the
   `allowed_artifact_classes` returned by
   `crates/soldr-cli/src/main.rs::allowed_artifact_classes`:

   ```rust
   // crates/soldr-cli/src/main.rs ~line 859
   fn allowed_artifact_classes(mode: &str) -> Vec<&'static str> {
       if mode == "full" {
           return Vec::new();
       }
       vec![
           "rlib",
           "rmeta",
           "dep_info",
           "proc_macro",
           "cargo_fingerprint",
           "build_script_metadata",
           "build_script_output",
       ]
   }
   ```

2. **`.github/actions/setup-soldr/resolve_setup.py`** points the GitHub Actions
   `actions/cache@v4` step at the bundle directory zccache writes to:

   ```python
   target_cache_bundle_path = cache_root.parent / f"{cache_root.name}-target-thin"
   ...
   target_cache_paths = str(target_cache_bundle_path)  # thin mode
   ```

   The cache key is
   `setup-soldr-targetcache-thin-v1-{os}-{arch}-{target-inputs-hash}-{sha}`
   with a restore-key that drops the SHA so warm restores can match across
   commits whose `Cargo.lock`, manifests, target shape, and toolchain digest
   are identical.

The resulting on-disk inventory (paths are relative to the workspace
`target/`), as restored back into the live `target/` tree, is roughly:

| Subtree | Origin | Size class |
|---|---|---|
| `<profile>/.fingerprint/<crate>-<hash>/{invoked.timestamp,*.json,dep-*}` | `cargo_fingerprint` | small |
| `<profile>/deps/lib<crate>-<hash>.rlib` and `.rmeta` | `rlib`, `rmeta` | **large** |
| `<profile>/deps/<crate>-<hash>.d` | `dep_info` | small |
| `<profile>/deps/<crate>-<hash>.so / .dll / .dylib` (proc-macros) | `proc_macro` | medium |
| `<profile>/build/<crate>-<hash>/{out, output, root-output, stderr, invoked.timestamp}` | `build_script_metadata`, `build_script_output` | usually small |

What is **not** captured by the explicit allowlist (and therefore should
already be excluded by the zccache filter when it walks `target/`):

- `<profile>/incremental/` — the rustc incremental DB. Multi-GB on big
  workspaces, churns per-commit, low CI hit rate.
- `<profile>/build/<crate>-<hash>/build-script-build*` — the compiled build
  script binary (the one cargo runs to produce `out/`). Cheap to regenerate
  from cached deps.
- `<profile>/deps/<crate>-<hash>.d` companion `.dwo` files (split
  debug-info), `.pdb`, `.dSYM/` bundles.
- Final test/bench harness binaries under `<profile>/deps/` that are not
  needed to satisfy `cargo build`.

What we have **observed in practice** (the trigger for this issue):

- The slice still ships `.rlib` files containing embedded DWARF for every
  workspace-external dependency. On a workspace with a few hundred deps in
  `[profile.dev]` this is the dominant contributor (>80% of slice size).
- `proc_macro` `.so`/`.dll` artifacts contain debug-info too.
- `build_script_output` includes raw stderr captures from build scripts which
  for crates like `ring`, `bindgen`, `aws-lc-sys` can each be tens of MB of
  inline assembler output.

The empirical claim "multi-GB" maps directly to `rlib` debug-info plus
incremental leakage that should not be in the slice but historically has been
because the bundle directory is written by a streaming zccache walker that
predates the explicit allowlist.

## 2. What cargo actually needs to skip work

Cargo's freshness check, run before each unit's compilation, is implemented in
`cargo::core::compiler::fingerprint`. The decision to skip a unit is a
function of:

1. **`<profile>/.fingerprint/<crate>-<hash>/invoked.timestamp`** — a zero-byte
   file whose mtime is the "this unit completed" sentinel.
2. **`<profile>/.fingerprint/<crate>-<hash>/dep-<target>`** — the precomputed
   hash of the unit's inputs (rustc args, env, features, dep graph).
3. **`<profile>/.fingerprint/<crate>-<hash>/output-<target>`** — the dep-info
   `.d` file path list, used to mtime-compare every input source.
4. **The dep-info `.d` file** under `<profile>/deps/<crate>-<hash>.d` — the
   source-file list cargo stats to decide if anything got newer.
5. **Existence** of the unit's primary outputs on disk (`*.rlib`, `*.rmeta`,
   `*.d`, build-script `out_dir/`).

Cargo does *not* read the bytes of the `.rlib`. It only checks that the file
exists and that no input is newer than `invoked.timestamp`. If the file is
missing, cargo schedules a rebuild — and **that is exactly the case where
zccache should serve the artifact from its content-addressed store**.

This is the load-bearing observation behind the proposal: we can drop the
heavy `.rlib`/`.rmeta` bytes from the GHA-cached thin slice, as long as we
keep `.fingerprint/`, `.d`, and the small build-script outputs, **and**
zccache's compilation cache is warm enough to serve the resulting rebuild
requests as zero-work hits.

## 3. Proposed thin-slice contents

### 3.1 Keep (required for cargo to consider work done)

```
target/<profile>/.fingerprint/**/{invoked.timestamp,output-*,dep-*,lib-*,bin-*}
target/<profile>/deps/*.d                       (dep-info)
target/<profile>/build/*/out/**                 (build-script generated sources)
target/<profile>/build/*/output                 (cargo:* directives)
target/<profile>/build/*/root-output            (build-script env passthrough)
target/<profile>/build/*/stderr                 (only if <128KB, else drop)
target/<profile>/build/*/invoked.timestamp
target/CACHEDIR.TAG
```

Plus a small **pin manifest** generated by soldr at save time:

```
target/.soldr-thin-manifest.json
```

containing the package-id -> expected-output-path map for every workspace
package and external dep that the build touched. The manifest is what lets a
verifier (Section 5) detect "cargo wants to build X but our manifest claimed X
was cached".

### 3.2 Drop (zccache repopulates on demand from its content store)

```
target/<profile>/incremental/                   incremental DB
target/<profile>/deps/*.rlib                    library output bytes
target/<profile>/deps/*.rmeta                   metadata bytes
target/<profile>/deps/*.{so,dll,dylib}          proc-macro bytes
target/<profile>/deps/*.{dwo,pdb}               split debug-info / pdb
target/<profile>/deps/*.dSYM/                   macOS debug bundles
target/<profile>/build/*/build-script-build*    compiled build-script binary
target/<profile>/{examples,doc,tests}/          always — test/doc/example
                                                products are never cacheable
                                                (soldr#2931, no opt-in exists)
target/<profile>/<bin>                          final binary, unless opted in
```

The dropped categories are exactly the ones that either (a) cargo never reads
to decide skip-vs-rebuild, or (b) zccache has byte-for-byte in its store
under a key derivable from the corresponding fingerprint we are still
shipping.

### 3.3 Concrete size targets

For the soldr workspace itself (representative dev profile, all features), a
prototype walker over a freshly-built `target/debug/` produces:

- Current thin slice (today, after streaming dump): **~1.3 GB** observed.
- Proposed thin slice: **~120 MB** estimated, dominated by `.fingerprint/`
  plus large bindgen `out_dir`s. Within the <300 MB target.
- Full slice (`target-cache-mode: full`): **~3.8 GB** observed. Out of scope
  for this PR.

Real workspaces will skew larger; the design holds because the
`.rlib`/incremental bulk grows linearly while fingerprints+dep-info grow
sub-linearly with crate count.

## 4. Algorithm

The selection logic lives in **soldr-cli** alongside the existing rust-plan
generator, because soldr already owns `cargo metadata` and the
`RustArtifactPlan` schema. zccache stays the I/O engine: soldr emits a
**file-list manifest**, zccache packs/unpacks against that manifest.

### 4.1 Save path (pseudocode)

```
fn build_thin_save_manifest(meta: CargoMetadata, profile: &str) -> ThinManifest {
    let target_dir = meta.target_directory;
    let prof_dir   = target_dir.join(profile);
    let fp_root    = prof_dir.join(".fingerprint");
    let deps_root  = prof_dir.join("deps");
    let build_root = prof_dir.join("build");

    let mut keep = Vec::new();

    // 1. All fingerprint metadata for units that cargo built this run.
    //    Cargo writes one directory per unit; selecting all of them is fine
    //    because the directories themselves are tiny.
    for unit_dir in read_dir(&fp_root)? {
        for entry in read_dir(&unit_dir)? {
            match entry.file_name().to_str() {
                Some(n) if n == "invoked.timestamp"
                       || n.starts_with("output-")
                       || n.starts_with("dep-")
                       || n.starts_with("lib-")
                       || n.starts_with("bin-") => keep.push(entry.path()),
                _ => {}
            }
        }
    }

    // 2. Dep-info .d files only. Drop .rlib/.rmeta/.so/.dll/.dylib siblings.
    for entry in read_dir(&deps_root)? {
        if entry.path().extension() == Some("d") {
            keep.push(entry.path());
        }
    }

    // 3. Build-script out_dir contents and small metadata files.
    for unit_dir in read_dir(&build_root)? {
        let out = unit_dir.join("out");
        if out.is_dir() {
            keep.extend(walk_dir(&out));
        }
        for small in ["output", "root-output", "invoked.timestamp"] {
            let p = unit_dir.join(small);
            if p.is_file() { keep.push(p); }
        }
        // stderr is kept only if small; large bindgen stderr is logspam.
        let stderr = unit_dir.join("stderr");
        if stderr.is_file() && metadata(&stderr)?.len() < 128 * 1024 {
            keep.push(stderr);
        }
        // Drop: build-script-build* binaries.
    }

    // 4. CACHEDIR.TAG marker (cargo writes it; restore it so we don't tag scans).
    let cd = target_dir.join("CACHEDIR.TAG");
    if cd.is_file() { keep.push(cd); }

    ThinManifest { files: keep, generated_at: now(), schema_version: 2 }
}
```

The manifest is serialized to `target/.soldr-thin-manifest.json` and the path
list is what zccache's `rust-plan save` then packs into the bundle directory
the GHA cache step uploads.

### 4.2 Restore path

Restore is symmetric. zccache unpacks the bundle into `target/`, then soldr
reads the manifest and:

1. Verifies every claimed file actually landed.
2. For each `.fingerprint/<crate>-<hash>/output-<target>` it sees, looks up
   the corresponding expected `.rlib`/`.rmeta` in `deps/`. If missing, it
   does **nothing** — cargo will discover the absence on its own and ask
   rustc to rebuild, at which point the `RUSTC_WRAPPER` (zccache) intercepts
   and serves the artifact from the compilation cache.
3. Logs counts: `kept_fingerprints`, `kept_dep_info`, `kept_build_out_dirs`,
   plus an estimated bytes-saved-vs-full.

### 4.3 Schema and code touchpoints

- New variant in `RustArtifactPlan.allowed_artifact_classes`: split
  `cargo_fingerprint` into `cargo_fingerprint_meta` (kept) and
  `cargo_fingerprint_outputs` (dropped under the new mode).
- Bump `RustArtifactPlan.cache_schema_version` to **2**. zccache must reject
  plans with `cache_schema_version >= 2` if its own thin walker does not
  understand the manifest, and fall back to the old behavior with a printed
  warning.
- Bump `RustArtifactPlan.schema_version` independently if the cargo-side
  shape changes; the two version numbers are intentionally separable.
- New env var `SOLDR_TARGET_CACHE_PROFILE` with values `thin-v1` (legacy
  opt-out) and `thin-v2` (current default). The default flipped in soldr
  v0.7.31 alongside the bump to managed zccache 1.9.1, which honors the
  `cache_profile` / `dropped_artifact_classes` wire fields. Operators
  pinned to older zccache (< 1.9.1) must set
  `SOLDR_TARGET_CACHE_PROFILE=thin-v1` until they upgrade.

## 5. Verification: build twice in one CI job

The danger of an aggressive thin slice is that cargo gets a partial restore,
*thinks* the build is up to date, but is silently wrong — or, worse, decides
the slice is invalid and rebuilds everything. We need an in-CI assertion
that the second build of the same inputs is a no-op.

### 5.1 Proposed verification job

Add a workflow `.github/workflows/cache-thin-verify.yml`:

```yaml
- uses: actions/checkout@v4
- uses: ./   # local setup-soldr
  with:
    target-cache: true
    target-cache-mode: thin

- name: First build (warm slice)
  run: soldr cargo build --workspace

- name: Second build (must be a no-op)
  env:
    CARGO_LOG: cargo::core::compiler::fingerprint=info
  run: |
    soldr cargo build --workspace --timings=json -Z unstable-options \
      > timings.json
    python .github/scripts/assert_thin_noop.py timings.json
```

`assert_thin_noop.py` checks the cargo `--timings=json` stream and **fails**
if any unit reports `mode != "fresh"`. Acceptable allowlist: workspace bins
that have no fingerprint (cargo always re-links). The script also greps the
captured stderr for `fingerprint dirty for` and prints the offending crate
plus reason — mirroring the diagnostic guidance already in
[`docs/CI_CACHE.md`](CI_CACHE.md#debugging-target-cache-restores-that-still-rebuild).

### 5.1.a Verifier script (`assert_thin_noop.py`)

The verifier ships at `.github/scripts/assert_thin_noop.py`. It reads two
captured `cargo build` logs (cold, then warm) and fails if the second build
recompiled any first-party (workspace / path-dep) crate, or if it
recompiled more than `--tolerance` third-party crates (default 2, to allow
trivial proc-macro re-runs).

Run it locally to spot-check a `thin-v2` change without spinning up CI:

```bash
# Build soldr-cli so SOLDR_TARGET_CACHE_PROFILE=thin-v2 is honored.
soldr cargo build -p soldr-cli

# Use any small workspace; a fresh `soldr cargo init` works.
mkdir -p /tmp/verify-noop && cd /tmp/verify-noop
soldr cargo init --name verify-noop --bin verify-noop >/dev/null
soldr cargo add serde --no-default-features --features derive >/dev/null
export SOLDR_TARGET_CACHE_MODE=thin
export SOLDR_TARGET_CACHE_PROFILE=thin-v2
export SOLDR_TARGET_CACHE_BACKEND=local
export SOLDR_TARGET_CACHE_BUNDLE_DIR=/tmp/verify-noop-thin-v2-bundle
export SOLDR_TRUST_INHERITED_ENV=1
rm -rf "$SOLDR_TARGET_CACHE_BUNDLE_DIR"
mkdir -p "$SOLDR_TARGET_CACHE_BUNDLE_DIR"

# Capture both passes. (If you have a real warm thin-v2 slice from a
# previous CI run, drop it into target/ between the two builds.)
set -o pipefail
soldr cargo build -v 2>&1 | tee first.log
soldr cargo build -v 2>&1 | tee second.log

uv run --no-project python /path/to/soldr/.github/scripts/assert_thin_noop.py \
  first.log second.log --allow-empty-second
```

`--allow-empty-second` is only appropriate after the second build command has
already succeeded. A completely fresh no-op can emit no captured lines through
soldr's non-interactive diagnostic path, so the command exit code is the
success proof and the empty log means "no compile lines observed."

Exit code semantics:

- `0` — second build is a no-op within tolerance. Slice is sufficient.
- `1` — second build did real work; the slice is missing fingerprints.
- `2` — input log not found (operator error).

The CI gate that runs this script lives at
`.github/workflows/thin-v2-verify.yml`. It is currently
`continue-on-error: true` (informational) while we shake out runner-specific
quirks; flip it to a hard required check once it has been green for a week
on `main`.

#### 5.1.b Manifest assertion (`assert_thin_manifest.py`)

The cargo-output verifier above only proves that cargo was happy with the
restored slice. It does **not** prove that soldr-cli actually wrote
`manifest.v2.json` next to the bundle, that the manifest enumerates the
files present, or that it never re-lists artifact classes thin-v2 is
supposed to drop. A second script,
`.github/scripts/assert_thin_manifest.py`, closes that gap:

```bash
python .github/scripts/assert_thin_manifest.py \
  <bundle_dir>/manifest.v2.json \
  <bundle_dir> \
  [--strict]
```

What it checks:

- `manifest.v2.json` exists, parses as JSON, and matches the
  `ThinSliceManifest` schema emitted by
  `crates/soldr-cli/src/main.rs::write_thin_manifest` (`schema_version: 2`,
  `cache_profile`, `bundle_root`, `generated_at_unix_seconds`, `files[]`).
- Every entry in `files[]` exists as a regular file under `bundle_dir`
  (drift detection).
- `--strict` additionally fails if any file under `bundle_dir` is missing
  from the manifest (orphan detection). Off by default because some bundle
  writers may legitimately leave scratch state behind.
- No path in the manifest matches the dropped-category patterns:
  `*/incremental/*`, `*.rlib`, `*.rmeta`, `*.dwo`, `*.pdb`, `*.dSYM/*`,
  `*/build-script-build`, `*/build-script-build.exe`. A regression that
  starts re-listing any of these classes hard-fails the gate.

Exit codes mirror `assert_thin_noop.py`: `0` ok / `1` validation failure
(human-readable reason printed to stderr) / `2` usage error.

The CI workflow runs this script unconditionally after the synthetic fixture
is built through `soldr cargo build` with `SOLDR_TARGET_CACHE_BUNDLE_DIR`
pinned to a deterministic temp directory. If the save path stops producing
`manifest.v2.json`, the verifier fails instead of silently skipping the
manifest assertion.

### 5.2 Counter-tests

| Counter-test | What it catches |
|---|---|
| Mutate one source file before second build, expect *exactly one* unit to rebuild | Over-pruning that breaks freshness |
| Rotate `RUSTFLAGS` between builds, expect full rebuild | Manifest reuse across distinct inputs |
| Run with `SOLDR_TARGET_CACHE_PROFILE=thin-v1` and assert second build is no-op | Regression guard for the legacy slice |
| `du -sb target-thin.tar` after save, fail if > 600 MB on the soldr workspace | Slice-size regression guard |
| Restore on a host with `RUSTC_WRAPPER` unset, then build, assert the build still completes (slow but correct) | Confirms thin-v2 is not silently dependent on a warm zccache |

The last test is the most important. It enforces the design contract: the
thin slice is a **freshness oracle**, not a build artifact store. zccache is
the artifact store, and the two layers must compose without false positives
in either direction.

## 6. Rollout plan

The change spans soldr-cli, zccache (out-of-repo), and the setup-soldr action.
Sequencing matters because the GitHub Action and the soldr binary are
versioned independently.

1. **PR 1 — landed**: design doc only. No behavior change.
2. **PR 2 — landed**: soldr-cli `thin-v2` plan generator behind
   `SOLDR_TARGET_CACHE_PROFILE=thin-v2` (off by default at the time).
   Manifest schema bump and the `allowed_artifact_classes` split shipped.
   Ground truth lives in `crates/soldr-cli/src/rust_plan.rs`.
3. **PR 3 — landed (zccache)**: zccache 1.9.1 accepts `cache_profile`,
   `dropped_artifact_classes`, and `cache_schema_version: 2` in
   `RustArtifactPlanV1`; the save walker consults the drop list and the
   new `CargoFingerprintMeta` / `CargoFingerprintOutputs` split.
4. **PR 4 — landed**: `thin-v2-verify.yml` plus the `assert_thin_noop.py`
   and `assert_thin_manifest.py` scripts. Currently
   `continue-on-error: true` while we wire the workflow through
   `soldr cargo build` end-to-end.
5. **PR 5 — landed (this PR, soldr#461)**: bump
   `MANAGED_ZCCACHE_VERSION` to 1.9.1, flip the default of
   `SOLDR_TARGET_CACHE_PROFILE` to `thin-v2`, and bump the setup-soldr
   `target-cache-bundle` cache-key version from `thin-v1` to `thin-v2`
   inside `resolve_setup.py` so stale heavy bundles are invalidated.
6. **PR 6 — followup**: re-export the action with the new defaults under
   `setup-soldr@v4`. `@v0` inherits the bump transparently because the
   exporter copies `resolve_setup.py` verbatim.

Rollback: revert PR 5. Old caches are gone but `thin-v1` regenerates them on
the next push. No rust-plan or zccache schema is destabilized because the
schema-version field protects forward and backward compatibility.

## 7. Risks

- **Risk: cargo decides a `.rlib` we dropped is "missing" and rebuilds, but
  zccache misses too** (e.g. zccache cache root was evicted between runs).
  Result: warm-looking restore, cold-feeling build. *Mitigation*: the
  verification job catches this. Log a clear "thin-restore HIT but
  zccache-rebuild MISS for N units" line so it is debuggable from a single
  CI log.
- **Risk: workspace bins that depend on linker reruns**. `.rlib` for a
  workspace lib is not in the thin slice, but the corresponding workspace bin
  is rebuilt every run. *Mitigation*: this is already true today for any
  workspace bin with a `[[bin]]` target; the thin slice was never sized for
  them. Document explicitly.
- **Risk: build-script `out_dir` files reference absolute paths to
  `target/`**. If we restore `out/` but the rest of `target/` is repathed,
  cargo's path-rewriting logic may flag dirty. *Mitigation*: keep the
  `target_dir` portion of the cache key shape so the directory layout is
  identical run-to-run; this is already the case via `target_shape_hash` in
  `resolve_setup.py`.
- **Risk: macOS `.dSYM/` bundles being dropped breaks `lldb`** for
  developers running soldr locally. *Mitigation*: thin slice is CI-only.
  Local builds do not go through `target-cache-mode`. Document.
- **Risk: schema-version skew between soldr-cli and zccache produces opaque
  errors**. *Mitigation*: zccache must emit a single-line, JSON-shaped
  warning when it sees an unrecognized `cache_schema_version` and
  `soldr-cli::warn_if_rust_plan_restore_incomplete` already grows a
  `schema_version_skew` branch.
- **Risk: aggressive prune accidentally breaks `cargo doc`**. `cargo doc`
  reads `.rmeta`. *Mitigation*: explicitly exclude `cargo doc` from
  `cargo_args_are_cacheable` for `thin-v2` (or refuse to drop `.rmeta` when
  the doc subcommand was invoked). Add a unit test next to the existing
  `cargo_args_are_cacheable` tests.

## 8. Open questions (to resolve in PR 2 review)

- Do we keep `.rmeta` always (cheap, small, used by `cargo check`) and only
  prune `.rlib`? Estimated win drops from ~90% to ~60%, but `cargo check`
  workflows become trivially correct. Default proposal: drop both, rely on
  zccache. Reconsider if `cargo check` -only CI is a primary user.
- Manifest format: JSON for diagability, or msgpack for size? JSON is
  ~3x larger but the manifest itself is <1 MB so it does not matter.
  Default: JSON.
- Should the manifest be content-hashed and the hash baked into the GHA
  cache key? It would let us detect "same inputs, different prune policy"
  without bumping `thin-vN`. Default: no, the `thin-vN` env var is simpler
  and matches how the slice is actually consumed.

## 9. Summary

Today's thin slice ships rustc output bytes that cargo never reads to decide
freshness. The proposed `thin-v2` slice ships only the freshness inputs
themselves (fingerprints, dep-info, build-script `out_dir`s) and lets
zccache repopulate the bytes on demand. Verified by an in-CI two-build
no-op assertion. Estimated drop from ~1.3 GB to <300 MB on the soldr
workspace, with no loss in cargo's "no work to do" rate.
