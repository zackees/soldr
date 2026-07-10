# Build-profile levers: `-Zshare-generics=y` and `incremental = true`

Outcome of the 2026-07 audit requested in
[soldr#1505](https://github.com/zackees/soldr/issues/1505) (alongside
TechWatchProject/datalake-core#279): every Rust repo under `zackees`
(soldr, zccache, clud, running-process, ai-tools) plus FastLED was audited
for wins from these two cargo levers. **The audit found no missing wins.**
This doc records why, so nobody cargo-cults the flags in later expecting a
speedup.

## TL;DR

| Lever | Verdict |
|---|---|
| `-Zshare-generics=y` | **Do not add anywhere.** Already default-ON at the opt-levels where it helps; a net negative for shipped release binaries. |
| `incremental = true` in `[profile.dev]` | **Never write it.** It is the default; writing it only invites confusion. Guard the inverse instead (no stray `CARGO_INCREMENTAL=0` in dev, no `incremental = false` in dev profiles — zero instances found). |
| `CARGO_INCREMENTAL=0` in CI | **Keep it the norm.** CI runs are effectively clean builds; incremental adds metadata overhead and non-determinism. |

## Lever 1: `-Zshare-generics=y`

Shares monomorphized generic instantiations across crates in the build
graph instead of re-instantiating them in every downstream crate. The
biggest theoretical win is a multi-crate workspace with generics-heavy
deps (serde, clap, tokio, rayon).

**The non-obvious fact that decides the audit:** rustc *already defaults
share-generics ON* at `opt-level = 0, 1, s, z`, and OFF only at
`opt-level = 2, 3` (`rustc_session::Session::share_generics()`; see
rust-lang/rust#142164). Consequences:

- **Dev builds (opt-level 0) already share generics** — on stable, with
  no flag. The explicit flag is a no-op there.
- The flag only changes behavior for code compiled at opt-level 2/3:
  release builds, or the Bevy-style `[profile.dev.package."*"]
  opt-level = 3` pattern. No zackees repo builds deps above opt-level 1.
- It needs nightly (`-Z`) or `RUSTC_BOOTSTRAP=1` on stable, and for
  *shipped release binaries* it is usually a net negative: it blocks
  cross-crate inlining, trading runtime performance and size for compile
  time. rustc defaults it off at opt 2/3 deliberately.
- Interaction caveat: monomorphizations from incrementally-compiled
  upstream crates are not reliably exported, so combining the two levers
  is unreliable — another reason to leave both at their defaults.

**If a repo ever adopts `[profile.dev.package."*"] opt-level = 2/3`,
revisit.** The cleanest home for such a policy would be soldr injecting
the flag (gated on nightly / `RUSTC_BOOTSTRAP=1` detection) rather than
per-repo `.cargo/config.toml` copies — soldr already treats
`RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` as cache-key inputs
(`cargo_front_door/inputs.rs`), so the flag would flow through cache keys
correctly.

## Lever 2: `incremental = true`

- Default **ON** for `dev`, default **OFF** for `release`. Writing
  `incremental = true` into `[profile.dev]` is a no-op.
- The real win is making sure it isn't *accidentally disabled* for local
  dev loops (`CARGO_INCREMENTAL=0` leaking into a dev environment, or
  `incremental = false` in a dev profile). The audit found zero instances
  of either across all five repos.
- Incremental is ignored when a profile enables LTO.
- **zccache + incremental coexist by design**: zccache strips
  `-C incremental` from cache-key computation (`EXCLUDED_CODEGEN` in
  `zccache-depgraph/src/rustc_args.rs`) and filters `CARGO_INCREMENTAL`
  from tracked env, so local incremental dev loops don't poison or
  fragment the shared cache.

## Per-repo verdicts (2026-07)

| Repo | share-generics | incremental | Verdict |
|---|---|---|---|
| **soldr** | Dev + `ci-bootstrap` (opt 0) + `ci-release`/`ci-nextest` (opt 1) get it by default. Shipped `release` is `lto = "thin"` + `codegen-units = 1` — forcing it there trades runtime perf for compile time. | Dev default-on; CI gets `CARGO_INCREMENTAL=0` via Swatinem/rust-cache. | No change |
| **zccache** | 19-crate workspace, but dev (opt 0) is default-on, so the classic big-workspace win is already banked. | Dev default-on; bench workflows pin `CARGO_INCREMENTAL: "0"`. | No change |
| **clud** | Dev default-on. | zccache excludes `-C incremental` from its cache key, so incremental doesn't fragment the shared cache. | No change |
| **running-process** | 9-crate workspace; dev default-on. | `ci/reproducible.py` correctly forces `CARGO_INCREMENTAL=0` (incremental artifacts are not stable across runs). | No change |
| **ai-tools** | Single effective crate; little to share even at opt 2/3; release is `lto = "thin"`. | Defaults everywhere. | No change |
| **FastLED** | C++/Arduino — neither lever applies. Its one Rust tool (`ci/lint_cpp_rs`) deliberately builds with the dev profile + `[profile.dev.package."*"] opt-level = 1`, which already gets both defaults. | — | No change |

Audited via GitHub API reads of each repo's root `Cargo.toml`,
`.cargo/config.toml`, and workflow files; full detail in
[soldr#1505](https://github.com/zackees/soldr/issues/1505).
