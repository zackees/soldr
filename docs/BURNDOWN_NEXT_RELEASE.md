# Burn-down list — next release, round 2 (10 verified bugs)

Round 1 (PR #3585, meta #3586) was fixed, merged, and closed on 2026-10-05.
This is the next ten. Every row was confirmed against `main` at `1b095968`
*after* round 1 merged — by reading the code, not by trusting the issue text.
Six issues previously believed actionable turned out fixed, blocked, or
superseded — see "Rejected candidates" at the bottom.

Ranked by user impact × confidence. `file:line` is the confirming evidence on
`main`. Overlap flags matter: the sibling meta #3582 claims some rows — do not
double-work them.

| # | Bug | Impact | Confirmed at | Issue | Also claimed by |
|---|-----|--------|--------------|-------|-----------------|
| 1 | `ci.toml`'s linter pin names a commit that does not exist | Gate-breaking: no `Local-Gate:` trailer can be produced when the pin is resolved | `ci.toml:14` (SHA 422s upstream) | #3584 | — (unclaimed) |
| 2 | `ci.toml` overstates the `compile` family 4.7× and omits the largest real family | Budget can never pass → cache policy has no enforcement | `ci.toml:110-114` | #3580 | — (unclaimed) |
| 3 | Managed `dylint-link` never on PATH for standalone lint-crate passes | `soldr cargo clippy` on a lint crate fails `linker not found` | `crates/soldr-cli/src/cargo_front_door/mod.rs:196` | #3571 | #3582 Group C (pair with now-closed #3483) |
| 4 | Cross-dylint provisions the target in one Rustup home, compiles from another | E0463 missing `core`/`std`; complete-gate failures | `crates/soldr-cli/src/dylint_target.rs:63-67` vs `binaries.rs:525` | #3567 | — (unclaimed) |
| 5 | Busy-lock remediation is a dead end: "inspect the lock file" names only a dead PID | Operators hand-scan `/proc/*/fd` (or copy stale guidance) on every occurrence | `crates/soldr-daemon/src/daemon/lifecycle/mod.rs:168-173` | #3581 | #3582 Group B |
| 6 | `cargo_front_door` wrapper tests need a clang on the isolated PATH | 10 tests fail on any host without `/usr/bin/clang`; only the `tests` lane catches it | `tests/common/mod.rs:2002` (`isolated_test_path`) | #3578 | fix **ready in PR #3579** (CLEAN) |
| 7 | Isolated cook IPC tests fail `WouldBlock` under workspace load | Green-lane flake; first-attempt evidence lost to the 20 KB tail (pre-#3564) | `crates/soldr-daemon/src/daemon/client_cook.rs:115` (error carries no stage) | #3558 | #3582 Group D |
| 8 | Broker route claim endpoint mismatch exhausts the 120 s acquisition ceiling | Local cross lanes stall 733 s+ before any lint finding | `crates/soldr-daemon/src/daemon/backend_handle_adoption.rs:400` | #3561 | #3582 Group D |
| 9 | 5–6 conflicting stderr-color predicates | Divergent `NO_COLOR`/`GITHUB_ACTIONS`/TTY rules across surfaces | `cache_states.rs:91`, `disk.rs:61`, `log_summary.rs:175`, `install/plan.rs:64`, `ci_test/test_targets.rs:152` | #3437 | #3582 Group C |
| 10 | Two implementations of the Nextest run-wrapper contract | Same contract in Python and Rust; drift risk | `.github/scripts/nextest_timeout_wrapper.py` (14.5 KB) vs `crates/soldr-nextest-wrapper/src/main.rs` | #3454 | #3582 Group C |

## Notes per row

**1 — invalid linter pin (#3584).** `ci.toml:14` declares
`linter = "zackees/ci.yml@a07bab94f16124b5c6857b137a237a53a61e06d1"`; the
commit does not exist upstream (verified today: HTTP 422, absent from the
store entirely — wrong on arrival, not drift). SEC-004 only checks that pins
are well-formed 40-hex SHAs, which this is. Note: round 1's gates ran because
they pinned `CI_LINT_REF` explicitly in `ci/local_gate.py`; anything that
resolves `ci.toml`'s `linter` fails before a lane starts. Fix: repoint at a
real commit **and** make "must resolve" a violation, not a review note.

**2 — budget misdeclaration (#3580).** `ci.toml:111` declares
`compile = { max = "2600MB", per = "platform", … }` — ~16.4 GB across six
platforms for a family whose live maximum entry is 558 MB — while the comment
one line above admits `zccache-unit` is the largest family measured at
2.43 GiB and **no such family is declared at all** (families: compile,
toolchain, registry, soldr-mini, dylint, dylint-out, uv). An over-max can
never trip, so the budget check never bites; the undeclared family is simply
invisible. Size `compile` from the live listing, declare (or explicitly
report as undeclareable) `zccache-unit-v1`, and confirm with
`ci-lint cache janitor --dry-run` that nothing proposed for deletion lives in
a declared family.

**3 — standalone `dylint-link` (#3571).** `append_subcommand_transitive_bin_dirs`
(`mod.rs:196`) adds the managed tool dir **only** when the subcommand is
literally `dylint`. A lint crate that declares
`[target.'cfg(all())'] rustflags = ["-C", "linker=dylint-link"]` and is built
by a plain `soldr cargo clippy` gets `linker dylint-link not found` —
reproduced in an isolated container against published 0.9.29
(FastLED/cli#279). Distinct from the closed #3483 (build succeeds, library
lookup fails) — this one is the binary never reaching the child PATH.
Resolution needs the canonical managed-tool answer the issue asks for;
ordinary crates must not start downloading Dylint tools.

**4 — split-homes E0463 (#3567).** `dylint_target::ensure_targets`
(`dylint_target.rs:67`) provisions targets through
`toolchain::effective_rustup_home` (managed homes applied), while the child
cargo builder goes through `binaries::apply_resolved_toolchain_homes`
(`binaries.rs:525`), whose host-binary branch deliberately preserves caller
homes (#1799). The provisioned `libcore` lands in the isolated cache home and
is absent from the caller's `~/.rustup` the compiler actually reads. Two
canonical-home rules, each correct in isolation — the exact shape the
CLAUDE.md home-origin rule exists to catch. Prove provisioner and compiler
select the same sysroot before touching either branch.

**5 — busy-lock diagnostic (#3581).** Half the issue is already fixed on
`main`: the message no longer recommends `pkill -f soldr-daemon`, and
`lifecycle/tests/events.rs:619` now asserts `!msg.contains("pkill")`. What
remains is the `(false, _)` arm (`mod.rs:168`): "inspect
`<root-owner.lock>` to identify its holder" — that file names only the **dead**
recorded PID, so the instruction dead-ends exactly as before, and the
recorded PID goes stale while live holders cycle. Fix direction from the
issue: enumerate holders (Linux `/proc/*/fd` scan for the lock inode) and
name them; where a platform cannot enumerate, say so.

**6 — isolated-clang tests (#3578).** The only `bug`-labeled issue open.
`isolated_test_path()` forces `PATH=/usr/bin:/bin` (+`System32`); ten tests
in `cli_cargo_wrappers.rs` then fail at the linker shim on hosts whose clang
lives elsewhere (e.g. NixOS `/run/current-system/...`). **A fix is already
open and green: PR #3579.** Burn-down action = review and merge, not new
work.

**7 — cook `WouldBlock` (#3558).** Concurrency-dependent: the same tests
pass in isolation and fail inside the 3,5xx-test workspace run. Triage last
round established the error is an expired `SO_RCVTIMEO` on the **read**
(zero non-test `set_nonblocking` calls), but `cook_record`'s error
(`client_cook.rs:115`) carries no stage — "read deadline expired",
"daemon not ready", and "allocation failed" are indistinguishable in the
report. The issue's RED is the exact focused command; its acceptance forbids
retries that discard first-attempt evidence. Instrument first, then fix the
diagnosed cause.

**8 — endpoint mismatch (#3561).** The diagnostic lives at
`backend_handle_adoption.rs:400` (`claimed=… expected=…`), unchanged after
#3573 (route-endpoint reaping) merged — that fix reclaims sockets no daemon
listens on, which is adjacent but not this: here the **claim** and the
**expectation** disagree under concurrent generations. Needs the repro the
issue describes (concurrent caller generations through the broker path) and a
bounded regression proving convergence; check explicitly whether #3573
already removed the stale-endpoint variant before building anything.

**9 — color predicates (#3437).** Five implementations on `main`, with
divergent rules: `install/plan.rs:64` treats `GITHUB_ACTIONS` as
color-capable, the others do not; `cache_states.rs:91` and
`log_summary.rs:175` and `ci_test/test_targets.rs:152` are three separate
`use_color()`s; `disk.rs:61` has the pure `color_enabled(no_color_set,
stderr_is_terminal)` rule. Meets the code-smell rule's three bars (multiplicity,
observable divergence, design call). Resolve to one predicate — pick which
behavior is canonical, don't inline-pick silently.

**10 — two Nextest wrappers (#3454).** Both exist on `main`: the Python
`.github/scripts/nextest_timeout_wrapper.py` and the Rust
`crates/soldr-nextest-wrapper` (its own workspace crate, exercised by the
wine lane). Same contract, two implementations — a code-smell-rule convergence
decision (retire the Python one per the issue's lean, or find the boundary
each owns).

## Rejected candidates (do not re-triage)

Verified already fixed on `main` (this round):

- **#3053** breach-dump slice — **done**: `rss_ceiling.rs:57-63` documents
  that the dump writes `cgroup.json`, `cgroup_path: Option<PathBuf>` is
  carried at `:284`, `BREACH_SCHEMA_VERSION = 3`.
- **#1060** installer libc fallback — **done**: `scripts/install.js` builds a
  `candidates` list (`primary` + sibling-libc `fallback` ~:171-181) and the
  download path walks it; the RFC's "failure → try the sibling" now exists.
- **#2930** bootstrap pin drift — stale as written: the "current published
  release" comment is gone; the Dockerfile now states *functional* floors
  (admission gate ≥ 0.9.6, PDEATHSIG > 0.9.15) and `0.9.21` satisfies both.
  The issue's real acceptance is a cold 8 GiB bosn proof, not a patch.
- **#3333** unshard macOS Recovery — not actionable alone: the 3-way
  `replay_partitions` sharding in `macos-recovery-replay.yml:72` is
  deliberate while the guest freeze (#3136, runner/hardware) stays open.

Carried forward from round 1's rejections (still valid):

- **#3435** musl host-target wheel — intended, pinned by
  `wheel_cmd_tests.rs:333-341`.
- **#3503** `ZCCACHE_CACHE_SIZE_BYTES` inert — the knob is read and drives
  the maintenance loop.
- **#3499** cook-index pack cost — perf request, not a defect.
- **#2924** nested-Cargo self-locks — `nested_cargo_guard` is wired (note:
  #3278's claim of "zero callers" was checked and disagrees with the tree).

Not bugs / not bounded fixes:

- **#2469** gated candidate promotion — P1 but a phased process repair whose
  Phase 0 depends on a setup-soldr release; RELEASE.md's two-dispatch flow
  (#3346) covers part of it. Track as process work, not a burn-down row.
- **#3570** ARM GTK/GLib target deps — real, but its own "Open questions"
  (ABI floor, which receipt owns the closure) make it a design row; strongest
  alternate if a row above closes early.
- **#2706 / #3044 / #3045** — upstream zccache, cross-repo pin bump.
- **#3494 / #3495** — parked on meta #3367's design decision.
- **#2693 / #3136 / #3088** — blocked on runner/hardware.
- **#3528 / #3535** — PY-003 / PY-002 ratchet burn-downs, self-contained and
  already owned.

## Coordination notes

- **Meta #3582** ("unclaimed backlog") claims rows 3, 5, 7, 8, 9, 10 — one
  owner per row; this list does not duplicate its work, it re-verifies it.
  Its Group A is now **stale**: the seven rows it listed (#3533, #3540,
  #3485, #3504, #3538, #3564, #3565) were all closed by round 1's merges
  today; its body still shows them open (written against `1240681c`).
- **PR #3579** (fixes row 6) is open, CLEAN, and awaiting merge.
- **PR #3583** (CACHE-034) touches the same `ci.toml` as rows 1–2; land
  ordering matters if #3584/#3580 edit the same lines.
- Rows 1–2 came from today's fleet audit (`zackees/bosn#534` shares row 1's
  root cause) — both were wrong-on-arrival in the first `ci.toml` (#3576).