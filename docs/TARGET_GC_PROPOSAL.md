# Proposal: Automatic GC of Cargo `target/` Directories

Status: Exploration / proposal. Not a spec yet.
Tracks: [#233](https://github.com/zackees/soldr/issues/233). Adjacent: [#234](https://github.com/zackees/soldr/issues/234).

## TL;DR

Adopt the [#234](https://github.com/zackees/soldr/issues/234) registry approach as the data plane, and layer a wrapper-driven, manually-triggered GC on top of it: every `RUSTC_WRAPPER` invocation upserts `(target_dir, last_used_unix_ts)` into `~/.soldr/state.redb`, and `soldr gc` (alias `soldr --purge`) is the single user entry point that scans the registry, sizes candidates lazily, and deletes whole `target/` trees that are older than a threshold and not currently locked. **Do not roll our own selective pruner; do not auto-delete on startup; do not try to share `CARGO_TARGET_DIR` across worktrees.** Defer atime, scheduled triggers, and incremental-subtree pruning until we have real telemetry.

The recommendation converges on #234 with three additions: (a) explicit lock-file safety check, (b) wrapper records the *workspace root*, not the `target/` path it deduces from cwd, (c) explicit "do not GC if `Cargo.lock` mtime is younger than threshold" guard.

---

## Question 1 — Signal for staleness

**Recommendation: use `last_used` written by the `RUSTC_WRAPPER` itself. Do not rely on atime.**

Reasoning:

- **atime is unreliable on the user's primary platform.** On Windows 10, `NtfsDisableLastAccessUpdate` defaults to "System Managed" (`0x80000002` / `0x80000003`), which *disables* last-access updates on volumes >128 GiB. The user's pain is on a workstation with 860 GB of dev directories spread across what is almost certainly a >128 GiB volume, so atime is silently off by default. Even when on, NTFS only flushes atime within one hour, and many devs disable it for SSD wear / perf reasons. Linux behaves similarly (`relatime` / `noatime`).
- **Cargo.toml mtime is the wrong signal.** Editing source under `src/` does not touch `Cargo.toml`. A repo that's been built daily for two years can still have an unchanged `Cargo.toml`. mtime measures *project change*, not *build recency*.
- **`cargo metadata` is too expensive to run across 41 dirs on every scan,** and it requires the toolchain to still resolve — useless for orphan `target/` dirs whose `Cargo.toml` has been deleted.
- **Wrapper-recorded `last_used` is exact, free, and platform-independent.** soldr is already in the rustc invocation path on every build. The upsert is one redb write per *cargo invocation* (debounce inside the daemon — not per rustc spawn), so cost is negligible.

Cost/benefit: Wrapper-recorded ts is the cheapest accurate signal we can get and avoids a Windows-specific footgun. Cost is the redb store.

---

## Question 2 — Granularity

**Recommendation: whole-`target/` deletion only, in v1. Do not implement selective pruning. Do not wrap `cargo-sweep`.**

Reasoning:

- The user's stated pain is "~250 GB across ~41 `target/` dirs across 7 repos." That math is **~6 GB/dir average**, dominated by *long-tail forgotten worktrees*, not by intra-`target/` waste in actively-used projects. Selective pruning (`incremental/`, old profile dirs, orphan `deps/`) is the wrong knob: it optimizes the small case while leaving the big case (whole forgotten dirs) untouched.
- `cargo-sweep` already exists for the selective-pruning case. If a user wants `incremental/`-only cleanup on an active repo, they can `soldr cargo-sweep --time 30` once it's added to `known_tools`. soldr should not duplicate that surface.
- `cargo-cache` operates on `$CARGO_HOME`, not `target/` — out of scope for this issue, but a candidate for a sibling `soldr cache prune-registry` later.
- Whole-`target/` deletion is **safe-by-construction**: cargo will rebuild from source. The only loss is wall-clock time on next build, and zccache absorbs much of that.

Cost/benefit: Whole-`target/` is one `remove_dir_all` call vs. an entire glob/age/profile heuristic engine. Ships in a sprint. Selective pruning ships in a quarter and is the wrong optimization for this user's disk-fill pattern.

---

## Question 3 — Worktree interaction

**Recommendation: per-worktree `target/` with per-worktree GC. Do NOT push users toward shared `CARGO_TARGET_DIR`.**

Reasoning:

- Shared `CARGO_TARGET_DIR` across worktrees of the same repo causes two well-known cargo footguns: (1) feature-unification thrash when worktrees check out branches with different feature sets, forcing recompiles on every worktree switch; (2) `Cargo.lock` divergence when worktrees pin different versions, which invalidates the entire shared `target/` on each switch. Net effect on agent worktrees (which churn branches constantly) is *worse* cache hit rate, not better.
- zccache already provides the cross-worktree dedup we want at the *compilation-unit* level. Pointing every worktree at the same `target/` would just move the contention from zccache (which is built for it) into cargo (which is not).
- The right answer for worktrees is the same as for any other `target/`: **let it grow in place, and reclaim it via the registry.** Worktrees that haven't been built in 30 days get GC'd just like any other dir.
- Optional v2 enhancement: detect `.git/worktrees/<name>` parent and use **last-commit-date of the worktree's HEAD** as a tiebreaker signal (cheaper than scanning every source file mtime). Skipped in v1.

Cost/benefit: Doing nothing special for worktrees is free and correct. Pushing shared `CARGO_TARGET_DIR` would be an active regression for the user's described workflow.

---

## Question 4 — Shared caches vs. per-project targets

**Recommendation: do NOT try to make `target/` disposable in this issue. Keep zccache as a pure compiler-output cache; treat `target/` as a workspace-local scratch dir that GC reclaims.**

Reasoning:

- Making `target/` truly disposable requires content-addressing the entire build graph including link outputs, build-script outputs (`OUT_DIR`), proc-macro `dylib`s, and incremental metadata. That's a multi-quarter project (it's roughly the scope of bazel / buck2's Rust rules) and is fundamentally a *cargo* problem, not a soldr problem.
- zccache today caches `rustc` outputs at the compilation-unit level. That's the right slice. Extending it to absorb `OUT_DIR` and link products would (a) require deep cargo integration soldr has explicitly chosen not to do (see CLAUDE.md: "soldr wraps rustc, NOT cargo"), and (b) duplicate the existing `target/` layout in the cache, doubling disk usage during the migration period.
- The pragmatic win is: **zccache makes `target/` cheap to rebuild (often 5-30s instead of 5-30min), which makes whole-`target/` GC a safe and obvious choice.** That's the leverage we already have. We don't need to make `target/` *disposable* — we just need to make it *re-buildable cheaply enough that deleting it doesn't hurt*.

Cost/benefit: Status quo + GC has 95% of the upside of "disposable target/" at 5% of the engineering cost. Revisit after we have a year of GC telemetry.

---

## Question 5 — Trigger model

**Recommendation: manual-only in v1 (`soldr gc`, `soldr gc --dry-run`). Plus a passive *warning* on startup (per #234) — but no automatic deletion.**

Reasoning:

- **Opportunistic deletion (before/after each invocation) is dangerous.** It makes build latency non-deterministic and risks racing an in-progress build in a sibling directory. Reject.
- **Threshold-based (>N% disk full) requires platform-specific disk-usage probing on every wrapper invocation.** Marginal value for the cost; hides the deletion from the user. Reject for v1.
- **Scheduled (cron / Task Scheduler) requires us to install a system service.** soldr has explicitly avoided this (no `soldr start`); the daemon is auto-spawned per-build, not a persistent system service. Adding a scheduler would be a category change. Reject.
- **Manual + warning is the minimum-surprise path.** User runs `soldr gc` when they notice the warning ("4 stale `target/` dirs using 12.3 GB"). One command, one obvious effect. We can add `--auto` or `--cron` later if telemetry says manual is too friction-heavy. Easy to extend, hard to walk back.

Cost/benefit: Manual ships in days. Auto-modes can be added later without breaking the manual flow. The reverse (shipping auto first, then bolting on manual) tends to leave footguns.

---

## Question 6 — Safety

**Recommendation: three explicit guards, all checked before any `remove_dir_all`:**

1. **Skip if any `Cargo.lock` in or above the target dir has mtime within the last hour.** Catches in-progress builds, paused-and-resumed dev sessions, and IDE-driven background `cargo check`. Cheap (one stat per candidate).
2. **Skip if a `.cargo-lock` file exists inside `target/`.** Cargo writes this during builds; presence means an active build process. Authoritative signal.
3. **Skip if the `target/` dir contains a `release/` build whose newest binary is younger than 7 days AND the parent repo has uncommitted changes / detached HEAD / a tag pointing at it.** This is the "force-pushed tag release artifact" case from the issue. Heuristic, not perfect; document it as best-effort.

Additional safety:

- **Always print the path + size before deleting**, even in non-`--dry-run` mode. Use a `--quiet` flag for scripted use.
- **Move-to-trash when available** (Windows Recycle Bin via `SHFileOperation` / `trash` crate on macOS / `gio trash` on Linux) for `--purge`; reserve hard delete for `--purge --hard` or `--purge-all`. Recovery without backups is otherwise impossible.
- **Refuse to GC any path that is not under a known dev parent** (`~`, `$HOME`, configured `dev_root`s in `~/.soldr/config.toml`). Prevents accidental deletion of `/var/lib/.../target` or similar.
- **Never follow symlinks during the `remove_dir_all` walk.** Use `std::fs::remove_dir_all` semantics, not a custom recursive walker that might descend through a symlink into the user's home dir.

Cost/benefit: All four guards are cheap (stat + path-prefix checks). They turn GC from "scary" into "safe enough that the user will actually run it."

---

## Concrete v1 scope (recommended)

**In:**

1. `~/.soldr/state.redb` with the target registry table from #234.
2. Wrapper-mode upsert: on every `RUSTC_WRAPPER` invocation, upsert `(workspace_target_dir, now_unix)`. Debounce in-process to one write per cargo invocation.
3. `soldr gc` command:
   - Default: list candidates (>10 days unused, >256 MB), prompt y/N per dir.
   - `--dry-run`: list only.
   - `--all`: skip prompts.
   - `--older-than 30d --larger-than 1GB`: tunable thresholds.
   - `--hard`: bypass trash, hard delete.
4. Startup warning (throttled to once per day per session) per #234.
5. Three safety guards from Q6 plus `dev_root` allowlist in config.

**Out (deferred to follow-up issues):**

- Selective intra-`target/` pruning (let users `soldr cargo-sweep`).
- Shared `CARGO_TARGET_DIR` orchestration.
- Disk-threshold / scheduled / opportunistic triggers.
- atime-based scoring.
- Making `target/` disposable via cache extension.

## Open questions for issue triage

- Should `soldr gc` register a Windows Task Scheduler entry on first run with user opt-in? (My instinct: no, but worth a vote.)
- Should the registry also track `~/.cargo/registry/cache` size, or punt to `cargo-cache`?
- Threshold defaults (10 days / 256 MB) come from #234 — do they hold up against the user's "every few weeks" sweep cadence? Probably yes; revisit after telemetry.

## References

- Issue #233 (this exploration)
- Issue #234 (the target-registry implementation proposal we're converging on)
- `cargo-sweep` — selective intra-`target/` pruning by stamp file or age
- `cargo-cache` — `$CARGO_HOME` cleanup, not `target/`
- NTFS atime semantics: [Ntfs Last Access Update rules by Windows version (jipegit gist)](https://gist.github.com/jipegit/4f6602456f0c2fe256642cecee09b425), ["The 'Last Access' updates are almost back" — DFIR blog](https://dfir.ru/2018/12/08/the-last-access-updates-are-almost-back/)
- `CLAUDE.md` "Key Design Rules" — frozen built-in commands list (note: `gc` is *not* in the frozen list; adding it requires updating that list)
