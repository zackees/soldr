# Perf Matrix Optimization Loop

You are a ralph-loop agent driving `.github/workflows/perf-matrix.yml`
down from **~17 minutes** to **≤ 9 minutes** of total wall time, with
the stronger invariant that **every warm build is ≤ ½ of its cold
parent**. You will iterate: change code, push a commit to one
long-lived PR, dispatch the workflow, read the run, compare to the
prior iteration, decide what to change next.

This file is your only context. Re-read it at the top of every
iteration.

---

## Stop condition (read this twice)

You are done when **all** of the following are true on the most
recent perf-matrix run on your branch:

1. The whole workflow run (max of `bench (linux / medium)` and
   `bench (linux / sqlite-link)`, plus `build-soldr`) finishes in
   **≤ 9 minutes wall time**.
2. For every scenario that emits a `cold_ms` and a `warm_ms` pair
   (today: `cold-tar-untar-warm`, `worktree-share` via `a_ms`/`b_ms`,
   `touch-no-change`), on **both** fixtures (`medium` and
   `sqlite-link`): `warm_ms ≤ cold_ms / 2`. Use the JSON line that
   each scenario writes to its job log — search for
   `"scenario":"...".`
3. `warm_hits + warm_misses > 0` AND `warm_hit_rate ≥ 0.5` for every
   warm row. (Today they all report 0 because of an observability
   bug — see Hypothesis B.)

When you hit all three, stop, post a short summary comment to the
PR (`gh pr comment <PR>`), and exit.

If you've completed **10 iterations** without hitting the stop
condition, stop anyway and post what you tried and what didn't work.
Do NOT exceed 10 iterations.

---

## Baseline numbers (run 26191930005, main @ 7940413)

| Job | Wall | Notes |
|---|---|---|
| build-soldr (linux) | 1m50s | one-shot |
| bench (linux / sqlite-link) | 4m52s | small fixture (~30 crates) |
| bench (linux / medium) | 15m25s | ~200 crates, dominates total |

Inside `bench (linux / medium)`:

```
{"scenario":"cold-tar-untar-warm","cold_ms":387299,"warm_ms":384666,"warm_hits":0,"warm_misses":0,"warm_hit_rate":0,...}
{"scenario":"worktree-share","a_ms":51730,"b_ms":9703,"b_hits":0,"b_misses":0,"b_hit_rate":0,...}
{"scenario":"touch-no-change","cold_ms":51347,"warm_ms":9530,"warm_hits":0,"warm_misses":0,"warm_hit_rate":0,...}
```

`worktree-share` and `touch-no-change` already get the speedup we
want (51s → 9.5s, ratio ≈ 0.19), but report 0 hits. `cold-tar-untar-warm`
is broken: 387s → 384s, ratio ≈ 1.0. That one scenario eats ~13 of
the 15.4 minutes of `bench/medium`. Fix it and we beat 9 minutes.

---

## Hypotheses to investigate, in priority order

### A. zccache bootstrap is duplicated per `SOLDR_CACHE_DIR` (highest impact)

Search the medium-bench job log for `Installing /home/runner/.../bin/zccache`:

```
21:52:58 Installing .../cache-cold/bin/.../zccache
21:54:50 Installing .../cache-cold/bin/.../zccache-daemon
21:55:46 Installing .../cache-cold/bin/.../zccache-fp
21:59:32 Installing .../cache-warm/bin/.../zccache       <-- AGAIN
22:01:24 Installing .../cache-warm/bin/.../zccache-daemon <-- AGAIN
22:02:21 Installing .../cache-warm/bin/.../zccache-fp     <-- AGAIN
```

Each install is ~5 minutes of `cargo install` from crates.io. The
`cold-tar-untar-warm` scenario creates two cache dirs (`cache-cold/`
and `cache-warm/`) so the bootstrap happens twice. The actual `cargo
build` of `medium-rust-app` is sub-second after install.

Likely fixes (try in this order):

1. **Pre-fetch zccache once in `build-soldr` and ship it alongside
   the soldr binary.** Set `SOLDR_ZCCACHE_LOCAL_DIR` in the bench
   job to point at the unpacked binaries so soldr skips the
   per-cache-dir install. See `CLAUDE.md` § "Local zccache for
   debugging" for the env-var contract.
2. **Share one cache dir across cold and warm in
   `cold-tar-untar-warm`** — populate via a normal build, then `tar`
   the dir, `rm -rf` it, `untar` to a SECOND location only for the
   warm half. The point of that scenario is archive fidelity, not
   bootstrap-from-scratch fidelity; the warm half only needs a fresh
   restoration of cache *contents*, not a fresh `cargo install
   zccache`.
3. **If neither of the above is enough,** investigate why managed-
   zccache GitHub-Releases fetch isn't being used. It should be
   downloading a prebuilt binary, not `cargo install`ing. Look at
   `crates/soldr-fetch/src/lib.rs` and the `MANAGED_ZCCACHE_VERSION`
   chain. Stdout/stderr lines like "soldr: using managed zccache
   1.x.y" or "soldr: fetched managed zccache 1.x.y" will tell you
   which path fired.

### B. `warm_hit_rate=0` is an observability bug, not a real miss

`perf/scenarios/*/run.sh` runs:

```bash
warm_stats="$(SOLDR_CACHE_DIR="${CACHE_WARM}" measure::session_end_json)"
warm_hits="$(echo "${warm_stats}" | jq -r '.stats.hits // 0')"
```

`measure::session_end_json` is in `perf/lib/common.sh`. It runs
`soldr session-end --json` without `--id` or `$ZCCACHE_SESSION_ID`,
which makes soldr exit non-zero with `session-end requires --id or
$ZCCACHE_SESSION_ID to be set`. The function swallows stderr and
echoes `{}` — that's why `.stats.hits // 0` becomes `0`.

Meanwhile the scenario's build DOES emit a real session summary —
search the log for `compilations: 146; hits: 115; misses: 0` — but
the script never sees that.

Fix options:

- Parse `<cache_dir>/cache/zccache/logs/last-session-stats.json`
  directly. soldr writes that file at session-end during the build,
  so it's already on disk. Use `jq -r '.hits // 0'` (no `.stats`
  prefix — the file is the inner stats blob).
- Or pass `--id "${ZCCACHE_SESSION_ID}"` to `soldr session-end` if
  the build leaks the session id into the calling shell (it doesn't
  by default; the env var lives in the child process).

Either way, the warm hit/miss reporting needs to actually work so
you can verify your other changes.

### C. Per-scenario fixture extract overhead (small)

Every scenario re-extracts the fixture tarball into its own per-
scenario workdir. medium.tar.gz is 14 KB, so this is negligible.
Don't waste an iteration on it.

### D. `bench/medium` and `bench/sqlite-link` run in parallel

They already do. No win here unless you fold them serially for
cache sharing — don't.

---

## Iteration protocol

Each iteration:

1. **Read the latest perf-matrix run** for your branch:

   ```bash
   gh run list -R zackees/soldr --workflow=perf-matrix.yml --branch <your-branch> --limit 3
   gh run view <run-id> -R zackees/soldr
   gh run view --job <medium-job-id> -R zackees/soldr --log \
     | grep -E "scenario:|Compiling|Installing|hits:|compilations:|^\\{\"scenario\""
   ```

   Capture the three JSON `{"scenario":...}` lines and the
   per-job wall times. Note them in your scratch.

2. **Form one hypothesis** that is most likely to move the worst
   row. Don't change two things at once — you'll lose attribution.

3. **Make the change**, keeping the surface area small. Touch the
   minimum number of files.

4. **Commit + push to your branch**, then **dispatch the workflow**:

   ```bash
   git add -A && git commit -m "perf: <what changed and why, ≤80 chars>"
   git push
   gh workflow run perf-matrix.yml -R zackees/soldr --ref <your-branch>
   ```

5. **Wait for the run.** `gh run watch <run-id>` blocks until done.
   The workflow takes 5–17 minutes. Use that time to draft the next
   hypothesis based on what you expect to see.

6. **Compare** the new JSON lines against the prior iteration's.
   Record cold_ms, warm_ms, ratio (warm/cold), and total job wall
   time, fixture by fixture.

7. **Decide:** are we closer to the stop condition? If yes,
   continue. If the change made things worse, **revert it in the
   next commit** rather than piling more changes on top — keep the
   PR's diff against `main` minimal and easy to read.

---

## Setup — do this once at iteration 1

1. Branch off `origin/main`:

   ```bash
   git fetch origin main
   git checkout -b perf/loop-optimize origin/main
   ```

2. Open a draft PR so subsequent commits all show up in one place:

   ```bash
   git commit --allow-empty -m "perf: open loop PR"
   git push -u origin perf/loop-optimize
   gh pr create --draft --title "perf: ralph-loop optimizations toward 9m total" \
     --body "Tracking PR for the LOOP.md ralph loop. Each commit is one iteration."
   ```

3. Note the PR number for `gh pr comment` later.

---

## Things you must NOT do

- **Do not edit `LOOP.md` itself.** It's your spec. Edit it only if
  the user explicitly tells you to.
- **Do not change perf-matrix.yml's structure (jobs, matrix axes,
  triggers).** Optimize within the existing shape. The user has
  spent multiple PRs nailing this shape down; don't churn it.
- **Do not delete scenarios or fixtures.** They each pin a specific
  failure mode (see `perf/README.md`). Optimize them, don't remove
  them.
- **Do not merge the PR.** The user wants visibility into every
  iteration. Push commits, let them review.
- **Do not exceed 10 iterations.** If you can't hit the goal in 10,
  stop and report.
- **Do not bypass repo policy.** Every action in the workflow must
  be SHA-pinned (`actions/foo@<40 hex> # vN`). The repo will reject
  unpinned tags at the API level — see the parent-cache-bench fix
  in commit `6985a297`.

---

## Useful context paths

- `perf/scenarios/*/run.sh` — scenario implementations
- `perf/lib/common.sh` — `measure::*` helpers (incl. the broken
  `session_end_json`)
- `perf/lib/extract.sh` — fixture extractor
- `perf/fixtures/medium/Cargo.toml` — the slow fixture
- `crates/soldr-fetch/src/lib.rs` — managed-zccache resolution
- `CLAUDE.md` — repo invariants, especially the
  "Local zccache for debugging" section about
  `SOLDR_ZCCACHE_LOCAL_DIR`
- `.github/workflows/perf-matrix.yml` — the workflow itself

Run `cat .github/workflows/perf-matrix.yml` and
`cat perf/scenarios/cold-tar-untar-warm/run.sh` before iteration 1
so you have the full surface in head.

Good luck.
