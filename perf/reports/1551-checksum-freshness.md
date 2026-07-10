# #1551 — checksum freshness for touch-without-content-change: evidence report

**Issue:** [zackees/soldr#1551](https://github.com/zackees/soldr/issues/1551)
(sub-issue of #1527) — evaluate Cargo's unstable `-Zchecksum-freshness` as the
content-based upper bound for natural-touch (mtime bumped, bytes unchanged)
builds.

**Verdict: technically confirmed, non-actionable as a soldr feature.**
Checksum freshness collapses touch-only builds exactly to the no-op floor with
no measurable hashing overhead, and it correctly rebuilds on same-size /
same-mtime content mutations. But it is nightly-only (`-Z` is rejected on the
repo-pinned stable 1.94.1), it does **not** cover build-script
`rerun-if-changed` inputs, and mixing flag-on/flag-off invocations against one
target dir causes a ~full rebuild storm in **both** directions (147 of 150
units). Per the issue's own acceptance criteria ("default rollout requires
stable supported Cargo; otherwise document/close as non-actionable") this stays
an experiment users can already opt into today with zero soldr changes — no
soldr hook is added, because a conditional hook would create the mode-mixing
storm hazard rather than remove it.

## Environment

- Container `soldr-perf-local` (image soldr-cook-dev), x86_64-linux, 4 CPUs
  shared with sibling agents (timings are ±10–30 % noisy; structural counts
  are exact).
- Fixture: `perf/fixtures/medium` (~150 compile units / ~200 transitive
  crates), extracted to `/target/issue-1551-fx` (native volume, not the
  Windows bind mount), plus a minimal `build.rs` with
  `cargo:rerun-if-changed=build_input.txt` added so the build-script-input
  axis is exercised.
- Toolchains: stable `cargo 1.94.1 (29ea6fb6a 2026-03-24)` and
  `cargo 1.99.0-nightly (59800466c 2026-07-07)` /
  `rustc 1.99.0-nightly (af3d95584 2026-07-09)`.
- Every build ran with `RUSTC_WRAPPER` set to a counting shim, so
  `rustc_invocations` below is the exact number of wrapper (rustc) spawns —
  the number soldr's zccache wrapper would see. Checksum mode ran with the
  wrapper on every build: **`-Zchecksum-freshness` is wrapper-compatible.**
- Dirty reasons captured with `CARGO_LOG=cargo::core::compiler::fingerprint=info`.
- Harness: `perf/reports/harness-1551.sh` (this directory), run as
  `bash harness-1551.sh /target/issue-1551-fx /repo/perf`.

## Modes

| mode | command | freshness |
|---|---|---|
| `stable-mtime` | `cargo build` (1.94.1) | mtime (today's default) |
| `nightly-mtime` | `rustup run nightly cargo build` | mtime (same-compiler baseline) |
| `nightly-checksum` | `rustup run nightly cargo build -Zchecksum-freshness` | blake3 content checksums |

Each mode used its own cold-started `CARGO_TARGET_DIR`. Compare
touch/edit/no-op *within* a mode; compare `nightly-mtime` vs
`nightly-checksum` for the checksum delta (same compiler). `stable-mtime` cold
is slower than nightly cold for unrelated compiler-version reasons; it is the
"what users have today" reference.

## Results (wall ms, exact rustc-invocation counts)

Scenario reps ran touch/edit → build, 3 timed reps (all reps shown; medians in
bold text below where quoted). `rustc_invocations` was identical across reps
of every scenario.

| scenario | stable-mtime ms | nightly-mtime ms | nightly-checksum ms | rustc invocations (mtime → checksum) |
|---|---|---|---|---|
| cold | 58 787 | 39 946 | 40 165 | 152/150 → 150 |
| no-op ×3 | 600, 225, 228 | 202, 183, 188 | 168, 180, 149 | 0 → 0 |
| touch `src/main.rs` ×3 | 1533, 1577, 1364 | 1025, 973, 942 | **159, 158, 159** | 1 → **0** |
| touch main+build.rs+input ×3 | 1627, 2340, 1733 | 1044, 1117, 1097 | 928, 1025, 1001 | 2 → 1 |
| same-size same-mtime mutation of `src/main.rs` | 212 (**no rebuild**) | 202 (**no rebuild**) | 1083 (**rebuilds**) | 0 → **1** |
| true edit ×3 | 1865, 1772, 1848 | 1236, 1163, 1100 | 1792, 2244, 1777 | 1 → 1 |
| touch `build_input.txt` (rerun-if-changed) ×3 | 1397, 1451, 1467 | 978, 886, 829 | 1378, 1505, 1481 | 1 → 1 (**not collapsed**) |
| same-stat mutation of `build_input.txt` | 208 (no rebuild) | 162 (no rebuild) | 263 (**no rebuild**) | 0 → 0 |

### Headline numbers

- **Touch-only collapses to the no-op floor.** nightly-checksum touch builds:
  ~158 ms / 0 rustc invocations vs nightly-mtime ~973 ms / 1 invocation. On
  this fixture the achievable upper bound is the full leaf-unit recompile+link
  cost (~0.8–1.4 s per touched leaf unit, more for wider dirty cones); the
  touch penalty goes to literally zero units.
- **Hashing overhead is unmeasurable at this scale.** no-op: 149–180 ms
  (checksum) vs 183–206 ms (mtime); cold: 40.2 s vs 39.9 s — both inside
  container noise. The hashing crossover is therefore effectively immediate:
  the per-build checksum cost (<±30 ms here) is repaid by avoiding even a
  fraction of one leaf recompile (~1 s). Only workspaces with very large
  source bytes per checked unit could tip this, and nothing in this data
  approaches it.
- **Correctness probe PASSED (no false-Fresh trap for rustc inputs).** A
  1-byte content mutation with byte-identical size and nanosecond-identical
  mtime (`size=2475 mtime=22:31:28.535926880` before and after — see STAT
  lines in `results.txt`) rebuilt under checksum mode with the exact reason:

  ```text
  dirty: FsStatusOutdated(StaleItem(ChangedChecksum {
      source: ".../src/main.rs",
      stored_checksum: Checksum { algo: Blake3, ... },
      new_checksum:    Checksum { algo: Blake3, ... } }))
  ```

  Under both mtime modes the same mutation was **falsely Fresh** (0
  invocations) — the documented stable-mtime hole; checksum mode fixes it for
  rustc source inputs.

### Exact Dirty sets (from `CARGO_LOG=cargo::core::compiler::fingerprint=info`)

- mtime mode, touch `src/main.rs`:
  `dirty: FsStatusOutdated(StaleItem(ChangedFile { reference_mtime < stale_mtime }))`
  for unit `medium-rust-app` only (dirty set = {leaf bin}).
- checksum mode, touch `src/main.rs`: no dirty entries — dirty set = ∅,
  `Finished ... in 0.11s`.
- checksum mode, same-stat mutation: dirty set = {leaf bin}, reason
  `ChangedChecksum` (blake3 old/new logged).
- **both** modes, touch `build_input.txt`: build-script unit goes
  `FsStatusOutdated(StaleItem(ChangedFile{...}))` (an **mtime** comparison,
  even under `-Zchecksum-freshness`), then the bin unit follows via
  `StaleDepFingerprint`. Dirty set = {RunCustomBuild, leaf bin}.

## Limitations found

1. **Nightly-only.** Stable 1.94.1: `error: the -Z flag is only accepted on
   the nightly channel of Cargo`. There is no stable surface as of
   cargo 1.99.0-nightly (2026-07); the tracking issue is
   rust-lang/cargo#14136.
2. **Build-script `rerun-if-changed` inputs are still mtime-based.** Touching
   `build_input.txt` reruns the build script + recompiles the crate even in
   checksum mode (1 invocation, ~1.4 s), and a same-stat mutation of it stays
   falsely Fresh in checksum mode too. Checksum freshness only covers
   rustc dep-info inputs today. (Same false-Fresh exists on stable mtime mode,
   so this is not a regression — just an un-fixed hole that caps the upper
   bound for build-script-heavy workspaces.)
3. **Mode mixing causes full rebuild storms — both directions.** In a
   checksum-built target dir, one plain `cargo build` (flag dropped) rebuilt
   **147 of 150 units**; re-adding the flag rebuilt **147 units again**. The
   flag changes the fingerprint format, so every invocation against a target
   dir must agree on the mode. Any consumer that runs cargo without the flag —
   rust-analyzer, an IDE save-hook, a bare `cargo check`, CI — storms the
   cache twice.
4. **Env-var opt-in exists and behaves asymmetrically.**
   `CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true` activates the feature on nightly
   (verified: touch-only stayed Fresh, 0 invocations) and is **silently
   ignored on stable** (build proceeds in mtime mode, no error). Convenient,
   but it means a globally-exported env var plus a channel switch silently
   flips modes → limitation 3 fires.

## Why soldr ships no hook (deliverable b intentionally omitted)

- Cargo remains the sole Fresh/Dirty authority (issue acceptance criterion);
  soldr fabricates no mtimes or fingerprint state. Nothing to do there.
- Opt-in already works through soldr **today with zero code changes**, because
  the cargo front door forwards args verbatim
  (`crates/soldr-cli/src/cargo_front_door/mod.rs`, `command.args(args)`) and
  does not scrub `CARGO_UNSTABLE_*` env:
  - `soldr cargo +nightly build -Zchecksum-freshness`
  - `CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true soldr cargo build` (active
    toolchain nightly)
- A soldr-side conditional injection ("add the flag when the toolchain is
  nightly") fails the "clean" bar: every non-soldr cargo invocation against
  the same target dir (rust-analyzer is the common case) would trigger the
  147-unit rebuild storm of limitation 3, and the storm also fires on every
  stable↔nightly channel switch. The hook would convert a 1-unit touch
  penalty into recurring ~full rebuilds for anyone with mixed drivers.
- The wrapper-count evidence (0 invocations on touch) shows soldr/zccache
  cannot improve on this from the `RUSTC_WRAPPER` slot anyway: when cargo is
  Fresh the wrapper is never spawned, and when cargo is Dirty soldr's cache
  already serves the hit. The touch-penalty gap is Cargo's to close, and
  rust-lang/cargo#14136 is the vehicle.

## Recommendation

Close #1551 as **evaluated / non-actionable for default rollout**:

- Upper bound quantified: touch-without-content-change → no-op floor
  (~0.16 s vs ~1.0–1.6 s on `medium`; dirty set ∅ vs {leaf}), hashing
  crossover immediate (no measurable no-op or cold overhead at ~150 units).
- Correctness criterion met by the upstream feature for rustc inputs
  (`ChangedChecksum` on same-stat mutation), NOT met for build-script
  `rerun-if-changed` inputs (still mtime).
- Default rollout blocked on stabilization (rust-lang/cargo#14136). Revisit
  when checksum freshness rides to stable; at that point the right shape is
  still pass-through (cargo-owned), not soldr injection.
- Document the manual opt-in + the mode-mixing caveat (this report) instead of
  shipping a hook.

## Raw data

Harness: `perf/reports/harness-1551.sh`. Full logs (results.txt, per-scenario
`*.fplog` fingerprint traces, wrapper counts) live in the container at
`/target/issue-1551-fx/logs/`; the tables above are a complete transcription
of `results.txt` plus the three follow-up probes:

```text
probe1 envvar-touch    rustc_invocations=0    (CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true, touch-only)
probe2 flag-dropped    rustc_invocations=147  (plain nightly build in checksum target dir)
probe3 flag-restored   rustc_invocations=147  (-Zchecksum-freshness again after probe2)
```
