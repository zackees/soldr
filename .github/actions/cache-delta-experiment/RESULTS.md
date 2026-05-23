# cache-delta-experiment — first-pass findings

Run-by-run iteration on the layered cook/delta cache prototype (see
`action.yml` for the design and `cache-delta-experiment-cleanup/` for
the save half). Numbers below come from the `Side-by-side comparison`
job's step-summary on `soldr cook --tests` + `soldr cargo build
--package soldr-cli --locked` against a single-`actions/cache@v4`
baseline running the same commands.

## Measured

| run | cook upload | delta upload | total | baseline | ratio |
|---|---|---|---|---|---|
| **cold** (cache empty) | 178 MB | 79 MB | 257 MB | 178 MB | **1.45×** |
| **warm** (Cargo.lock + rust-toolchain.toml unchanged) | 0 MB | 79 MB | 79 MB | 178 MB | **0.45×** |

Warm numbers are stable across three independent runs (79.5 MB ±
0.05 MB per run, baseline 178.5 MB ± 0.1 MB).

## What works

- **Layered restore + save** end-to-end. Both `actions/cache` entries
  (cook + delta) survive the round-trip; the composite action correctly
  applies cook then delta to the same on-disk path; the cleanup action
  saves cook only when the cook-key missed.
- **Upload-bytes savings on warm runs**: 55% reduction (79 MB vs.
  178 MB). The cook layer is correctly skipped (`cache-hit: true`)
  and only the per-PR delta is uploaded.
- **Cold-run overhead** is expected and bounded (~45% over baseline) —
  cold has to populate both layers separately.

## What does NOT work yet

**zccache cache hit rate stays at 0% on warm runs.**

The session stats from `soldr cache report --json` after a warm-cache
build:

```
"compilations": 244,
"hits": 0,
"misses": 199,
"non_cacheable": 45,
"errors": 3,
"hit_rate": 0.0
```

Identical numbers on cold and warm. The restored cache files are
**present on disk** (oldest entries' mtimes are 80 min older than the
cook-mark, confirming they survived the tar extract), but zccache
treats every compile as a miss and re-writes the same content under
new keys.

Things tried that did NOT fix it:
- Explicit `ZCCACHE_PATH_REMAP=auto` on both cook and build steps
  (verified env block contains it).
- Verified `.git/` exists in the workspace (`actions/checkout` creates
  it; `git rev-parse --short HEAD` returns the commit).
- Coordinated `SOLDR_CACHE_DIR` + `ZCCACHE_CACHE_DIR` to the same
  per-job path so the cache lives where soldr expects it.

The same 0% hit rate is observed in **both** the delta job and the
baseline (single-cache) job, so this is not specific to the layering
design — it's a more fundamental zccache-cache-key issue across
GitHub Actions runs.

## Implications

- **The upload-bytes win is real and shippable** if quota / upload time
  is the bottleneck. Two `actions/cache` entries shrink the per-PR
  upload from ~178 MB to ~80 MB on warm runs.
- **The compile-time win is NOT validated.** Until zccache hits the
  restored cache, every CI run does a full rebuild regardless of cache
  layering. The 55% upload-byte savings doesn't translate to faster
  CI builds.

## Open question (for the reader)

Why does zccache report 0 hits against a cache dir that contains
exactly the entries the previous run just wrote? Candidates:

1. **Source-path embedding in cache keys**. CLAUDE.md notes
   `ZCCACHE_PATH_REMAP=auto` is supposed to normalize absolute paths.
   It's set in the env and `.git/` is present, so the documented
   auto-detect path should fire — but the symptom suggests it isn't.
2. **Toolchain rebuild between runs**. `setup-soldr` installs rustup
   + the toolchain fresh on each runner. Hash inputs that depend on
   the rustc binary's path or build-time identifiers might shift.
3. **Profile mismatch**. `soldr cook` defaults to dev profile;
   `soldr cargo build` does the same — but it's worth verifying
   they invoke rustc with identical flags.
4. **cargo-chef stub-source caching**. Cook builds dep tree with
   stubbed first-party source; build uses real source. The
   first-party crate's compile key WILL differ, but dep-crate cache
   keys should match — yet none do.

A separate, focused experiment (just `soldr cargo build` run twice
back-to-back with a cache restore between, no cook layer) would
validate whether zccache caching survives the actions/cache
round-trip at all. If even that simpler scenario shows 0% hits, the
delta-cache design is moot until the underlying issue is identified.

## PRs that built up to this point

1. #443 — composite action scaffold
2. #445 — workflow that exercises the action
3. #446 — baseline job + side-by-side compare
4. #447 — fix workflow parse (runner.temp in job env)
5. #448 — bump setup-soldr install to a version that has `soldr cook`
6. #449 — drop to 0.7.28 (0.7.29 not yet published)
7. #450 — drop unsupported `--tests` flag from `soldr cook`
8. #451 — coordinate `ZCCACHE_CACHE_DIR` with `SOLDR_CACHE_DIR`
9. #454 — fix comparison summary markdown (fake `colspan:`)
10. #455 — diagnostic step: zccache stats + cache-dir mtimes
11. #456 — diagnostic step exits 0 unconditionally
12. #457 — force `ZCCACHE_PATH_REMAP=auto` + print env

Findings stopped here pending the open question above.
