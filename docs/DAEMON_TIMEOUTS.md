# Daemon timeout & stall runbook

When a build stops making progress, the usual cause is the embedded zccache
daemon being slow, wedged, or shutting down — and the symptom (a long silent
wait, or a build that quietly runs uncached) rarely names itself. This is the
one place that maps each failure mode to its signal, how to confirm it, and
what to do. It is the operator companion to soldr#1838.

If you just want out **now**: `soldr --no-cache cargo <args>` bypasses the
wrapper and daemon entirely and runs the compiler directly (uncached). The rest
of this page is for understanding *why* and choosing a durable fix.

## First: ask soldr what it sees

Two read-only commands surface everything below without guessing:

- **`soldr doctor`** — prints the effective value and provenance of every
  timeout (default vs. env override vs. an override that was ignored), plus a
  **compile-daemon fallback rollup**: how many recent builds ran uncached via
  the direct compiler, newest first. `soldr doctor --json` carries the same as
  `timeouts[]` and `fallbacks: { total, recent[] }`.
- **`soldr status`** — the same fallback rollup, from the command you reach for
  first.

A non-zero fallback count is the single most important signal: it means builds
have been **silently running uncached**, which is the "quietly 10–50× slower,
indefinitely" failure. The raw journal is
`~/.soldr/logs/compile-daemon-fallbacks.jsonl` (`~/.soldr-dev/…` for
development builds).

**A long wait is not silent.** While blocked on a slow daemon reply — a
compile, a cache flush, or a graceful shutdown — soldr prints a progressive
heartbeat to stderr every ~minute rather than going quiet until the backstop:

```
soldr: daemon compile reply still waiting after 120s (deadline 1800s from SOLDR_COMPILE_REPLY_TIMEOUT_SECS); if this is a wedged cache rather than slow work, `soldr --no-cache cargo ...` or ZCCACHE_DISABLE=1 bypasses the daemon
```

The line names the operation, the elapsed time, the active deadline and its
env override, and the bypass — so seeing one *is* the signal that a wait is
running long, with the remedy attached. (It goes to stderr, so `--json` output
on stdout is untouched.)

## The timeout surface

`soldr doctor` is authoritative for the *effective* values on your machine;
this table is the defaults and the knobs.

| Bound | Default | Override | Notes |
|---|---|---|---|
| Hot-path write | 50 ms | — | daemon must accept a request quickly or the client moves on |
| Status / shutdown reply | 2 s | — | health handshakes are meant to be instant |
| Cache flush reply | 5 min | — | a large index/LTO flush is legitimately slow |
| **Compile reply** | **30 min** | `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` | the backstop a wedged compile waits out; **shorten it to fail fast** |
| Graceful shutdown wait | 5 min | — | how long `soldr daemon stop` waits before escalating |
| Cache shutdown | 5 min | `SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS` | end-of-command embedded-cache drain |
| Command output capture | 60 s | `SOLDR_COMMAND_OUTPUT_TIMEOUT_SECS` | bounded reads of helper tools |
| Toolchain command | 30 min | `SOLDR_TOOLCHAIN_COMMAND_TIMEOUT_SECS` | `rustup`/`cargo` toolchain steps |
| rustup target add | 15 min | `SOLDR_RUSTUP_TARGET_ADD_TIMEOUT_SECS` | |
| rustup-init bootstrap | 15 min | `SOLDR_RUSTUP_INIT_TIMEOUT_SECS` | |
| build-from-source cargo install | 45 min | `SOLDR_BUILD_FROM_SOURCE_INSTALL_TIMEOUT_SECS` | |
| Cargo front-door wait | off | `SOLDR_CARGO_WAIT_TIMEOUT_SECS` | opt-in wall-clock cap on the whole cargo run |

**Override rule (soldr#1837):** a malformed or `0`/empty value **falls back to
the default, never to "disabled"** — a fat-fingered timeout can never silently
remove a backstop. `soldr doctor` marks an override that was set but did not
take effect.

## Failure modes → signal → remedy

### 1. A compile hangs for minutes with no output

- **Signal:** cargo sits on one crate; soldr prints a `daemon compile reply still waiting after Ns` heartbeat every ~minute (see above) up to the 30-min backstop.
- **Confirm:** the daemon is reachable (`soldr status` returns) but slow, or a maintenance pass is holding state.
- **Remedy now:** `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=30 soldr cargo <args>` fails the wrapper fast and falls back to the direct compiler instead of waiting out the 30-min backstop. Or bypass entirely: `soldr --no-cache cargo <args>`.
- **Durable:** if it recurs, the daemon is wedged — `soldr daemon stop`, then rebuild (a fresh daemon starts automatically).

### 2. Builds are quietly slow (no hang, just uncached)

- **Signal:** warm/incremental builds are much slower than they should be, but nothing fails.
- **Confirm:** `soldr doctor` (or `soldr status`) shows a **non-zero compile-daemon fallback count**. Each entry names why the cache was bypassed and when.
- **Remedy:** the reasons point at the cause — daemon unreachable, reply timed out, ownership busy. If it is an unrecoverable-looking wedge, `soldr daemon stop` clears it. If the daemon is healthy and the count is old, it is history, not a live problem.

### 3. A build fails outright during daemon shutdown

- **Signal:** a compile fails right as a daemon is retiring.
- **Design:** a daemon that is shutting down answers with an explicit **`Retiring`** signal (soldr#1838 Phase 2), and the wrapper **degrades to the direct compiler** rather than hard-failing. This is automatic; you should not see a failure from this path on a current daemon.
- **If you do:** you are likely on a version-skewed daemon that predates the signal — `soldr daemon stop` and let the current build start a fresh one.

### 4. `soldr daemon stop` itself hangs

- **Signal:** stop takes minutes; a `daemon graceful shutdown still waiting after Ns` heartbeat prints while it drains.
- **Design:** it waits up to the 5-min graceful-shutdown window for in-flight work to drain, then escalates.
- **Remedy:** let it finish; it is bounded. Do not kill the process mid-drain unless it exceeds the window — that is what the watchdog is for.

### 5. `Database already open. Cannot acquire lock.` from a build-session fallback

- **Signal:** `soldr warning: failed to persist build-session start/end fallback for <id>: … Database already open. Cannot acquire lock.`, usually while another build is running.
- **It is not corruption.** That is redb's wording for "another process holds this file", and `~/.soldr/state.redb` is deliberately shared by the `soldr cargo` front door, the per-compile rustc wrapper, the daemon, GC, and the reporting CLI. soldr#2223 was filed on the corruption reading; soldr#2224 is the fix.
- **Blast radius:** the build itself is unaffected — only that session's history row is skipped, so `soldr status` / build-log history may be missing one entry.
- **Design (soldr#2224):** the two things that made it likely are gone. The daemon's maintenance sweep no longer holds the handle across directory sizing and deletion (it snapshots, releases, deletes, then reopens for a bounded write), and each fallback now acquires the database **once** instead of three or four times.
- **Forensics:** every contended open appends a record to `~/.soldr/logs/redb-contention.jsonl` with `attempts`, `elapsed_ms`, `intent`, and the holder-side `pid`. If you still see the warning, that file says how long the wait was and how often it happens.

## Recovery cheat-sheet

| Goal | Do this |
|---|---|
| Get unblocked immediately | `soldr --no-cache cargo <args>` (note: `--no-cache` goes **before** `cargo`) |
| Same, via env | `ZCCACHE_DISABLE=1` |
| Fail fast instead of waiting the 30-min backstop | `SOLDR_COMPILE_REPLY_TIMEOUT_SECS=30 soldr cargo <args>` |
| Clear a wedged daemon | `soldr daemon stop` (a fresh one starts on the next build) |
| See effective timeouts + recent fallbacks | `soldr doctor` |

## Degrade policy — what auto-degrades vs. hard-fails

soldr degrades a compile to the direct compiler when the daemon *answered but
cannot serve this compile* — including the `Retiring` shutdown signal — and
records a fallback so `soldr doctor` can surface it. It **hard-fails** only on a
genuine protocol violation, so a real daemon bug is never masked as a silent
slowdown. The full decision table lives in
`crates/soldr-cli/src/compile_dispatch.rs` (`the_degrade_policy_matches_its_documented_table`).
