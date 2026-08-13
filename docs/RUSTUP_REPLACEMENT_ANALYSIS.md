# Rustup Replacement Research

Status: research-only.
Tracking issue: [#235](https://github.com/zackees/soldr/issues/235).
Predecessor: [#139](https://github.com/zackees/soldr/issues/139) (runtime decoupling, closed).

This document answers a single product question: should `soldr`
subsume `rustup`'s job (toolchain provisioning), keep relying on it,
or land somewhere in the middle?

The recommendation is at the bottom (TL;DR: **hybrid, native fetcher
for the common case, keep `rustup` as a deferred fallback**). The
intermediate sections show the work.

---

## 0. Where soldr stands today

Before evaluating replacement, the current relationship needs to be
stated precisely, because it is not "soldr requires rustup":

- **Runtime resolution**: `crates/soldr-core/src/lib.rs::probe_toolchain_binary`
  walks `RUSTUP_HOME/toolchains/<single>/bin` → `CARGO_HOME/bin` → `PATH`
  and only falls back to `rustup which` if no direct binary is found.
  This means a runner that already has `rustc`/`cargo` on disk does
  not need `rustup` to be a callable shim.
- **Provisioning**: `.github/actions/setup-soldr/ensure_rust_toolchain.py`
  does require `rustup`. If `rustup` is not on the runner it downloads
  `rustup-init` from `https://static.rust-lang.org/rustup/dist/...`,
  installs it with `--default-toolchain none`, then drives a normal
  `rustup toolchain install <channel> --profile <profile> --component ... --target ...`.
- **Trust**: `soldr-fetch::trust` enforces SHA-256 pinning for
  ecosystem tools. The toolchain bytes installed by `rustup` sit
  outside that policy. We trust whatever `rustup` decides to fetch.

So the gap is narrow but real: the *bootstrap* step is rustup-shaped
even though the *resolution* step is not.

---

## 1. Surface area audit — what `rustup` does that we would have to subsume

This is the canonical rustup feature set, mapped against how often
soldr's actual users need each one.

### 1.1 Core (any replacement must handle)

- **Channel resolution**: `stable`, `beta`, `nightly`,
  date-pinned `nightly-YYYY-MM-DD`, and exact versions `1.95.0`.
  Mechanism: HTTP GET on
  `https://static.rust-lang.org/dist/channel-rust-<channel>.toml`
  (for fixed versions, `channel-rust-1.95.0.toml`).
- **Component selection**: `rustc`, `cargo`, `rust-std`, `rustfmt`,
  `clippy`, `rust-src`, `llvm-tools-preview`, `rust-analyzer`,
  `rust-docs`, `rustc-dev`, `miri` (nightly only), `rust-mingw`
  (Windows-GNU only). The manifest's `[pkg.<name>.target.<triple>]`
  table tells you whether a component exists for a given host/target.
- **Cross-compile target installation**: `rust-std-<triple>` per
  manifest; required so cargo can link for non-host targets.
  Common ones: `x86_64-unknown-linux-musl`,
  `wasm32-unknown-unknown`, `wasm32-wasi`, `aarch64-apple-darwin`,
  `aarch64-pc-windows-msvc`.
- **Profile resolution**: `minimal` = `rustc + cargo + rust-std`,
  `default` = minimal + `rustfmt + clippy + rust-docs`, `complete`
  = everything. These are *rustup-side* expansions, not properties
  of the manifest itself. Any replacement must hard-code or re-derive
  these expansions (rustup's `src/dist/profile.rs` is authoritative).
- **`rust-toolchain.toml` parsing**: see section 3.
- **Manifest-version skew tolerance**: the `manifest-version`
  field is currently `2`. We need to refuse to silently consume
  `manifest-version = 3` if/when it ships.

### 1.2 Convenience (could be deferred / not implemented)

- **`+toolchain` shorthand** (`cargo +nightly build`, `rustc +1.85 --version`):
  rustup intercepts argv[1] starting with `+`. If soldr owns the
  cargo front door this is mechanically easy to implement
  (`soldr cargo +nightly build` → resolve `nightly` toolchain,
  set `RUSTUP_TOOLCHAIN=nightly`, exec cargo). But the symbol `+`
  is convention from rustup; we do not have to support it on day
  one if we ship a `--toolchain` flag instead.
- **`rustup self update`**: irrelevant if soldr is the manager —
  soldr already has its own release loop (PyPI / npm / GitHub Releases).
- **`rustup target list` / `rustup component list`**: discovery UX,
  not required for builds, can be a follow-up.
- **`rustup doc`**: opens local rust-docs in browser. Punt entirely.
- **`rustup override set <toolchain>`** (per-directory overrides
  not in `rust-toolchain.toml`): stored in `~/.rustup/settings.toml`.
  Edge case; can punt or implement later as
  `~/.soldr/overrides.toml`.
- **`rustup completions`**, `rustup man`: not needed.
- **Multi-toolchain coexistence**: rustup supports many toolchains
  installed side-by-side and switches by env var / shim. soldr would
  need at minimum `~/.soldr/toolchains/<triple>-<channel>/...`
  layout to keep a `nightly-2025-12-01` and a `1.95.0` installed
  simultaneously without re-downloading.

### 1.3 Things rustup does that we explicitly should NOT do

- **PATH shim binaries** (`~/.cargo/bin/cargo`, `~/.cargo/bin/rustc`
  etc., all symlinks to `rustup`): incompatible with soldr's stated
  "frozen built-in commands" rule and adds first-run latency for
  every `cargo` invocation. soldr's `probe_toolchain_binary` already
  prefers real binaries over shims; staying that way is correct.
- **Self-update**: soldr ships its own update story.
- **`rustup which` as the resolution oracle**: already removed by #139.

### 1.4 Volume estimate

Of the surface area above, the "core" set (1.1) is roughly:

- 1 manifest fetcher + parser (TOML, ~30 known fields).
- 1 component/target installer (decompress xz tarball, lay out
  files under a toolchain root).
- `rust-toolchain.toml` parser (already partially implemented in
  `soldr-core`, only `targets` is consumed today).
- Profile expansion table (~10 lines of constants).

That is small. The hard part is not LOC — it is matching rustup's
*behavior* on edge cases (renamed components,
historical-channel-name fallbacks, partial install recovery).

---

## 2. Distribution channel mechanics — `static.rust-lang.org`

### 2.1 Manifest format

- URL pattern:
  `https://static.rust-lang.org/dist/channel-rust-<channel>.toml`
  where `<channel>` is `stable`, `beta`, `nightly`, an exact
  version `1.95.0`, or a dated nightly `nightly-2025-12-01`.
- Format: TOML, `manifest-version = "2"`, `date = "YYYY-MM-DD"`.
- Top-level keys we care about:
  - `[pkg.<component>]` blocks with `version` and a nested
    `[pkg.<component>.target.<triple>]` table containing:
    - `available = true|false`
    - `url`, `hash` (sha256 hex), `xz_url`, `xz_hash`
    - newer entries also carry `zst_url`/`zst_hash`.
  - `[renames.<old>]` mapping for renamed components.
- Stability: format is on `manifest-version = 2` since 2017. The
  rustup project treats unknown keys as ignorable, and added
  `zst_*` fields without a version bump. Practical risk of breaking
  format change is low (single-digit-years horizon), but a future
  bump to v3 is plausible.

### 2.2 Artifact layout

Each component on each target is shipped as a single archive
(e.g. `rust-std-1.95.0-x86_64-unknown-linux-gnu.tar.xz`). Inside:

```
rust-std-1.95.0-x86_64-unknown-linux-gnu/
  components            # plain text list
  install.sh            # rustup historically used this
  rust-installer-version
  rust-std-x86_64-unknown-linux-gnu/
    lib/rustlib/x86_64-unknown-linux-gnu/{lib,...}
    manifest.in
```

Notably, *we do not need to run `install.sh`*. Each component's
`manifest.in` lists `file:relative/path/inside/component-dir`
entries. A native fetcher just iterates that list and copies
files into the toolchain root. rustup's own `dist::installer`
module is the reference.

### 2.3 Signatures

Manifests are also published with `.sha256` and `.asc` (PGP)
sidecars. Per-archive `.asc` and `.sha256` files exist too.
The signing key is the [Rust release signing key].

For trust parity:

- `.sha256` from manifest field is enough if you trust the manifest
  fetch over TLS. Same model rustup uses by default.
- `.asc` PGP verification is *optional* in rustup
  (`--no-self-update --insecure` etc. don't disable it; it's just
  not the default verification path for dist tarballs). Adding PGP
  is a security upgrade, not a parity requirement.
- soldr's existing `SOLDR_TRUST_MODE=strict` policy can be extended:
  toolchain components participate in the same `SOLDR_CHECKSUMS_FILE`
  pinning, with checksums seeded from the manifest hash.

### 2.4 Frequency / breakage history

- Stable: every 6 weeks.
- Beta: weekly-ish.
- Nightly: every UTC midnight.
- Format-breaking changes since 2017: ~zero. Additive only
  (`zst_*` for zstd transition, `extensions` for components).

---

## 3. `rust-toolchain.toml` semantics (in scope)

The full schema is documented in [rustup-book#rust-toolchain]. The
file is in TOML and may also appear as plain `rust-toolchain` (no
extension) containing a single channel string.

Schema:

```toml
[toolchain]
channel    = "1.95.0"           # required if no `path`
profile    = "minimal"          # optional, default "default"
components = ["rustfmt", "clippy"]
targets    = ["wasm32-unknown-unknown", "x86_64-unknown-linux-musl"]
path       = "/abs/path"        # mutually exclusive with channel
```

soldr-core already parses `targets` for triple detection. A
replacement has to additionally consume `channel`, `profile`,
`components`, and `path` (path mode means "use this toolchain
on disk; do not install anything").

**Walk-up rules** (from rustup source `src/toolchain/distributable.rs`,
`config::find_override_config`):

1. Walk `cwd` upward until a `rust-toolchain.toml` *or* `rust-toolchain`
   file is found; first hit wins.
2. If absent, fall back to the active default toolchain
   (`rustup default ...`, stored in `~/.rustup/settings.toml`).
3. Environment variable `RUSTUP_TOOLCHAIN` overrides everything.

soldr already implements step 1's directory-walk pattern in
`find_in_ancestors`. Steps 2 and 3 would need a soldr equivalent
(`~/.soldr/default-toolchain` or a field in `~/.soldr/config.toml`).

**Ambiguous case to match**: when `rust-toolchain.toml` references
a toolchain not yet installed, rustup installs it on first cargo
invocation. soldr would have to do the same — and `RUSTC_WRAPPER`
re-entry must not race the install. The cache daemon already
serializes per-key work and is the right enforcement point.

---

## 4. Hybrid options

The space is not binary. Three serious mid-points exist:

### Option A — "Vendor `rustup-init` only"

What changes: nothing functional. We download `rustup-init` from
`static.rust-lang.org/rustup/dist/...` (already do this), pin its
SHA-256 in `SOLDR_CHECKSUMS_FILE`, and document `rustup` as an
implementation detail.

Pros: zero engineering. Closes the trust gap on the bootstrap binary.
Cons: doesn't reduce CI step count; doesn't fix #140-class
on-demand install brittleness; "single binary, owns the front door"
pitch still has an asterisk.

### Option B — "Native dist fetcher for the common case, rustup fallback"

What changes: when soldr needs a toolchain (either `setup-soldr`
provisioning or a runtime miss against `rust-toolchain.toml`), it
fetches the manifest and component archives directly from
`static.rust-lang.org/dist`. Layout under `~/.soldr/toolchains/<channel>/`.
Components and targets handled if they appear in the manifest. If
the request involves something the native fetcher does not handle
(unusual nightly profile, exotic component), fall back to invoking
`rustup` if it exists, else fail with a clear message.

Pros:
- Single HTTP fetch + extract for the 95% case (`channel = "1.95.0"`,
  profile minimal, no extras).
- No `rustup-init` step in CI; one fewer subprocess generation.
- Toolchain bytes get the same `SOLDR_TRUST_MODE` treatment as
  ecosystem tools.
- #140 fix-out-of-the-box: no on-demand "rustup install component"
  race because component install is a discrete soldr step.

Cons:
- New code to maintain (~1500 LOC realistically, including tests).
- Have to track manifest schema drift.
- Edge cases (renames, dated nightlies missing components) need
  rustup as escape hatch indefinitely, so we don't fully eliminate
  the dependency for advanced users.

### Option C — "Full replacement"

What changes: B, plus implement profile expansion, `rust-toolchain.toml`
override semantics, multi-toolchain coexistence, `rustup default`
equivalent, `+toolchain` shorthand, and explicitly drop the `rustup`
fallback.

Pros: clean architectural story; soldr is genuinely the front door.
Cons: long tail of "rustup did this one weird thing" we'll
re-discover for years; UX regression risk for users with established
rustup workflows (`rustup override set`, `rustup component add` from
shell scripts); large maintenance commitment; no clear user demand
for parity beyond what B provides.

### Option D — "Keep rustup, document the dependency"

Status quo. Add explicit note in DESIGN.md.

Pros: zero work. rustup is well-maintained.
Cons: pitch stays asterisked; trust gap remains; #140-class
brittleness remains.

---

## 5. Cost estimate (if we go Option B)

PR sequence, rough complexity per step (S = ~1 day, M = a few days, L = a week+):

1. **`soldr-toolchain` crate skeleton** (S). New crate under
   `crates/`, depends on `soldr-core` for trust types.
2. **Manifest fetch + parse** (M). HTTP GET `channel-rust-<channel>.toml`,
   TOML deserialize, surface `pkg/component/target/url+hash` lookups.
   Cache manifests under `~/.soldr/cache/manifests/` keyed by URL+date.
3. **xz/tar extraction with manifest-driven file layout** (M). Stream
   `.tar.xz` from the URL, walk `manifest.in`, copy to target
   toolchain dir under `~/.soldr/toolchains/<channel>/`. Reuse soldr-fetch's
   sha256 verification.
4. **Profile expansion table + channel resolver** (S). Hard-code
   `minimal`/`default`/`complete` -> component list. Channel string
   parser: stable / beta / nightly / `nightly-YYYY-MM-DD` / `X.Y.Z`.
5. **`rust-toolchain.toml` consumer** (M). Extend soldr-core's
   parser to read `channel`, `profile`, `components`. Wire the
   walk-up resolver into `setup-soldr` and into the runtime
   `RUSTC_WRAPPER` miss path.
6. **`setup-soldr` switchover** (S). `ensure_rust_toolchain.py`
   becomes a thin wrapper around `soldr toolchain install`. Keep
   the rustup fallback behind `SETUP_SOLDR_USE_RUSTUP=1` for one
   release cycle.
7. **Trust integration** (S). Toolchain component sha256s flow
   through the same `SOLDR_CHECKSUMS_FILE` mechanism as ecosystem
   tools. Document the new keys.
8. **Cross-platform validation matrix** (M). CI job that installs
   stable, nightly, `1.95.0`, and a dated nightly on linux-gnu,
   linux-musl, macos-arm64, windows-msvc. Confirm `cargo build`
   works in each.
9. **Docs + DESIGN.md update** (S). Amend "Not a Rust toolchain
   manager" → "Optional toolchain manager via `soldr toolchain ...`".
10. **Deprecation of rustup fallback** (S, ~1 release later).
    Remove the env-var escape hatch once telemetry / community
    feedback is clean.

Total: roughly 3 weeks of focused work, spread across ~10 PRs.

**Largest unknowns**:
- Windows MSVC linker behavior when `rust-mingw` is required for
  GNU targets — easy to mishandle.
- Component renames mid-channel (the `[renames]` table) — we have
  to honor it or installs silently miss `rust-std`.
- Daemon serialization of "first build sees missing toolchain,
  install it, don't double-install" — easy to race.
- Per-directory override semantics. rustup uses
  `~/.rustup/settings.toml` *and* an in-tree `rust-toolchain.toml`.
  Order of precedence has to be matched exactly or scripts break.

---

## 6. Comparable prior art

### `uv`

`uv` (Astral) explicitly *does* manage Python interpreters as of
its v0.4 series via `uv python install`. It downloads pre-built
interpreters from
[python-build-standalone](https://github.com/astral-sh/python-build-standalone)
rather than building from source. Layout under
`~/.local/share/uv/python/`. This is the closest precedent for
what soldr would do: a tool that started "downstream of the language
manager" and absorbed it. Notable: uv ships `uv python pin
<version>` which writes a `.python-version` file (`rust-toolchain.toml`
analog). uv's own users mostly do not realize uv is doing this; it
just works.

### `nvm` / `fnm` / `volta`

Node ecosystem already has multiple toolchain managers. `volta`
in particular is interesting: it auto-installs a project's pinned
Node version on first command, transparently. soldr's daemon-driven
"miss → install → resume build" path for `rust-toolchain.toml`
would be the same shape.

### Cost to upstream rustup if we drop the dependency

Approximately zero. rustup is a stable, mature, low-maintenance
project with thousands of users outside the soldr ecosystem.
Whether soldr uses it has no bearing on its release cadence or
maintenance burden. We are not "competing" with rustup; we are
considering whether it is the right *bootstrapping* dependency
for our specific automation surface.

---

## 7. Open questions (from the issue)

> Does the trust story actually demand replacing rustup, or is it
> sufficient to verify the rustup-init binary itself and trust
> rustup's downstream pinning?

Pinning `rustup-init` closes the *bootstrap* gap. It does not
close the *runtime* gap: every `rustup toolchain install` still
fetches manifests and tarballs over TLS without `SOLDR_CHECKSUMS_FILE`
participation. If the goal is "every byte that ends up running
on the runner is in our pin file", we need Option B or C. If the
goal is "the entry point is verified", Option A is enough.

> Is `+toolchain` shorthand a hard requirement, or can soldr punt on it?

Punt for v1. Replace with `soldr cargo --toolchain nightly build`
or `RUSTUP_TOOLCHAIN=nightly soldr cargo build`. The `+` syntax
is a rustup convention, not a Cargo convention; cargo itself does
not handle `+toolchain` — rustup intercepts argv before exec.

> What's the maintenance cost of tracking rust-lang.org manifest
> format changes long-term? rustup absorbs that today.

Low but nonzero. Format is `manifest-version = 2` since 2017 with
only additive changes. Realistic estimate: 1 to 2 small PRs per
year of "support new optional field", plus a single larger PR if/when
manifest v3 ships (currently no proposal exists). The fallback path
(invoke rustup if our parser bails) bounds the worst case.

---

## 8. Recommendation: **Option B (hybrid, native fetcher with rustup fallback)**

Rationale:

1. **Trust parity matters more than UX parity.** soldr's strongest
   product claim is end-to-end SHA-256-verified execution. Today
   that claim has a hole the size of `rustc` itself. Option B
   closes that hole for the 95% case (channel + profile + maybe a
   component or two) without committing to chase rustup's long
   tail of override semantics.
2. **CI brittleness is real.** Issue #140 is concrete evidence
   that on-demand `rustup component add` mid-build can fail. A
   discrete `soldr toolchain install` pre-step removes that class
   of failure.
3. **Cost is bounded.** ~3 weeks of work, spread across ~10 small
   PRs, with a `rustup` escape hatch behind an env var for the
   first release cycle. Nothing here blocks reverting if the
   manifest format ever does break.
4. **Precedent is favorable.** `uv` made the same move from
   "lean on system Python" to "manage Python interpreters" without
   user backlash, and arguably became more useful because of it.
5. **Full replacement is overshoot.** `+toolchain` shorthand,
   `rustup override set`, multi-toolchain housekeeping, and
   `rustup self update` parity buy nothing for soldr's actual
   user base (CI plus a small set of devs). Cutting them shrinks
   maintenance load without cutting trust coverage.

### Consequence (per acceptance criteria)

DESIGN.md line 26 ("Not a Rust toolchain manager") is *amended*
in this PR to flag that the position is under active reconsideration
and points at this document. The line is not yet flipped to
"is a toolchain manager" because that change should land in the
PR that implements step 1 of the sequence in section 5.

### Concrete next step

File a tracker issue "Native rust toolchain installer (hybrid)"
and break it into the 10 sub-PRs from section 5. The first PR is
small enough to ship in a single afternoon (`soldr-toolchain`
crate skeleton + manifest fetch + parse).

---

## References

- rustup book: <https://rust-lang.github.io/rustup/>
- rust-toolchain file: <https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file>
- dist server protocol: <https://github.com/rust-lang/rustup/blob/master/doc/dev-guide/dist.md>
- `uv` Python install: <https://docs.astral.sh/uv/concepts/python-versions/>
- soldr issue #139 (runtime decoupling, closed predecessor)
- soldr issue #140 (on-demand rustup install brittleness)
- soldr issue #235 (this research)

[Rust release signing key]: https://static.rust-lang.org/rust-key.gpg.ascii
[rustup-book#rust-toolchain]: https://rust-lang.github.io/rustup/overrides.html#the-toolchain-file
