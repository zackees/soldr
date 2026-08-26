# soldr API Reference

This file is the CLI reference for `soldr`.

For the product-level support contract about what counts as a supported external API, see [API_BOUNDARY.md](./API_BOUNDARY.md).

## Overview

soldr is a single front door for Rust tool execution and Rust builds.

It has four invocation modes:

1. `soldr cargo ...`
   Delegates to the real Cargo binary while routing cacheable compiler work to
   the zccache service embedded in `soldr-daemon`.
2. `soldr <tool> [args...]`
   Fetches and runs a Rust CLI tool binary.
3. `soldr rustc ...`
   Low-level passthrough wrapper mode for explicit `RUSTC_WRAPPER=soldr` usage.
4. `soldr cc ...` / `soldr c++ ...`
   Compile native source through a catalogue-backed compiler and sysroot.

The primary user experience is `soldr cargo ...`.

## Machine-Facing Support Level

Current support policy:

- the supported external integration surface is invoking the `soldr` executable through documented commands and flags
- the internal Rust crates are not a supported public API
- wrapper mode and internal environment variables are operational mechanics, not a general-purpose API contract
- human-oriented command output is not the stable machine-facing protocol
- the first stable machine-facing protocol is the JSON mode on selected commands documented below

---

## Invocation Modes

### Mode 1: Cargo Front Door

```bash
soldr cargo build --release
soldr cargo test
soldr cargo run -- --help
```

Behavior:

- Resolve Cargo and Rust tools through Soldr's front door. Host-owned binaries
  retain the caller's `CARGO_HOME` / `RUSTUP_HOME`; binaries from Soldr's
  managed toolchain receive the matching managed context.
- Fall back to `rustup which <tool>` when no matching binary is present in the
  selected toolchain location.
- Set `RUSTC_WRAPPER` to a compiler-named Soldr shim when caching is enabled.
- Register the exact `soldr-daemon` image and selected root with the singleton
  broker before Cargo starts. Only the broker places and starts that image.
- Route cacheable compiler work through the broker SESSION service to the zccache
  service compiled into `soldr-daemon`; no separate zccache binary or daemon
  is fetched.
- Start a per-build correlation session while keeping cache artifacts under
  the exact selected Soldr root.
- Delegate to Cargo with the exact flags the user passed.

Current cache behavior:

- caching is enabled by default for `soldr cargo ...`
- `--zccache=managed` and the legacy `--zccache=system` spelling are retained
  for CLI compatibility; both use the embedded runtime. To deliberately put an
  external compiler cache in Cargo's wrapper slot, use
  `SOLDR_RUSTC_WRAPPER=/path/to/zccache soldr cargo ...`.
- zccache integration currently targets Rust builds through the cargo front door
- session logs and archived reports live under
  `<soldr-root>/cache/zccache/history/`; the embedded service's versioned
  state and compile journal live under
  `<soldr-root>/cache/zccache/daemon-state/embedded-v1/v<VERSION>/`.
  `SOLDR_CACHE_DIR` moves this compiler-store boundary; `ZCCACHE_CACHE_DIR`
  affects auxiliary front-door/session, rust-plan, and rustfmt state, not the
  service hosted by `soldr-daemon`. Soldr neither uses nor sweeps the
  standalone `~/.zccache` root
- toolchain binaries (`rustc`, `rustfmt`, `clippy-driver`, etc.) are resolved directly from `RUSTUP_HOME` / `CARGO_HOME` / `PATH` before any `rustup` call; `rustup which` is only used as a fallback when the direct probe fails. The sole exception is when `RUSTUP_TOOLCHAIN` is explicitly set to a non-empty value — in that case soldr skips the direct probe and asks `rustup` for the matching toolchain binary so the pinned channel always wins
- once a concrete Cargo or Rust tool has been resolved, Soldr propagates its managed `CARGO_HOME` / `RUSTUP_HOME` only when that binary came from the Soldr-managed toolchain. Host-owned binaries keep the caller's host toolchain context instead of being paired with Soldr's default-less managed Rustup home

This is the normal build entry point.

### Prefer a newer global Soldr

Projects can opt in to using an installed global Soldr when it is newer than
the checkout-local executable. Add this to the root manifest:

```toml
[workspace.metadata.soldr]
prefer_newer_global = true
```

Soldr resolves the first different `soldr` executable on `PATH`, probes its
`--version`, and delegates only when its SemVer version is strictly higher.
Failures to locate or probe a global executable leave the local invocation
unchanged. Explicit `soldr --as <version>` / `SOLDR_AS` pins remain
authoritative.

### Mode 2: Tool Fetcher

```bash
soldr <tool>[@<version>] [tool-args...]
```

Examples:

```bash
soldr maturin build --release
soldr cargo-dylint check
soldr rustfmt src/main.rs
soldr maturin@1.7.0 build
```

Rustfmt remains routed through Soldr. Because rustfmt recursively discovers
child modules that are absent from Cargo's explicit rustfmt argv, normal
recursive invocations always execute the real formatter. The embedded marker
shortcut is used only when the invocation explicitly sets
`skip_children=true`, making the source-file set complete and safe to cache.

Resolution order:

1. **Cargo verb shorthand** — if `<tool>` (with no `@<version>` suffix) is either a cargo subcommand soldr already prebuilds (`nextest`, `deny`, `audit`, ...) OR one of cargo's own first-party verbs (`build`, `test`, `clippy`, ...), the invocation is rewritten as `soldr cargo <tool> [args...]` and dispatched through the cargo front door. See [Cargo Verb Shorthand](#cargo-verb-shorthand) below for the full list.
2. Local cache in `~/.soldr/bin/`
3. crates.io repository lookup
4. GitHub Releases for that repository

Current implementation note:

- the broader binstall/QuickInstall/`cargo install` fallback chain is planned behavior, not the current shipped fetch path

#### Cargo Verb Shorthand

When `soldr <verb> [args...]` is invoked and `<verb>` is not a frozen soldr built-in, the External arm rewrites the invocation as `soldr cargo <verb> [args...]` whenever `<verb>` matches one of two lists. The long form `soldr cargo <verb>` continues to work — the shorthand is purely additive.

**Routed cargo subcommands** (soldr prebuilds these via `KNOWN_TOOLS`):

`nextest`, `deny`, `audit`, `llvm-cov`, `udeps`, `semver-checks`, `expand`, `watch`, `chef`, `zigbuild`, `xwin`, `binstall`, `machete`

**Routed cargo built-in verbs** (cargo's first-party commands):

`build`, `test`, `check`, `run`, `bench`, `doc`, `fmt`, `clippy`, `tree`, `update`, `fix`, `add`, `remove`, `metadata`, `pkgid`, `search`, `vendor`, `yank`, `owner`, `login`, `logout`, `init`, `new`, `generate-lockfile`, `verify-project`, `locate-project`, `report`, `install`, `uninstall`, `publish`

**Collision policy.** These three verbs MUST stay anchored to their soldr-native meaning and are explicitly excluded from the shorthand:

| Verb       | soldr-native meaning                              | Cargo equivalent (use long form) |
|------------|---------------------------------------------------|-----------------------------------|
| `clean`    | Clear the embedded zccache build cache            | `soldr cargo clean`               |
| `config`   | Show or set soldr configuration                   | `soldr cargo config` (unstable)   |
| `version`  | Print soldr's version                             | `soldr cargo --version`           |

The borderline case `install`: bare `soldr install <crate>` routes to
`cargo install <crate>`. The former standalone `soldr install-zccache`
surface was removed when zccache became a compiled-in service.

**Version pinning skips the shorthand.** Cargo built-in verbs and registered cargo subcommands cannot be version-pinned via the bare `@<version>` form — the cargo front door has no per-invocation version knob. `soldr build@1.0` keeps the existing External fetch path (and errors with "no crate named build"), and so do the registered subcommands (`soldr nextest@0.9.x` falls through to External). For pinned cargo-subcommand versions, use the soldr registry (`KNOWN_TOOLS::pinned_version` in source); for pinned tool fetches, use the External path for crates that actually exist.

```bash
# Shorthand
soldr build --release          # == soldr cargo build --release
soldr test --workspace         # == soldr cargo test --workspace
soldr clippy -- -D warnings    # == soldr cargo clippy -- -D warnings
soldr fmt --all -- --check     # == soldr cargo fmt --all -- --check
soldr nextest run              # == soldr cargo nextest run
soldr zigbuild build --target ...  # == soldr cargo zigbuild build --target ...

# Long form (always works as the escape hatch)
soldr cargo clean              # explicitly route `clean` to cargo (NOT soldr's cache clean)
soldr cargo config get profile.dev
```

### PEP 517 Build Backend

soldr ships a PEP 517 build backend (`src/soldr/__init__.py` in the
wheel), so Rust+Python packages can build through soldr instead of
raw maturin:

```toml
[build-system]
requires = ["soldr"]
build-backend = "soldr"
```

The existing `[tool.maturin]` configuration is honored unchanged — the
backend delegates to `soldr maturin pep517 <hook>` with a pinned
maturin. The dispatch pins the child's toolchain before exec:

| Pin | Effect |
|---|---|
| `CARGO` / `RUSTC` | rustup-resolved cargo + its sibling rustc — `rust-toolchain.toml` wins over PATH-shadowing standalones (chocolatey/scoop GNU installs). |
| `CARGO_BUILD_TARGET` | runtime MSVC-default triple on Windows (same policy as the cargo front door). |
| `CMAKE` / `CMAKE_GENERATOR=Ninja` | managed cmake + ninja from the soldr toolchain archive for cmake-based `*-sys` crates. |
| `RUSTC_WRAPPER=soldr` | compilation caching. The Python backend sets this before calling `soldr maturin pep517`; direct `soldr maturin build` / `develop` auto-inject it when cache is enabled and `RUSTC_WRAPPER` is unset. |

For a project whose packaging logic is not maturin, soldr can wrap another
PEP 517/660 backend while keeping the same managed environment:

```toml
[build-system]
requires = ["soldr", "setuptools>=64"]
build-backend = "soldr"

[tool.soldr.pep517]
delegate-backend = "setuptools.build_meta"
```

All standard wheel, editable, metadata, sdist, and dynamic-requirements hooks
are forwarded to the delegate. The delegate runs with soldr's target/profile/
linker/cache environment, and the caller environment is restored after each
hook. Maturin remains the default when `delegate-backend` is absent; recursive
delegation back to `soldr` is rejected.

The `profile`, `--profile`, and (for editable hooks) `editable-profile` PEP
config settings are also applied to delegated builds. For example,
`pip install . --config-settings profile=release` selects an explicit release
profile; without an explicit setting, the fast local `dev` policy remains in
effect.

For local PEP 517 wheel and editable builds, the backend defaults to Cargo's
`dev` profile and sets `opt-level = 0`, `codegen-units = 256`,
`debug = "line-tables-only"`, `lto = false`, and `incremental = true` for
every field the project has not explicitly configured in `profile.dev`. This
keeps manual installs fast; release pipelines should continue to select an
explicit release profile. A project's
`[tool.maturin] profile` / `editable-profile`, PEP config settings, or the
`SOLDR_PEP517_PROFILE` environment variable are authoritative. Set
`SOLDR_PEP517_PROFILE=none` to preserve maturin's profile selection entirely.

On every successful wheel or editable build, the backend writes a concise
timing and cache summary to stderr. `SOLDR_PEP517_STATS=off` disables it;
`SOLDR_PEP517_STATS=full` prints the complete cache-session payload as a
second stderr line. Soldr also selects `full` when `PIP_VERBOSE` or
`UV_VERBOSE` is set by the frontend or caller.

### PEP 517 callers with a wall-clock timeout

The compile-reply backstop defaults to 30 minutes so a legitimate large
release compile is not cut off. A hook, editor, or CI harness with a shorter
wall-clock timeout should set `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` to a smaller
value than its own timeout before invoking the PEP 517 frontend. That makes
Soldr report an actionable daemon-timeout diagnostic before the harness kills
the backend; it does not change the default for ordinary `pip` or `uv` builds.

```bash
# A five-minute hook timeout: leave time for Soldr to report the failure.
SOLDR_COMPILE_REPLY_TIMEOUT_SECS=240 uv build
```

If a harness sends `SIGTERM`, `SIGBREAK`, or `SIGINT` first, the backend also
names the interrupted command and elapsed time. A hard process kill cannot be
intercepted, so the explicit timeout remains the reliable diagnostic boundary.

For wheel and editable hooks, soldr keeps the last successful artifact under
`<effective-soldr-root>/pep517/wheels/`. It performs a metadata-only recursive scan of the
project (relative path, size, and modification time), staged files included,
and also hashes supplied prepared metadata by content. If that fingerprint and
the build settings match, soldr hardlinks the cached wheel into the frontend's
output directory (copying only when hardlinks are unavailable) and skips the
backend's packaging/compression work. Set `SOLDR_PEP517_WHEEL_CACHE=off` to
disable reuse.

The local PEP 517 path also sets `SOLDR_PEP517_LINKER=auto` by default. Soldr
tries the fastest supported linker for the active target, retries once with
the platform linker only for a linker-availability failure, and records that
successful fallback in versioned state under the soldr cache. Equivalent later
builds skip the failed candidate and warn that the standard linker was
previously verified. Set `SOLDR_PEP517_LINKER=none` to disable the automatic
policy. A user-specified `SOLDR_LINKER=fast` remains explicit and never
silently falls back; it emits an actionable warning instead. Target-specific
linker or rustflags settings from the command environment or project
`.cargo/config.toml` retain precedence.
Every pin defers to a pre-set user env var. Maturin acquisition is a
ladder controlled by `SOLDR_MATURIN_PROVISIONER` (`auto` default:
pinned prebuilt binary from GitHub Releases, falling back to the PyPI
maturin wheel provisioned into an isolated uv-managed env under
`~/.soldr/bin/maturin-uv-<ver>/`; `binary` and `uv` force one rung).
Direct `soldr maturin ...` preserves caller-provided `CARGO`, `RUSTC`,
and `RUSTC_WRAPPER`; `SOLDR_RUSTC_WRAPPER` only controls Soldr's
auto-injected wrapper when `RUSTC_WRAPPER` is unset.

Target resolution is shared by direct maturin and the PEP 517 backend,
in this precedence order: an explicit `--target` argument (including
PEP 517 config settings named `target`, `--target`, or `build-target`),
`CARGO_BUILD_TARGET`, `[tool.maturin].target`, then the host triple.
Before maturin starts, soldr applies the same target OS SDK preparation
used by `soldr build`, target clippy, and target test compilation. The Python
backend and the child build also share the same `CARGO_TARGET_DIR`.

`soldr env --target <alias-or-triple> --json` includes an additive
`target_plan` object (`schema_version: 1`) for setup-soldr and other tooling.
It reports the canonical target and alias, stable toolchain family, concrete
compiler/linker/archiver programs, SDK/sysroot provider and cache identity,
sorted environment-key contract, and supported operations. Consumers should
invoke the soldr operation rather than reconstructing those environment
values themselves.

PyO3 configuration is resolved separately from the target OS SDK. Soldr
reads workspace dependency metadata and only injects
`PYO3_NO_PYTHON=1` for a proven cross-compiled ABI3 extension.
PyO3-free projects receive no Python variables; modern PyO3 Windows
extensions use raw-dylib without a Python import library; Unix/macOS
extensions keep PyO3/maturin's normal dynamic extension behavior.
Embedding, legacy, ambiguous, and non-ABI3 builds are never guessed to
be ABI3. Caller-provided `PYO3_*` values always win. `soldr env --target
... --json` includes the resolved `pyo3_plan`.

Set `SOLDR_PYO3_COMPATIBILITY=sysroot` to opt an older PyO3 or embedded
Python cross-build into managed target-Python assets. Only this plan mode
consults the Python rows in the toolchain catalogue. Soldr selects the
newest published version for the target, verifies its catalogue SHA-256,
and exports `PYO3_CROSS`, `PYO3_CROSS_LIB_DIR`,
`PYO3_CROSS_PYTHON_VERSION`, and `PYO3_CROSS_PYTHON_IMPLEMENTATION`.
`SOLDR_PYTHON_VERSION=X.Y.Z` requests an exact published version.
`SOLDR_SYSLIB_ASSET_ORIGIN` overrides the asset origin for mirrors and
controlled integration tests; the default remains the soldr-toolchain
assets branch.

This split follows PyO3's own cross-compilation contract: target Python is
optional for ABI3 extensions, `PYO3_CROSS_LIB_DIR` is only needed when the
output must link libpython or consume target interpreter configuration, and
Windows uses Rust `raw-dylib` linking without an import library. See the
[PyO3 0.29 cross-compilation guide](https://pyo3.rs/v0.29.0/building-and-distribution.html#cross-compiling)
and [raw-dylib configuration](https://pyo3.rs/main/building-and-distribution.html#the-pyo3_use_raw_dylib-environment-variable).

The backend also pins `CARGO_TARGET_DIR` to a stable per-project path under
`<effective-soldr-root>/cargo-target/pep517/<project-id>` so PEP 517 isolated builds
(pip/uv copy the sdist to a throwaway temp dir, discarding `target/`
after every build) keep Cargo's incremental cache hot across invocations.
The project ID is a content identity over build configuration (not Rust
source); Cargo remains authoritative for source freshness. This keeps
temporary source directories warm without sharing target state between
unrelated projects. `SOLDR_PEP517_PROJECT_ID` exposes the identity to soldr
diagnostics and cache keys. A caller-provided `CARGO_TARGET_DIR` always wins,
and `SOLDR_PEP517_STABLE_TARGET_DIR=0` disables the pin entirely. The effective
root is `SOLDR_CACHE_DIR` when set; otherwise the backend queries the selected
`soldr version --json` binary for its provenance-aware production (`.soldr`) or
development (`.soldr-dev`) root, then forwards that exact root to child hooks.
The owning daemon removes both PEP517 target namespaces and wheel-cache
namespaces after 30 days, or once they are older than four days while the
root/volume is under pressure. Active build leases defer the pass.

### Mode 3: Internal Wrapper Mode

Wrapper mode is entered when Cargo invokes soldr as the configured `RUSTC_WRAPPER`.

Typical shape:

```text
soldr /path/to/rustc --crate-name foo ...
```

In this mode, soldr should act as the transparent build-assistance layer around `rustc`.

Current implementation status:

- Wrapper mode still transparently resolves the real `rustc`, preferring direct binaries before `rustup`
- The normal cache-enabled build path now runs through soldr wrapper mode and delegates into managed `zccache`
- If caching is disabled, wrapper mode falls through to real `rustc` without zccache involvement

---

## Mode Detection

When soldr starts, it decides its mode in this order:

1. If `argv[1]` looks like `rustc` or a path to `rustc`, enter wrapper mode.
2. Otherwise, parse CLI commands with Clap.
3. `cargo` is a first-class built-in subcommand.
4. Any other first argument falls through to the External arm, which itself tries dispatch in order: cargo verb shorthand (see [Cargo Verb Shorthand](#cargo-verb-shorthand)) first; if no match, treat the argument as a tool name to fetch and run.

---

## Built-in Commands

### `soldr cc` / `soldr c++` (soldr#2335)

Compile and link C or C++ with a verified compiler/sysroot bundle from the
soldr-toolchain catalogue:

```bash
soldr cc --target x86_64-linux-gnu.2.17 hello.c -o hello
soldr c++ --target linux-x64 main.cpp -o app
```

`--target` accepts the regular friendly aliases and Rust triples and defaults
to `host`. The supported standalone targets are GNU/Linux, musl/Linux, and
`x86_64-pc-windows-gnu`; unsupported host/target pairs fail explicitly.
GNU/Linux uses the catalogue's glibc 2.17 sysroot whether the suffix is
explicit or omitted.

Arguments not consumed by the front door are forwarded verbatim to the
compiler. Exactly one of these query flags may be used instead of compiler
arguments:

- `--print-cc`
- `--print-cxx`
- `--print-ar`
- `--print-linker`

The query output is only the prepared executable path. Invoke the `soldr cc`
or `soldr c++` wrapper when the target's required sysroot arguments must be
applied. CMake supports that shape through its `CC` and `CXX` environment
variables:

```bash
CC="soldr cc --target x86_64-linux-gnu.2.17" \
CXX="soldr c++ --target x86_64-linux-gnu.2.17" \
  cmake -S . -B build
cmake --build build
```

### `soldr wheel` (soldr#2139)

Build an **abi3** Python wheel through soldr's blessed toolchain:

```bash
soldr wheel                                        # quick DEV wheel, host target
soldr wheel --release                              # release wheel, host target
soldr wheel --release --target aarch64-unknown-linux-gnu   # release cross wheel
soldr wheel --release --target linux-arm64         # friendly aliases resolve identically
soldr wheel --release --target mac-arm64 --out dist # later arguments reach maturin
```

`--release` is **opt-in**, matching `cargo` and `soldr build`: a bare
`soldr wheel` builds the dev profile, which is what you want while iterating.
`--target` is optional and defaults to the host triple.

`soldr wheel` resolves the target (same alias table as `soldr build`), prepares
the sysroot and toolchain environment, provisions maturin, and delegates to the
existing `soldr maturin build` execution path. Wheel *naming* is unchanged;
that contract is maturin's.

#### Platform tags: soldr only claims a floor it enforced

| invocation | `--compatibility` passed to maturin |
| --- | --- |
| `soldr wheel --release --target <cross *-linux-gnu>` | `manylinux_2_17` |
| `soldr wheel --release --target <cross *-linux-musl>` | `musllinux_1_2` |
| any host-target build (`--target` omitted or equal to the host) | `pypi` |
| any dev-profile build (no `--release`) | `pypi` |
| any non-Linux target | `pypi` |

`pypi` is maturin's pseudo-option for "derive the platform tag from the bytes,
then validate the filename for PyPI" — a description of the artifact rather
than a promise about it.

The distinction is not cosmetic. Target preparation
(`target_lifecycle::prepare_for_invocation`), which is what mounts the
catalogue sysroot that *creates* the 2.17 floor, runs on the maturin path only
when the target differs from the host. A host-target `*-linux-gnu` build links
against the machine's own glibc — 2.39 on ubuntu-24.04 — so a `manylinux_2_17`
tag there would be a claim nothing backed, and pip acts on tags: it installs
such a wheel on an old host and the program then dies with
``version `GLIBC_2.39' not found``. `.github/scripts/verify_wheel_glibc.py`
exists to catch exactly that, and the `wheel-cross-verify` CI lane runs the
whole path end to end on a cross target.

(maturin does not silently downgrade an explicit tag: with
`--compatibility manylinux_2_17` and an ELF needing GLIBC_2.39 its auditwheel
implementation fails the build with "Error ensuring manylinux_2_17
compliance". It auto-selects the highest satisfied policy only when no tag was
requested. So the previous unconditional claim did not ship broken wheels — it
broke `soldr wheel` for host-target Linux builds on any modern distro.)

Scope notes:

- **abi3 only.** A non-abi3 extension module needs a CPython built for the
  target, not just a sysroot. When soldr cannot place the build in an
  interpreter-free mode it refuses and names `soldr maturin build` as the
  escape hatch, rather than quietly building against the host's Python.
- **No glibc-floor suffix.** `--target <triple>.<major>.<minor>` (soldr#2202)
  is rejected here. A floor is a request to zig, never a guarantee — the
  effective floor is also bounded by every symbol the vendored C dependencies
  reference — so folding it into a manylinux tag would publish a promise soldr
  cannot keep.
- Pass `--target` once, before any forwarded maturin arguments. A second
  `--target` in the passthrough is refused rather than silently disagreeing
  with the sysroot soldr prepared.
- `--release` together with a forwarded `--debug` is refused: those are two
  different profiles, and picking one would build something you did not ask
  for.

### `soldr cargo`

Run Cargo through soldr's front door.

### Stable `-Zthreads` fallback

When a stable compiler rejects exactly `-Zthreads=<N>` with rustc's nightly-only
diagnostic, the Cargo front door retries once without that flag. The retry is a
fresh `soldr cargo` invocation, so its effective flags and cache/session plan
are rebuilt; artifacts from the failed flag set are never treated as
equivalent. Soldr emits a warning containing the removed value and notes that
the build may be slower. GitHub Actions receives the warning in
`::warning::` form, while supported local terminals render it in yellow.

This fallback is intentionally limited to one removable flag. It applies only
when `-Zthreads=<N>` comes from `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, or a
supported `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` environment variable, with no other
`-Z` flags present. It never sets or changes `RUSTC_BOOTSTRAP`, never retries a
normal compilation failure, and never retries a nightly invocation. If the
flag comes only from Cargo configuration, Soldr leaves the original failure in
place and asks the caller to remove it or provide it through one of the
supported environment variables.

```bash
soldr cargo build --release
soldr cargo test --workspace
soldr cargo check -p soldr-cli
```

For cross-target builds (`soldr cargo --target ...`), the target's Rust standard library must be provisioned separately — see the [native vs cross targets](../README.md#native-vs-cross-targets) section of the README.

### `soldr lint`

Run the repository validation suites through Soldr. The default and `rust`
suites run formatting, Clippy, and Dylint with one canonical workspace scope:

```bash
soldr lint
soldr lint rust --package soldr-cli
soldr lint deps
soldr lint ci
soldr lint ci --format json
soldr lint all
```

`deps` runs `deny check`, `audit`, and `machete` concurrently as cache-disabled
children because they do not compile Rust. `ci` runs the CI/build-surface policy
suite (see [`soldr lint ci`](#soldr-lint-ci) below). `all` runs the `ci` suite
first, then adds `--all-features`, `udeps`, and `semver-checks` after the standard
Rust and dependency suites. Compiler-bearing steps stay on the regular Soldr cache
lifecycle; `cargo-dylint` is fetched from its Linux GNU release asset or
source-built from the pinned registry version on Windows and macOS.

### `soldr ci-test`

Run Soldr's prescribed host-validation DAG with maximum artifact sharing inside
each compatible compiler domain:

```bash
soldr ci-test
soldr ci-test --package soldr-cli --features feature-a,feature-b
soldr ci-test --explain-plan
soldr ci-test --explain-plan --format json
```

The stable host chain runs formatting and `soldr lint ci`, Clippy, exactly one
Nextest test-profile build/run, and doctests. `soldr cargo check` is not run
because Clippy subsumes the same workspace/all-targets host scope. Dependency
policy fans out across `soldr cargo deny check bans`, `soldr cargo audit`, and
`soldr cargo machete`. Nextest runs from the workspace root, so
`.config/nextest.toml` keeps its test groups, slow-test budgets, timeout grace
period, and platform wrappers. By default the command fixes compiler work at
one Cargo/Soldr job and one Nextest test thread. Explicit
`CARGO_BUILD_JOBS`, `SOLDR_JOBS`, and `NEXTEST_TEST_THREADS` values are frozen
into the plan instead; this keeps memory bounded without conflating compiler
serialization with test-process concurrency.

All six repository Dylints are retained. They intentionally use their exact
pinned nightly rather than the stable project toolchain. The command reads all
six lint manifests, requires their pins to agree, and rejects an environment
override that selects another nightly. Before entering the Dylint domain it
self-provisions the catalogue-pinned `cargo-dylint`, `dylint-link`, and matching
prebuilt driver, so a caller does not need to install Dylint tooling separately.
Release-profile lint libraries share
`target/dylint/libraries/<nightly-host>`, workspace analysis uses
`target/dylint/target/<nightly-host>`, and UI tests share
`target/dylint/tests/<nightly-host>` through both `--target-dir` and
`CARGO_TARGET_DIR`. The command verifies all three trees and rejects material
artifacts in per-lint `dylints/*/target` directories. This exception preserves
Dylint correctness without causing stable Cargo fingerprints to flip between
toolchains.

Accepted scope selectors are `--package`/`-p`, `--features`,
`--all-features`, and `--no-default-features`. An explicit `--target` is
accepted only when it equals the detected host. Target-directory, profile,
toolchain, manifest, release, and cross-target overrides are rejected because
they would create an undeclared compile domain; use `soldr cargo ...` for those
intentional variants.

`--explain-plan` performs no compiler work. Its schema-version-1 JSON freezes
workspace metadata identity, toolchain/compiler identity, host target, target
directories, profile, scope/features, Cargo configuration, Rust flags, wrapper
identity, stage dependencies, resource limits, and metric slots. Human output
is the compact diagnostic view of the same plan.

### `soldr lint ci`

Statically validate the repository's executable CI/build surfaces against
Soldr's build policies (soldr#2038). This suite is a **pure filesystem scan**:
it requires no `Cargo.toml`, no Rust toolchain, and never starts the compiler
cache, so it runs in any repository — including non-Rust ones.

```bash
soldr lint ci                 # human-readable diagnostics (default)
soldr lint ci --format json   # stable machine-readable report
```

`ci` accepts only `--format json|human`; it does not take Cargo scope flags.
It exits `0` when there are no error-severity findings (warnings alone still
exit `0`) and non-zero when any error-severity finding remains after
suppressions.

**Scanned surfaces:** `.github/workflows/**/*.yml|yaml`,
`.github/actions/**/action.yml|yaml`, everything under `.github/scripts/**`,
and any `*.sh` / `*.py` / `*.ps1` helper referenced from a `run:`/`uses:` line
that exists on disk. Full-line and inline `#` comments are ignored, so prose
that mentions a legacy command is never flagged.

The suite is an extensible **registry of independent rules**. Adding a new CI
policy is a bounded module + one registration entry; the public command surface
does not change.

#### Rule: `cross-compile-surface`

Enforces that Apple Darwin (`*-apple-darwin`) and Windows MSVC
(`*-pc-windows-msvc`) builds go through the blessed `soldr build --target ...`
surface (`soldr prepare --target ...` is also accepted). It flags direct use of
a legacy cross wrapper or raw cross compiler for those targets:

- `cargo xwin` / `cargo-xwin` (and `cargo install cargo-xwin`),
- `cargo zigbuild` / `cargo-zigbuild`,
- raw `zig cc` / `zig c++` / `zig build-exe`,
- `*-w64-mingw32-*` (mingw), osxcross `o64-clang`, or a `clang`/`gcc`/`cc`
  carrying an Apple/Windows `--target`.

**Target-aware Zig exception:** Zig and `cargo-zigbuild` remain **allowed** for
`*-unknown-linux-*` and manylinux targets. The rule resolves each invocation's
`--target` (including matrix placeholders such as `${{ matrix.target }}`
against the workflow's declared matrix targets) and evaluates **per target**, so
a legitimate Linux-Zig matrix row never masks an invalid Apple/Windows row. Soldr's
own managed xwin / Apple-SDK / LLVM assets are allowed implementation details;
the rule governs the repository's public build entrypoint, not soldr internals.

A target that cannot be statically resolved on a surface that is otherwise
capable of Apple/Windows builds is reported as a lower-severity `warning`
(non-failing) rather than silently passing.

#### Inline suppression

A finding can be suppressed with a comment on the offending line or the line
immediately above it:

```yaml
soldr cargo xwin build --target x86_64-pc-windows-msvc  # soldr-lint-ci: allow cross-compile-surface -- intentional legacy-path regression test
```

`allow all` suppresses every rule on that line; `allow <rule-id>[,<id>...]`
suppresses only the named rules. Any text after `--` is a free-form reason.

#### JSON schema (`schema_version: 1`)

`--format json` emits a single object:

```json
{
  "schema_version": 1,
  "ok": true,
  "findings": [
    {
      "rule": "cross-compile-surface",
      "severity": "error",
      "file": ".github/workflows/release.yml",
      "line": 42,
      "tool": "cargo zigbuild",
      "target": "aarch64-apple-darwin",
      "recommendation": "use `soldr build --target aarch64-apple-darwin` (or `soldr prepare --target aarch64-apple-darwin`) instead of `cargo zigbuild`"
    }
  ]
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `schema_version` | integer | Schema version; bumped only on a breaking shape change. |
| `ok` | boolean | `true` when there are zero **error**-severity findings. |
| `findings[]` | array | Every finding (error and warning) after suppressions. |
| `findings[].rule` | string | Stable rule id, e.g. `cross-compile-surface`. |
| `findings[].severity` | string | `"error"` or `"warning"`. |
| `findings[].file` | string | Repo-root-relative path, `/`-separated. |
| `findings[].line` | integer | 1-based line where the command begins. |
| `findings[].tool` | string | Detected non-blessed tool, e.g. `cargo xwin`. |
| `findings[].target` | string | Resolved triple, a `*`-representative, or `<unresolved>`. |
| `findings[].recommendation` | string | The exact blessed replacement command. |

For Dylint builds, Soldr installs an absolute `soldr-dylint` compiler shim.
Ordinary dependency and lint-library compilation follows
`soldr-dylint -> rustc`; workspace analysis follows
`soldr-dylint -> dylint-driver -> rustc`. The compiler cache keys the driver,
loaded lint-library contents, and Dylint configuration. It caches individual
compiler outputs and diagnostics, never a command-level lint verdict: the real
Dylint pass executes on every invocation, so changed source is always analyzed.
Cargo incremental state accelerates repeated work in the same target tree,
while Soldr's object cache enables clean-target and sibling-worktree reuse.

### `soldr dylint cook`

Prepare external dependencies for a real Dylint pass without mixing its
nightly artifacts into the repository's ordinary stable build tree:

```bash
soldr dylint cook --workspace --all-targets
soldr dylint cook --plan-only --json
```

The command resolves one exact Dylint nightly from the verified
soldr-toolchain catalogue (or an explicit `--toolchain nightly-YYYY-MM-DD`),
then verifies the installed compiler's full release and commit identity. It
reconstructs a dependency skeleton and runs a check-shaped pass through
Soldr's normal compilation cache. `RUSTC_WORKSPACE_WRAPPER` and every
`DYLINT_*` library variable are removed for this phase, so custom lint
libraries are loaded only by the later real Dylint invocation.

Outputs live under `target/dylint/target/<nightly>/`, matching Dylint 6's own
workspace-check directory. The warm marker includes the observed compiler
commit, manifests, lockfile, selected target/profile/features/packages,
configuration, and wrapper identity. Workspace source contents are excluded,
so editing only a local source file preserves the external-dependency layer.
Conflicting nightly requirements from configured lint-library paths fail
instead of selecting one heuristically.

`--plan-only --json` does not install a missing toolchain. Its stable
`schema_version: 1` result includes `compiler`, `target_directory`,
`build_shape`, `cache_key`, and `outcome`. A plan-only result can report a
verified hit only when the installed compiler identity, marker, and target
payload all match. A normal invocation verifies the compiler again after any
restore/install and reports `miss` after cooking or `skip` when the complete
layer is already warm.

Shape options are `--target`, `--release` / `--profile`, `--workspace`,
repeatable `--package`, `--features`, `--all-features`,
`--no-default-features`, `--all-targets`, `--tests`, `--benches`,
`--examples`, repeatable `--config`, `--locked`, `--frozen`, and `--offline`.
Ordinary `soldr cook` behavior is unchanged.

### `soldr cook`

Content-addressable dependency pre-build (issue #359). `cook` is a shim
around [`cargo-chef`](https://github.com/LukeMathWalker/cargo-chef) — soldr
ships no Rust reimplementation of the recipe logic; it fetches the pinned
`cargo-chef` binary (currently v0.1.73), drives `cargo chef prepare` to
synthesise a `recipe.json` derived only from `Cargo.toml` + `Cargo.lock`,
then drives `cargo chef cook` to compile a stub project containing those
deps. The output lands in `target/` with no project source code touched,
so any subsequent `soldr cargo build` reuses the compiled deps.

Both phases route through `soldr cargo` so they automatically pick up
zccache (`RUSTC_WRAPPER`), `ZCCACHE_PATH_REMAP=auto`, the soldr linker
selection, and the soldr-managed `CARGO_HOME` / `RUSTUP_HOME`.

```bash
soldr cook                                  # debug cook, ephemeral recipe
soldr cook --release                        # release cook, ephemeral recipe
soldr cook --release --target x86_64-unknown-linux-musl
soldr cook --release --recipe-path recipe.json --prepare-only   # Docker phase 1
soldr cook --release --recipe-path recipe.json --cook-only      # Docker phase 2
soldr cook --release --keep-recipe          # cook + leave recipe.json behind
soldr cook --release -- --features extra,fast --no-default-features
```

Recognised flags:

- `--release` — forwarded to `cargo chef cook --release`.
- `--target <triple>` — forwarded to `cargo chef cook --target <triple>`.
- `--workspace` (alias `--all`) — forwarded to `cargo chef cook --workspace`.
- `--profile <name>` — forwarded to `cargo chef cook --profile <name>`.
- `-p` / `--package <name>` — repeatable; forwarded as `--package <name>`.
- `--recipe-path <path>` — write/read the recipe at this absolute or
  manifest-relative path. Without this flag the recipe lives in a temp
  dir and is deleted on exit.
- `--keep-recipe` — retain the recipe on disk (at `--recipe-path` if
  supplied, else `<cwd>/recipe.json`) so you can inspect it.
- `--prepare-only` — run `cargo chef prepare` only and exit. Used for
  the Docker recipe-layer pattern below.
- `--cook-only` — skip `prepare`; requires `--recipe-path`. Used for the
  Docker cook-layer pattern below.
- `--no-trim` — skip the post-cook `target/` trim. By default (issue
  #459) cook removes cargo-recreatable noise — incremental state, the
  synthetic stub binary, build-script binaries, large stderr blobs,
  debug sidecars, and `examples/`/`doc/`/`tests/` — so the downstream
  tarball ships dramatically fewer bytes (~30–40% drop on a typical
  cook output). Use `--no-trim` only if you genuinely need the full
  raw `target/` tree.
- Anything after `--` is forwarded verbatim to `cargo chef cook` (e.g.
  `--features`, `--no-default-features`, `--all-features`, `--tests`,
  `--benches`).

**Docker recipe pattern** — the canonical use case the issue highlights:

```dockerfile
FROM rust:1 AS chef
RUN cargo install soldr
WORKDIR /app

FROM chef AS planner
COPY . .
RUN soldr cook --release --prepare-only --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Heavy step — cached as long as recipe.json (i.e. Cargo.lock) is stable.
RUN soldr cook --release --cook-only --recipe-path recipe.json
COPY . .
RUN soldr cargo build --release --bin myapp
```

**Local-dev pattern** — first-time setup on a fresh clone:

```bash
git clone <repo> && cd <repo>
soldr cook --release         # one-time dep prebuild; ~10 min cold, ~0s warm
soldr cargo build --release  # builds only the project on top
```

**Cargo.lock missing.** `soldr cook` continues with a warning if
`Cargo.lock` is absent — cargo-chef will derive the recipe from
`Cargo.toml` alone, which weakens content-addressability. For
deterministic builds, commit `Cargo.lock` (libraries should still ship
`Cargo.lock` for reproducibility under `soldr cook`, even though cargo
normally `.gitignore`s lockfiles in library crates).

**Companion automation.** [`zackees/setup-soldr#110`](https://github.com/zackees/setup-soldr/issues/110)
proposes a GitHub Action that keys the complete dependency closure by
`Cargo.lock`: the cooked `target/` plus Cargo registry/git sources. With
`soldr cook` available as a primitive, the action restores both archives and
runs cook only when the dependency base is absent.

### `soldr save` / `soldr hydrate` (`soldr load` alias)

Bundle a build-cache directory plus a content-verified snapshot of
source-file mtimes into a single `.tar.zst` archive (`save`), then
rematerialize it on a fresh checkout (`hydrate`). The historical `load`
spelling remains a compatibility alias. Intended for CI cache layers
that need stable Cargo fingerprints across `actions/checkout` runs
without resorting to mtime-rewrite tricks.

```bash
soldr save --cache-dir <dir> --workspace <dir> --out cache.tar.zst
soldr hydrate --archive cache.tar.zst --cache-dir <dir> --workspace <dir>
```

Recognised `soldr save` flags:

- `--cache-dir <DIR>` — cache directory to archive.
- `--workspace <DIR>` — workspace whose source-file mtimes get snapshotted.
- `--out <FILE>` — destination archive.
- `--threads <N>` — parallel hash / compression worker hint.
- `--mtimes-only` — write only the source-file mtime manifest.
- `--delta-from-manifest <FILE>` — save a delta against a previously
  restored base manifest.
- `--ci`, `--minimal` — CI/minimal payload profile. Excludes logs,
  sockets, lock files, runtime scratch, zccache runtime binaries, and
  soldr-managed binary/toolchain trees from the cache payload while
  preserving cache artifacts and manifest state needed for warm rustc hits.
- `--json` — emit a JSON summary. Save JSON includes
  `profile`, `source_files`, `cache_files`, `excluded_files`,
  `excluded_bytes`, `archive_bytes`, and `elapsed_ms`.

When `--cache-dir` contains the active embedded zccache root, `soldr save`
first completes a cache checkpoint, gracefully shuts down the daemon, and
waits for the exact daemon generation that acknowledged shutdown before the
archive walk begins. A checkpoint alone is not a snapshot barrier because a
new publication could otherwise land between the checkpoint and tar traversal.
If quiescence cannot be proven, save fails instead of producing a racing
snapshot. This intentionally stops the daemon; the next compile-like command
starts a fresh generation. An unrelated `--cache-dir` does not flush, stop, or
otherwise depend on the ambient Soldr daemon.

Recognised `soldr hydrate` / `soldr load` flags (issue #575):

- `--archive <FILE>` — input archive produced by `soldr save`.
- `--cache-dir <DIR>` — destination cache directory; created if absent.
- `--workspace <DIR>` — workspace whose source-file mtimes get the
  content-verified replay. Optional when `--mtimes-only` is unset.
- `--threads <N>` — parallel-extract worker count. Defaults to
  rayon's `num_cpus`; capped at the value passed here.
- `--mtimes-only` — refuse cache entries; apply mtime snapshot only.
- `--manifest-out <FILE>` — write the archive's protobuf manifest for a
  later `soldr save --delta-from-manifest`.
- `--profile-extract` — emit a per-phase profile line to stderr after
  the load completes. Shape:
  ```text
  soldr load: profile: zstd_decode=4120ms tar_parse=890ms extract_total=10510ms \
    workers={0:n=12058, 1:n=12090, 2:n=12053, 3:n=12030} \
    per_file_p50_us=180 p95_us=450 p99_us=1200 cache_files=48231
  ```
  Also enabled when `SOLDR_PROFILE_EXTRACT=1` is set in the environment.
  Useful for tuning the parallel-extract worker count against real
  workloads (issue #575). The line lands on **stderr** and retains the
  historical `soldr load:` prefix for log-parser compatibility; the existing
  machine-readable status line on stdout is untouched.
- `--auto-defender-exclude` — on Windows + admin, briefly add
  `--cache-dir` to Defender's exclusion list for the duration of the
  load (issue #596). Never UAC-prompts: no-op on non-Windows or when
  the current process isn't elevated.
- `--json` — machine-readable status line on stdout instead of the
  human-readable summary.

`SOLDR_LOAD_WORKERS=<N>` overrides `--threads` and sizes the rayon
pool that runs both the per-file extract workers and the
mtime-replay walk.

### `soldr status`

Show cache and target information. The stable JSON shape retains the historical
compile-daemon `fallbacks` rollup for upgrade compatibility; the mandatory
broker route no longer appends new fallback records.

Stable machine-facing mode:

```bash
soldr status --json
```

### `soldr clean`

Clear the local embedded-zccache artifact cache and remove Soldr's zccache
session state directory.

### `soldr config`

Show or set configuration.

### `soldr cache`

Inspect the embedded zccache service and its cache status.

Stable machine-facing mode:

```bash
soldr cache --json
```

#### `soldr cache flush`

Synchronously checkpoint pending embedded zccache publications and persistent
index state without stopping `soldr-daemon`. The command succeeds only when
all reported steps complete; `--json` exposes the detailed flush report.

```bash
soldr cache flush
soldr cache flush --json
```

#### `soldr cache shutdown`

Checkpoint the embedded cache, request graceful daemon shutdown, and by default
wait for the exact PID and generation returned in the shutdown
acknowledgement. The default wait is 300 seconds and can be changed with
`--shutdown-timeout-seconds <SECONDS>`. Once a daemon acknowledges shutdown,
Soldr never force-kills it: a timeout reports failure while the daemon
continues its durability work.

`--archive-logs <DIR>` copies the finalized session log, journal, and stats
only after the acknowledged generation is proven quiescent. It therefore
cannot be combined with `--no-wait`. `--no-depgraph-save` skips the explicit
pre-shutdown checkpoint for debugging, but graceful shutdown still completes
its own cache flush. `--json` emits the stable machine-facing result.

```bash
soldr cache shutdown
soldr cache shutdown --shutdown-timeout-seconds 600 --archive-logs ./soldr-logs
soldr cache shutdown --no-wait
```

#### `soldr cache prune-target <path>`

Prune stale per-prefix cargo build artifacts inside a given `target/`
directory. Scanned subdirectories under each profile (`debug/`,
`release/`, …) are `deps/`, `.fingerprint/`, `incremental/`, and
`build/`.

Two strategies are available:

- **Default — orphan siblings (issue #336).** Keep the newest entry per
  `(parent_dir, prefix)` bucket. Each subdirectory's set of
  hash-siblings is pruned independently. Conservative: cargo tolerates
  orphans across the four subdirs as long as the live entry inside
  each is current.
- **`--keep-latest` — aggressive (issue #316).** Bucket by `prefix`
  alone; keep only the **newest hash family** per logical artifact
  name, deleting every other hash's files across the four subdirs.
  Shrinks a heavily-rebuilt target/ from 16+ GB to ~2 GB on real
  workloads. Use when you don't need the per-subdir orphan retention.

**Recency ranking.** Both strategies prefer cargo's authoritative
`target/<profile>/.fingerprint/<prefix>-<hash>/invoked.timestamp`
mtime — the same signal cargo's unstable `-Zgc` uses — and fall back
to the entry's own filesystem mtime when the fingerprint file is
missing or unreadable. The JSON output reports per-decision
provenance via `keep_decisions_from_fingerprint` and
`keep_decisions_from_mtime`.

```bash
soldr cache prune-target ./target                  # dry-run, orphan-siblings
soldr cache prune-target ./target --keep-latest    # dry-run, aggressive
soldr cache prune-target ./target --force          # actually delete (orphan-siblings)
soldr cache prune-target ./target --keep-latest --force  # actually delete (aggressive)
soldr cache prune-target ./target --dry-run --json # machine-readable plan
```

Defaults to a dry run for safety; pass `--force` (or `--no-dry-run`)
to actually delete entries. If `target/.cargo-lock` or any
`target/<profile>/.cargo-lock` is present the command refuses with a
non-zero exit code (suggests a live build).

JSON schema (`--json`):

```json
{
  "schema_version": 1,
  "command": "cache prune-target",
  "target_dir": "<absolute path>",
  "dry_run": true,
  "keep_latest": false,
  "scanned": 3,
  "kept": 1,
  "deleted": 2,
  "reclaimed_bytes": 0,
  "reclaimed_human": "0 B",
  "keep_decisions_from_fingerprint": 1,
  "keep_decisions_from_mtime": 0,
  "entries": [
    {
      "path": "<absolute path>",
      "prefix": "libfoo",
      "hash": "abcdef1234567",
      "size_bytes": 0,
      "size_human": "0 B",
      "mtime_unix": 1700000500,
      "action": "keep"
    }
  ]
}
```

**Explicit target pruning (issues #485 and #1818).** Soldr no longer
prunes hash families automatically around build-like invocations.
Multiple families for one crate can all be live at once, so selecting
only the newest family can turn an unchanged warm build into a rebuild
loop.

Use `soldr cache prune-target <path>` when explicit target maintenance
is desired. The historical `--no-gc-target`,
`--no-gc-target-before`, and `--no-gc-target-after` flags remain
accepted as compatibility no-ops. `SOLDR_NO_GC_TARGET` is likewise
accepted but unnecessary now that no automatic pass runs.

### `soldr version`

Print soldr version.

Stable machine-facing mode:

```bash
soldr version --json
```

### `soldr toolchain`

Project-aware orchestrators around `rustup` and `cargo install` that
read `rust-toolchain.toml` so users (and CI) don't have to thread the
pinned channel through every command.

```bash
soldr toolchain install   # rustup toolchain install <channel> --profile minimal --no-self-update
soldr toolchain prepare   # install + component add + target add + cargo install for [soldr.plugins]
soldr toolchain ensure    # bootstrap rustup if missing, run `prepare`, smoke-verify (cargo/rustc --version)
soldr toolchain ensure --json  # same, but emit a stable JSON payload (schema_version: 1)
soldr toolchain link --shim-dir <path> [--json] [--force]  # write PATH shim files (issue #407 Phase 3)
soldr toolchain doctor [--json]   # run env-detection probes (musl-cc, shared target/) (issue #407 Phase 4)
```

`prepare` runs, in order:

1. `rustup toolchain install <channel> --profile minimal --no-self-update`
2. `rustup component add --toolchain <channel> <component>` for every entry in `[toolchain].components`
3. `rustup target add --toolchain <channel> <target>` for every entry in `[toolchain].targets`
4. `cargo install <name> [--version V] [--locked] [--features ...] [--no-default-features]` for every entry in `[soldr.plugins]`

The Cargo front door performs the toolchain/component/target portion
automatically before launching the child build. A successful preparation is
memoized under the Soldr cache, so an unchanged warm invocation launches no
Rustup subprocesses. The memo is invalidated when the channel, profile,
component or target requirements, explicit `+toolchain`, effective Rustup
home/binary, or installed toolchain identity changes. Failed preparation is
never memoized. `soldr toolchain prepare` and `ensure` remain explicit,
unconditional orchestrators and also handle `[soldr.plugins]`.

Plugin installs are bootstrap/dev-tool acquisition, not project compilation.
They invoke the directly resolved cargo binary and clear inherited
`RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER` so a setup step cannot
recursively re-enter Soldr's zccache wrapper while installing the tools
that future builds may use.

The first non-zero exit short-circuits the chain.

#### `soldr toolchain ensure`

One-shot "make sure this host can build" verb (issue #407 Phase 2):

1. Auto-bootstraps `rustup` into the soldr-managed bin dir if missing
   (reuses the same logic as `soldr bootstrap`). Respects
   `SOLDR_NO_BOOTSTRAP=1`.
2. Runs the same `install` + `component add` + `target add` + plugin-
   install pipeline as `prepare`.
3. Smoke-verifies the resolved toolchain by spawning `cargo --version`
   and `rustc --version`. Either failure marks the verify as failed and
   exits non-zero.

`ensure` is intended for one-stop bootstrap callers like
[setup-soldr#133](https://github.com/zackees/setup-soldr/issues/133)
that want to delegate every TS toolchain step to the soldr binary.
Existing `install` and `prepare` output formats are unchanged.

The `--json` payload (`schema_version: 1`) is the stable contract for
those callers — fields may be added in future schema versions but
existing field names and types will not change without a schema bump:

```json
{
  "schema_version": 1,
  "channel": "1.95.0",
  "rustup_bootstrapped": false,
  "components_added": ["rustfmt", "clippy"],
  "targets_added": ["aarch64-apple-darwin"],
  "plugins_installed": ["cargo-zigbuild@0.18"],
  "smoke_verify": {
    "cargo_version": "cargo 1.95.0 (abc1234 2026-04-15)",
    "rustc_version": "rustc 1.95.0 (def5678 2026-04-15)",
    "ok": true
  },
  "elapsed_ms": 12345
}
```

Notes on the schema:

- `channel` is `null` when no `rust-toolchain.toml` is present; the rest
  of the payload still serializes so consumers can parse unconditionally.
- `rustup_bootstrapped` is `true` only when `ensure` actually fetched
  rustup-init for this invocation. A pre-existing rustup or
  `SOLDR_NO_BOOTSTRAP=1` keeps it `false`.
- `components_added` / `targets_added` / `plugins_installed` mirror the
  manifest entries that the `prepare` pipeline attempted. Plugin labels
  are `name` for bare or `*` versions, otherwise `name@version`.
- `smoke_verify.ok` is `false` when either spawn fails or returns
  non-zero. The JSON payload is emitted in both success and failure
  cases; only the process exit code differs.

#### `soldr toolchain link`

Write PATH shim files into `--shim-dir` so a child process that
resolves `cargo` / `rustfmt` / `clippy-driver` / `rustc` / `rustdoc`
through PATH gets routed back through `soldr <tool>` (issue #407 Phase
3, ports setup-soldr's `ensure-shims.ts`).

Each shim is platform-aware:

- **Unix**: a `#!/bin/sh` script that `exec`s `<soldr-path> <tool>
  "$@"`. File mode `0o755`.
- **Windows**: a `.cmd` script with CRLF line endings that calls
  `"<soldr-path>" <tool> %*`.

The absolute path to the running soldr binary (resolved via
`std::env::current_exe()`) is baked into each shim at write time, so
the shim does not depend on PATH to find soldr itself.

Idempotency:

- Existing shim file whose contents equal the expected body → left
  alone, reported as `skip_reason: "existing-matches"`. Mtime is
  preserved.
- Existing shim file whose contents differ AND `--force` not passed →
  left alone, reported as `skip_reason: "existing-differs"`. The
  caller can detect this and decide whether to re-run with `--force`.
- Existing shim file whose contents differ AND `--force` passed →
  overwritten, reported as `created: true`.

The `--json` payload (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "shim_dir": "/runner/.setup-soldr/shims",
  "tools": [
    {"name": "cargo", "shim_path": "/runner/.setup-soldr/shims/cargo", "created": true},
    {"name": "rustfmt", "shim_path": "/runner/.setup-soldr/shims/rustfmt", "created": false, "skip_reason": "existing-matches"},
    {"name": "clippy-driver", "shim_path": "/runner/.setup-soldr/shims/clippy-driver", "created": true},
    {"name": "rustc", "shim_path": "/runner/.setup-soldr/shims/rustc", "created": true},
    {"name": "rustdoc", "shim_path": "/runner/.setup-soldr/shims/rustdoc", "created": true}
  ],
  "elapsed_ms": 12
}
```

Notes on the schema:

- `tools[].skip_reason` is omitted (not `null`) when `created: true`.
- The `tools` array order is stable: `cargo`, `rustfmt`,
  `clippy-driver`, `rustc`, `rustdoc`.
- `link` never adds the shim directory to the caller's `PATH` — that
  is the caller's responsibility (e.g. setup-soldr emits a GitHub
  Actions `addPath` call after consuming the JSON).

#### `soldr toolchain doctor`

Run env-detection probes that ship the diagnostic intel `setup-soldr`
used to compute in TypeScript (issue #407 Phase 4, ports the
env-detection halves of setup-soldr's `detect-musl-cc.ts`,
`detect-shared-target-warning.ts`, and `diagnostics.ts`).

Namespaced under `toolchain` to avoid colliding with the top-level
`soldr doctor` system check.

Probes (run in stable order):

1. **`musl-cc`** — scans `PATH` for `musl-gcc` / `musl-clang` and, if
   found, captures the first line of `--version` output. Auto-skipped on
   non-Linux hosts (`details.skipped = "not-linux"`). When the host is
   Linux but no musl C compiler is on `PATH`, the probe still reports
   `ok: true` with `details.found = false` — not all Linux workflows
   need musl tooling, so missing it is informational rather than fatal.
2. **`shared-target-warning`** — walks the current workspace's
   `target/` (up to 3 levels deep) looking for a populated
   cargo `.fingerprint/` directory. Mirrors the prepopulated-target
   detector landed in PR #508. Reports `would_warn: true` when at least
   one fingerprint dir is found, signalling that a subsequent
   `soldr cargo build` may collide with the rust-plan restore path.

Exit code: `0` when every probe reports `ok: true`, `1` otherwise.

The `--json` payload (`schema_version: 1`) is the stable contract for
`setup-soldr#133` and similar consumers:

```json
{
  "schema_version": 1,
  "host": {"os": "linux", "arch": "x86_64", "libc": "gnu"},
  "probes": [
    {
      "name": "musl-cc",
      "ok": true,
      "details": {
        "musl_cc": "/usr/bin/musl-gcc",
        "tool": "musl-gcc",
        "version": "musl-tools 1.2.5-1"
      }
    },
    {
      "name": "shared-target-warning",
      "ok": true,
      "details": {
        "target_dir": "/workspace/target",
        "fingerprint_dirs_found": 0,
        "would_warn": false
      }
    }
  ],
  "elapsed_ms": 12
}
```

Notes on the schema:

- `host.os` is `std::env::consts::OS` (`linux` / `windows` / `macos`).
- `host.arch` is `std::env::consts::ARCH` (`x86_64` / `aarch64` / …).
- `host.libc` is `gnu` on Linux, `msvc` on Windows, `darwin` on macOS
  (CLAUDE.md mandates MSVC-default on Windows).
- The `probes` array preserves declaration order: `musl-cc` first,
  `shared-target-warning` second. Future probes will be appended.
- Each probe's `details` object is probe-specific. Consumers must
  switch on `probe.name` before reading nested keys.
- `probe.ok = false` is reserved for future probes; today both probes
  always set `ok: true` because a missing musl-cc or a clean target/
  is informational rather than blocking.

#### `[soldr.plugins]`

Top-level `[soldr]` section of `rust-toolchain.toml`. Currently
surfaces a `plugins` table keyed by cargo crate name. Each value is
either a bare version requirement or a detailed table.

```toml
[toolchain]
channel = "1.95.0"

[soldr.plugins]
cargo-nextest = "0.9"
cargo-zigbuild = { version = "0.18", locked = true }
cargo-deny    = "*"          # any version — `--version` is omitted
cargo-llvm-cov = { version = "0.6", features = ["no_cfg_coverage"], no_default_features = true }
```

Field semantics for the detailed shape:

| Field                 | Type           | Maps to                  |
|-----------------------|----------------|--------------------------|
| `version`             | string         | `--version <value>`. `"*"` or unset omits the flag entirely. |
| `locked`              | bool           | `--locked` when `true`.  |
| `features`            | list of string | `--features <a,b,c>` when non-empty. |
| `no_default_features` | bool           | `--no-default-features` when `true`. |

Installs are dispatched to the cargo binary resolved by soldr's
toolchain probe (`resolve_toolchain_binary("cargo")`) and invoked
directly — **not** through the rustc wrapper. This routes installs
into soldr-managed `$CARGO_HOME` while letting the active cargo honor
`rust-toolchain.toml` at exec time, so no explicit channel is threaded
through. `cargo install` is idempotent by design, so a second
`prepare` after a successful one is a no-op for plugins.

### `soldr doctor`

Diagnose drift between `rust-toolchain.toml` and the rustup state
currently installed for the declared channel. Read-only — `doctor`
never invokes `rustup toolchain install`, `rustup component add`, or
`rustup target add`. Exits `1` when drift is detected, `0` otherwise.
When no `rust-toolchain.toml` exists in the current working directory
the command exits `0` and reports that no manifest was found.

```bash
soldr doctor
soldr doctor --json
soldr doctor --refresh-defender-probe       # Windows: force fresh probe of the cache dir
```

Flags:

- `--json` — emit the stable machine-facing JSON form.
- `--refresh-defender-probe` — ignore the cached Defender real-time-scan
  probe result and run a fresh probe of the soldr cache directory.
  No-op outside Windows. Issue #357.

Example human output (drift detected):

```text
manifest: /home/user/project/rust-toolchain.toml
toolchain: 1.95.0
  status: installed

components (declared 2):
  rustfmt   installed
  clippy    MISSING

targets (declared 1):
  x86_64-unknown-linux-musl   installed

result: drift detected (1 missing component)
hint: run `soldr toolchain prepare` to bring installed state in sync with manifest
```

Example JSON output (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "command": "doctor",
  "manifest_path": "/home/user/project/rust-toolchain.toml",
  "toolchain": {"channel": "1.95.0", "installed": true},
  "components": [
    {"name": "rustfmt", "installed": true},
    {"name": "clippy", "installed": false}
  ],
  "targets": [
    {"triple": "x86_64-unknown-linux-musl", "installed": true}
  ],
  "drift": true,
  "missing_components": ["clippy"],
  "missing_targets": [],
  "timeouts": [
    {"name": "compile reply", "env_var": "SOLDR_COMPILE_REPLY_TIMEOUT_SECS", "default_secs": 1800, "effective_secs": 1800, "source": "default", "override_ignored": false}
  ],
  "fallbacks": {"total": 0, "recent": []},
  "soldr_debug_info": {
    "binary_path": "C:\\Users\\user\\.soldr\\bin\\soldr.exe",
    "debug_info_found": 1,
    "debug_info_expected": 1,
    "symbol_path": "C:\\Users\\user\\.soldr\\bin"
  },
  "defender_probe": {
    "verdict": "scanned",
    "probed_path": "C:\\Users\\user\\.soldr\\cache",
    "median_write_ms": 412,
    "probed_at_unix": 1715000000,
    "refreshed_this_run": false
  }
}
```

**`timeouts`** (soldr#1838 Phase 3). One row per daemon/toolchain timeout that
has an env override, so a consumer can see the *effective* value and where it
came from without re-deriving it: `name`, `env_var`, `default_secs`,
`effective_secs`, `source` (`"default"`, `"override"`, or `"default (override
ignored: unparseable)"`), and `override_ignored` — `true` when a variable was
set but did not take effect
(the soldr#1837 malformed-value-falls-back-to-default rule), broken out so CI
can assert on it without string-matching `source`.

**`fallbacks`** (soldr#1838 Phase 4). Historical compile-daemon fallback
telemetry retained as `{ total, recent[] }` for JSON compatibility. The
mandatory broker route does not append new records.

**Defender probe** (issue #357, Windows-only). `soldr doctor` runs a
throttled empirical scan probe to detect whether the soldr cache
directory is being inspected by Windows Defender's real-time
protection. The probe writes a 1 MiB `.dll` file into the cache
directory, times the syscall, and classifies the median across 3
repeats: ≥80 ms means scanned, below means excluded (or running on a
trusted Dev Drive). State persists at `~/.soldr/defender-probe.json`
and refreshes only when the cache path changes, the soldr version
changes, the cached state is older than 7 days, or
`--refresh-defender-probe` is passed. The field is omitted from
output on non-Windows platforms.

Component installed-state matching is target-qualified: rustup's
`component list --installed` returns names like
`clippy-x86_64-unknown-linux-gnu`, so a declared `clippy` matches any
installed entry that either equals `clippy` exactly or starts with
`clippy-`.

### `soldr optimize`

Apply platform-specific hot-cache optimizations to remove
build-time penalties caused by antivirus / real-time scanners. On
Windows this adds soldr-owned cache paths (and optionally the current
project's `target/`) to Windows Defender's exclusion list. On macOS
and Linux it is a no-op with a clear message — soldr's workloads do
not require exclusions there.

```bash
soldr optimize                                # default scope: all
soldr optimize --scope global                 # only ~/.soldr/* paths
soldr optimize --scope project                # only <workspace>/target
soldr optimize --scope all                    # both
soldr optimize --dry-run --json               # plan-only output
soldr optimize --undo --scope global          # reverse soldr-added exclusions
```

Flags:

- `--scope {global|project|all}` — what to optimize. Defaults to
  `all`.
- `--undo` — reverse exclusions previously added by soldr. Only
  paths tracked in `~/.soldr/managed-defender-exclusions.json` are
  removed; entries the user added by hand are never touched.
- `--dry-run` — print the plan and exit without invoking PowerShell.
- `--json` — emit the stable machine-facing JSON form.
- `--manifest-path <PATH>` — explicit `Cargo.toml` for the `project`
  scope. When unset, soldr walks up from the current directory.

Platform behavior:

| Platform | Behavior |
|---|---|
| Windows 10 | Add Defender exclusions. UAC self-relaunch when not admin. |
| Windows 11 pre-22H2 | Same as Windows 10, plus an info note about Dev Drive in 22H2. |
| Windows 11 22H2+ | Same as Windows 10, plus a Dev Drive suggestion when `fsutil devdrv` is supported. |
| Windows + Defender disabled | Prints "Defender not active; no exclusions needed." Exits 0. |
| Windows + no PowerShell on PATH | Hard error; exits non-zero. |
| macOS / Linux | No-op; exits 0 with a message. |

The global scope covers:

- `~/.soldr/cache`
- `~/.soldr/bench`
- `~/.soldr/runtime`
- `~/.soldr/state.sqlite3`
- Soldr's embedded-zccache state and session/report owner directory
  (`~/.soldr/cache/zccache`).
  When `ZCCACHE_CACHE_DIR` is set, that caller-selected zccache cache root is
  excluded explicitly.

The project scope covers `<workspace_root>/target/`, where
`<workspace_root>` is the nearest ancestor of the current directory
containing a `Cargo.toml`. Without `--manifest-path` and without a
matching ancestor, the command exits with `no Rust project detected`.

UAC self-relaunch:

When the current process is not elevated, soldr re-launches itself
via `Start-Process powershell -Verb RunAs --as-elevated-helper`. The
elevated child writes its JSON status to a temp file referenced by
the `SOLDR_OPTIMIZE_HELPER_OUTPUT` env var; the parent reads,
prints, and deletes it. The same exit code is propagated. If UAC is
denied or unavailable, soldr prints instructions for running the
helper from an elevated PowerShell and exits non-zero — soldr will
never silently elevate via tokens or scheduled tasks.

CI auto-skip:

`soldr optimize` detects CI via `GITHUB_ACTIONS`, `CI`, `BUILDKITE`,
`CIRCLECI`, `TRAVIS` (truthy values), or a non-empty `JENKINS_URL`,
and exits 0 with a clear message. Ephemeral runners discard
Defender state at job end, so adding exclusions is a no-op there.

Example JSON output (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "command": "optimize",
  "platform": "Windows10",
  "scope": "all",
  "undo": false,
  "dry_run": true,
  "defender_present": true,
  "defender_active": true,
  "actions": [
    {
      "path": "C:\\Users\\you\\.soldr\\cache",
      "action": "add",
      "scope": "global",
      "status": "planned"
    }
  ],
  "note": "dry-run: would add 6 paths to Defender exclusions"
}
```

Suppressing the pre-build warning:

When the cargo front door detects that the soldr cache directory is
being scanned by Defender, it emits a one-line warning to stderr
suggesting `soldr optimize global`. Set `SOLDR_QUIET_DEFENDER=1` to
silence it. The warning is also suppressed automatically in CI.

### `soldr gc`

Review reclaimable Cargo `target/` directories tracked in
`~/.soldr/state.sqlite3`. Implemented by issue #234 and made safe-by-default
by issue #289. The wrapper-mode hot path upserts each invocation's
resolved workspace `target/` path with the current timestamp; `soldr gc`
walks the registry, drops missing rows, applies safety guards, and
prints an info summary without prompting or deleting anything.

```bash
soldr gc                                      # info summary only
soldr gc --json                               # machine-readable summary
soldr gc --older-than 30d --larger-than 1GB   # summary with tunable filters
soldr gc purge                                # interactive deletion flow
soldr gc purge --all                          # delete every eligible candidate
soldr gc purge --older-than 30d --larger-than 1GB
soldr gc purge --json                         # machine-readable purge report
soldr gc list --kind cargo_target_incremental # list one taxonomy kind
soldr gc purge --target-incremental --all      # delete target/<profile>/incremental/
soldr gc purge --build-scripts --all           # delete build-script-build binaries
soldr gc purge --doc --all                     # delete target/doc/
soldr gc purge --subcommand-caches --all       # delete target/{criterion,nextest,...}/
soldr gc purge --registry-src --all            # delete extracted registry sources
soldr gc purge --git-checkouts --all           # delete git checkout worktrees
```

Defaults:

- `--older-than 10d`
- `--larger-than 256M`

Compatibility:

- `soldr gc --dry-run` is accepted as a temporary alias for `soldr gc`
- `soldr gc --all` is rejected with a pointer to `soldr gc purge --all`

Safety guards (from `docs/TARGET_GC_PROPOSAL.md`):

- skip a candidate whose workspace `Cargo.lock` was modified within
  the staleness window
- skip a candidate whose `target/.cargo-lock` exists (active build)
- only consider paths under `gc.allowlist_roots` (default: `~/dev`)

The summary includes the registry path, eligible candidate count, total
reclaimable size, skipped/dropped counts, and the largest eligible
target directories with size and last-used age.

`gc list --json` uses the issue #323 taxonomy. Derived kinds can be
purged through explicit opt-in flags or `--kind`; primary kinds are
report-only and `gc purge --kind <primary>` is rejected before deletion.

Derived purge kinds:

- `cargo_target` (default `gc purge` behavior)
- `cargo_target_incremental`
- `cargo_target_build_script_binaries`
- `cargo_target_doc`
- `cargo_target_subcommand_caches`
- `cargo_registry_src`
- `cargo_git_checkouts`

Report-only primary kinds:

- `cargo_registry_cache`
- `cargo_git_db`
- `cargo_installed_binaries`
- `rustup_toolchain`

**Eviction order and `in_worktree` (issue #2134).** `cargo_target`
entries carry `in_worktree`, and `age_seconds` is the same **effective**
age eviction uses — the more recent of soldr's registry stamp and the
directory's mtime, because the stamp goes stale while a directory stays
hot (a repo built with bare `cargo` never updates it). Eviction orders
`worktree → coldest → size`, so those two fields together explain why
any given target was chosen. Size is the last key, not the first: it
only breaks ties between equally cold targets.

`in_worktree` is omitted for kinds that have no owning workspace.

**`cargo_registry_src` last-used provenance (issue #349).** When
`soldr gc list --kind cargo_registry_src` (or unfiltered `gc list`)
walks `$CARGO_HOME/registry/src/...`, each entry's `last_used_unix`
field is preferentially derived from cargo's own
`$CARGO_HOME/.global-cache` SQLite tracker — the same data cargo's
unstable `-Zgc` uses to evict least-recently-used crate sources. When
the tracker is missing, locked, schema-drifted, or has no row for a
particular crate, soldr falls back to the directory's filesystem
mtime. The provenance is exposed as `last_used_source` on each entry:
`"global_cache"` or `"fs_mtime"`. The field is omitted from
`cargo_target` entries (no comparable tracker exists).

Configure additional allowlist roots via `~/.soldr/config.toml`:

```toml
[gc]
allowlist_roots = ["~/dev", "/work/repos"]
```

During build-like `soldr cargo ...` invocations, soldr checks free
space on the relevant target/current filesystem before spawning Cargo.
When less than 2 GB is available, it emits a yellow stderr warning that
recommends `soldr gc`. Disk-space detection failures are ignored so they
never fail the build.

### `soldr gc cargo` (issue #323)

Shells out to nightly cargo's unstable `-Zgc clean gc` against
`$CARGO_HOME`. The CLI prepends `rustup run <toolchain>` so the workspace
`rust-toolchain.toml` is bypassed — `cargo` here always means "the cargo
shipped with the requested toolchain".

```bash
soldr gc cargo                                  # nightly, conservative defaults
soldr gc cargo --dry-run --json                 # plan + machine-readable report
soldr gc cargo --max-src-age 7days --max-crate-age 14days
soldr gc cargo --toolchain nightly-2026-01-01   # pin the nightly snapshot
```

Toolchain resolution: `--toolchain` flag → `$SOLDR_GC_CARGO_TOOLCHAIN`
→ `nightly` default. Missing toolchain is a hard error from explicit
`gc cargo`; the `gc sweep` orchestrator downgrades it to a skip so CI
runners without nightly still get the soldr target purge stage.

JSON shape (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "command": "gc",
  "mode": "cargo",
  "toolchain": "nightly",
  "exit_code": 0,
  "dry_run": false,
  "args": ["-Zgc", "clean", "gc", "--max-src-age=7days"],
  "stdout_bytes": 612,
  "stderr_bytes": 0,
  "skipped": false,
  "skipped_reason": null
}
```

### `soldr gc locations` (issue #323)

Read-only enumeration of every cache directory soldr cares about. No
deletion, no last-used derivation. Walks `$CARGO_HOME/{registry/{src,
cache,index},git/{db,checkouts},.global-cache}`,
`$RUSTUP_HOME/{toolchains,update-hashes}`, `~/.soldr/cache/`, and
`~/.soldr/state.sqlite3`. Missing paths are reported with `exists: false`
and zero size.

```bash
soldr gc locations
soldr gc locations --json
```

Per-entry JSON: `{kind, path, exists, size_bytes, size_human, file_count,
owner, purge_safety}`. `owner` is `cargo` / `rustup` / `soldr`;
`purge_safety` is `regenerable` (safe to delete; cargo will refetch) or
`user_action` (the user installed it on purpose — never auto-purge).

### `soldr gc sweep` (issue #323)

Orchestrator that combines `gc locations`, cargo's `clean gc`, and the
soldr target purge in one shot. Designed to be the user-facing "free
me some disk space" command.

```bash
soldr gc sweep                            # full pipeline, prompt for each target
soldr gc sweep --all --dry-run --json     # plan everything, delete nothing
soldr gc sweep --no-cargo-gc              # skip cargo (e.g. no nightly available)
soldr gc sweep --aggressive               # second cargo pass with tighter ages
```

Stages, in order:

1. `gc locations` (always — read-only).
2. cargo `clean gc` with conservative defaults (unless `--no-cargo-gc`
   or nightly is missing — auto-skipped).
3. soldr's target purge over registered workspaces. Respects `--all`
   (no prompt) and the configured `gc.allowlist_roots`.
4. `--aggressive` only: second cargo pass with
   `--max-src-age=7days --max-crate-age=14days --max-git-co-age=7days`,
   each clamped to `auto_gc.min_age_secs`.

### `soldr gc target` (issue #574)

Cross-repo `target/` reclamation: walks a configurable root, finds every
workspace with a sibling `target/` directory, and reports their sizes
(sorted descending). With `--purge` it deletes them after a single
confirmation prompt (or `--yes` for non-interactive automation).

```bash
soldr gc target                                    # report under ~/dev (default)
soldr gc target --root ~/code --max-depth 6        # custom root and walk depth
soldr gc target --dry-run --json                   # machine-readable report
soldr gc target --purge                            # interactive deletion
soldr gc target --purge --yes                      # non-interactive deletion
soldr gc target --purge --yes --json               # CI-friendly purge + JSON
```

Options:

- `--root <PATH>` — walk root. Falls back to `$SOLDR_GC_TARGET_ROOT` then
  `~/dev`.
- `--max-depth <N>` — maximum directory depth for the workspace scan
  (default `4`). Hidden directories (leading `.`) and any directory
  named `target` are skipped.
- `--dry-run` — report-only (default).
- `--purge` — delete every reported `target/` directory.
- `--yes` — skip the y/n confirmation prompt. Required when stdin is
  not a terminal (CI).
- `--json` — emit the stable machine-facing payload (`schema_version: 1`).

JSON shape (stable):

```json
{
  "schema_version": 1,
  "command": "gc target",
  "mode": "report" | "purge",
  "root": "/home/me/dev",
  "max_depth": 4,
  "entry_count": 0,
  "total_bytes": 0,
  "total_human": "0 B",
  "entries": [
    {
      "workspace_root": "/home/me/dev/foo",
      "target_dir": "/home/me/dev/foo/target",
      "size_bytes": 0,
      "size_human": "0 B",
      "file_count": 0,
      "last_modified_ms": 0
    }
  ],
  "purged_count": 0,
  "failed_count": 0,
  "purged_bytes": 0,
  "purged_human": "0 B",
  "failures": []
}
```

The walker is independent of the per-repo `gc list` taxonomy above —
it scans real filesystem trees rather than the soldr registry, so it
finds workspaces that have never gone through `soldr cargo ...`.

### Host-volume disk watchdog (issue #574)

Before every `soldr cargo ...` build, soldr probes free space on the
build volume (the disk hosting the project's `target/` dir, or CWD if
`target/` doesn't exist yet) and either warns or aborts:

- Above `SOLDR_TARGET_WARN_FREE_GB` (default 10 GiB) — silent.
- Between warn and `SOLDR_TARGET_BLOCK_FREE_GB` (default 5 GiB) —
  one-line stderr warning pointing at `soldr gc target`.
- Below the block threshold — cargo is refused with a clear error
  directing the user at `soldr gc target --purge`.

Set `SOLDR_TARGET_AUTO_PRUNE_ENABLED=0` to disable the watchdog
entirely. The watchdog is layered on top of the legacy 2 GiB low-disk
advisory (issue #289) and never replaces it.

### Daemon-owned cache maintenance (issues #1762–#1764)

Each `soldr-daemon` owns exactly the one root selected by `SoldrPaths`:
production `.soldr`, development `.soldr-dev`, or an explicit
`SOLDR_CACHE_DIR`. Embedded zccache remains below that root. Standalone
`.zccache` is owned only by a standalone zccache daemon; Soldr never discovers,
migrates, links into, or deletes it.

The daemon stays resident by default, performs a lightweight pressure check
every five minutes, and performs a full age pass every 24 hours. The last
completed full pass and the latest structured report are stored under
`<root>/cache/soldr-daemon/`, so an overdue restart catches up immediately.
Nonzero `soldr daemon start --idle-timeout <seconds>` remains an explicit
auto-exit option. Soldr does not install Task Scheduler jobs, systemd timers,
launchd agents, or any other OS scheduler.

The embedded artifact budget defaults to 5% of filesystem capacity, clamped to
40–200 GiB and reduced when needed to preserve recovery space. At 85% fill it
evicts LRU artifacts older than four days toward 70%; at 100% fill or critically
low free space it evicts regardless of age toward 80% plus recovery headroom.
A daily pass expires artifacts older than 30 days even below budget. Physical,
hardlink-deduplicated allocated bytes are used. Set exactly one of
`ZCCACHE_CACHE_SIZE_BYTES` or `ZCCACHE_CACHE_SIZE_PERCENT` to override the
dynamic budget.

The same coordinated pass bounds cook artifacts, trash buckets, registered
workspace targets, PEP517 targets and wheels, daemon events, stale embedded generations,
and `cache/zccache/history`. Build history defaults to four days and 1 GiB per
root; active publishers survive, while abandoned unfinished rows do not block
retention forever. Expired database records retain their useful metrics with
archived paths marked unavailable. The first pass
after the zccache#1149 integration removes completed pre-redaction histories.
The daily 30-day absolute bound overrides cook's `keep_per_origin` protection,
so every abandoned cook origin eventually expires.

Builds hold a shared root-maintenance lease from before `BuildSessionStart`
through sanitized archive publication. Maintenance holds the exclusive side
for the complete pass, so a build cannot begin in the probe/delete gap. Daemon
shutdown lets an already-started pass finish. Soldr embeds zccache with
host-owned maintenance, so zccache does not also start its standalone periodic
maintenance scheduler inside the same process.

`soldr status`, `soldr cache`, and `soldr doctor` expose the owning root,
identity, embedded root, effective budget policy, measured usage/fill/free
space, last attempt/success, pressure tier, reclaimed bytes/items, and
component errors. For an orphaned root, the only manual surface is explicit:

```text
soldr gc maintain --root /absolute/path/to/the/root [--json]
```

It refuses relative paths, symlinks/junctions/reparse points, any version-blind
daemon endpoint occupant, and any root whose daemon-ownership lock is busy. It
holds that ownership lock for the full pass and never searches for sibling
roots. Deferred or partially failed manual passes print their structured status
but exit nonzero, so emergency cleanup scripts cannot mistake a no-op for
success.

### Invocation fallback and Cargo-volume GC (issue #323)

The daemon is primary for Soldr-owned state. The cargo front door retains its
post-build detached fallback for Cargo-owned global caches and registered
workspace targets when free
space on any soldr-relevant volume drops below the configured trigger.
Per volume (Windows: drive letter; Unix: device id):

1. tier 1 — cargo `clean gc` with conservative ages,
2. tier 2 — soldr target purge with `larger_than = 256M` and
   `older_than = max(1h, auto_gc.min_age_secs)`,
3. tier 3 — cargo `clean gc` with `--max-src-age=7d
   --max-crate-age=14d --max-git-co-age=7d`,
4. stop. Anything more aggressive requires explicit
   `soldr gc sweep --aggressive`.

Configure in `~/.soldr/config.toml`:

```toml
[auto_gc]
enabled = true          # opt-out; on by default
trigger_free_gb = 20    # start GC when free space < this
target_free_gb = 30     # stop GC when free space >= this
min_age_secs = 3600     # never touch anything modified within this window
```

Fallback behavior:

- Background-only. A detached `soldr gc auto-sweep` process survives the
  wrapper exit, so the build never blocks waiting for Cargo-volume GC.
- Throttled to ~once per 5 minutes via `~/.soldr/.auto_gc_marker`.
- Set `SOLDR_AUTO_GC_DISABLED=1` to disable for a single invocation
  without editing config.
- Volumes that already have plenty of free space are skipped, even
  when another volume on the same machine is below the trigger.
- Every check that crosses the trigger writes a structured log line to
  `~/.soldr/logs/auto-gc.log`. The file rotates to
  `auto-gc.log.old` once it exceeds 10 MiB.
- Cargo's `.package-cache` mutex serializes auto-GC against any
  in-flight `cargo build`, so concurrent builds and GC don't race.

---

## Per-Build XML Log (issue #1790)

Every managed `soldr cargo ...` build — success or failure, cache enabled or
disabled — writes one self-contained XML file:

```text
~/.soldr/logs/builds/<timestamp>-<sanitized-cwd>.xml
```

The directory is flat and the filename's compact UTC timestamp prefix
(`YYYYMMDDTHHMMSSZ`) sorts lexically = chronologically, so `ls` /
`Get-ChildItem` in that directory is already newest-last. The cwd suffix is a
lowercased, filename-safe slug (non `[a-z0-9]` bytes collapsed to `-`, capped
at 80 chars) so logs from different checkouts of the same repo (or different
repos entirely) don't collide.

The write is always-on and best-effort: a failure to write the log never
fails the build — soldr prints `soldr warning: failed to write build log:
<err>` to stderr and continues.

The format was originally JSON and was converted to XML by owner decision:
the log is dominated by repeated attribute-blocks inside group nodes (one
record per compiled crate, with the same handful of build-settings fields
stamped on every group), which XML's attribute-on-element shape expresses
more naturally than JSON's per-item object repetition. The emitter is
hand-rolled (no new dependency).

### Schema (`schema_version: 1`)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<build schema_version="1" soldr_version="0.8.21" cwd="C:\Users\niteris\dev\soldr2" started_at_ms="0" ended_at_ms="0" duration_ms="0" exit_code="0">
  <args>
    <arg>cargo</arg>
    <arg>build</arg>
    <arg>--release</arg>
  </args>
  <steps>
    <download wall_ms="0" cpu_ms="0">
      <item name="cargo-nextest" source="github-release" started_at_ms="0" duration_ms="0"/>
    </download>
    <compile wall_ms="0" cpu_ms="0" target="x86_64-pc-windows-msvc" profile="release" debug="false" opt_level="3" lto="off">
      <item crate="foo" duration_ms="0" cache="hit"/>
    </compile>
    <link wall_ms="0" cpu_ms="0" derived="true" target="x86_64-pc-windows-msvc" profile="release" debug="false" opt_level="3" lto="off">
      <item crate="foo" duration_ms="0"/>
    </link>
  </steps>
  <totals wall_ms="0" cpu_ms="0" crate_count="0" cache_hits="0" cache_misses="0"/>
</build>
```

- **`<build>` header attributes** — `cwd`, `started_at_ms` / `ended_at_ms` /
  `duration_ms`, `exit_code`. The full invoked argv is the child `<args>`
  element (one `<arg>` per token) rather than an attribute, since argv is
  variable-length.
- **build settings on `<compile>` and `<link>`** — there is no separate
  `<settings>` element. Instead, the derived `[profile.*]` metadata
  (`target`, `profile`, `debug`, `opt_level`, `lto`) — read from the invoked
  flags plus the target `Cargo.toml`'s `[profile.<name>]` table — is stamped
  as attributes on BOTH the `<compile>` and `<link>` group nodes, so each
  group is self-describing on its own. `RUSTFLAGS` / `CARGO_PROFILE_*`
  environment overrides are not accounted for in v1.
- **`<steps><download>`** — any tool/artifact fetches (e.g. `cargo-nextest`,
  `zccache`) that happened during the build, one self-closing `<item>` per
  fetch with `name`, `source` (`"github-release"`, `"crates-io"`,
  `"catalogue"`, etc.), `started_at_ms`, and `duration_ms`.
- **`<steps><compile>`** — one `<item>` per crate compiled during the
  session (`crate`, `duration_ms`, `cache`: `"hit"` / `"miss"` /
  `"unknown"`), sourced from the daemon's build-history DB and
  cross-referenced against the zccache compile journal for cache outcome.
- **`<steps><link>`** — v1 has no independently-measured link phase, so this
  section is *derived*: the compile event with the latest end timestamp is
  treated as the linking crate and echoed here with `derived="true"`. That
  crate's entry therefore also appears in `<compile>` — this is
  intentional, not a double-count bug.
- **`wall_ms` vs `cpu_ms`** — `wall_ms` is calendar time spanning the
  earliest start to the latest end within a group. `cpu_ms` is
  *aggregate busy time summed across (possibly parallel) units* — e.g. the
  sum of every compile's duration — **not** OS-reported CPU time. On a
  build with N-way parallelism, a group's `cpu_ms` can comfortably exceed
  its `wall_ms`. Note: `<totals>` `cpu_ms` excludes the derived `<link>`
  group's `cpu_ms` (it re-labels a slice already counted in `<compile>`, so
  adding it again would double-count).
- **`<totals>`** — build-wide `wall_ms` / `cpu_ms` rollup, `crate_count`,
  and `cache_hits` / `cache_misses` (falls back to the zccache session-stats
  summary when no per-crate cache outcome could be resolved from the
  compile journal).
- Empty groups (e.g. no fetches happened) render as a self-closing element
  with only the group's own attributes, e.g. `<download wall_ms="0"
  cpu_ms="0"/>`.
- All attribute values and `<arg>` text content are XML-escaped (`&`, `<`,
  `>`, `"`, `'`, and stray control characters below `0x20`).

### Retention

The same detached `soldr gc auto-sweep` process that runs the Cargo-volume
GC pass above (throttled to ~once per 5 minutes) also prunes
`~/.soldr/logs/builds/` down to the newest 100 files, sorted by filename
(so oldest-timestamp files are removed first). This matches both `*.xml`
files and any legacy `*.json` files left over from interim builds before the
JSON->XML conversion, so old logs in either format still get GC'd. No
separate throttle or opt-out exists for this prune — it rides the existing
auto-GC sweep unconditionally.

---

## Structured JSON Output

The supported JSON protocol currently exists on:

- `soldr status --json`
- `soldr cache --json`
- `soldr cache prune-target <path> --json`
- `soldr version --json`

The JSON response always includes:

- `schema_version`
- `command`

Current schema version:

- `schema_version: 1`

Compatibility rules for schema version `1`:

- existing fields keep their current meaning
- fields may be added in later releases without changing `schema_version`
- removing a field, renaming a field, or changing the meaning/type of an existing field requires a new schema version
- human-readable stdout for commands without `--json` is not covered by this compatibility promise

Example:

```json
{
  "schema_version": 1,
  "command": "version",
  "soldr_version": "0.7.4"
}
```

---

## Help Surface

```text
Usage:
  soldr <COMMAND>
  soldr <TOOL>[@version] [args...]

Commands:
  cargo    Run Cargo through soldr
  status   Show cache status and tool info
  clean    Clear caches
  config   Show or set configuration
  cache    Inspect the compilation cache
  version  Show version
  gc       Review reclaimable Cargo target/ directories; use gc purge to delete
```

---

## Environment Variables

| Variable | Purpose | Default |
|---|---|---|
| `RUSTC_WRAPPER` | Internal build hook used by `soldr cargo ...` | unset |
| `SOLDR_CACHE_ENABLED` | Internal toggle propagated from `soldr cargo ...` into wrapper mode | `1` |
| `SOLDR_RUSTC_WRAPPER` | Replace Soldr's normal embedded-zccache route with another wrapper binary, or disable rustc wrapper injection with `none` / empty while leaving other Soldr front-door behavior intact | unset |
| `SOLDR_REAL_CARGO`, `SOLDR_REAL_RUSTC`, ... | Internal real-tool path overrides used by setup-soldr PATH shims to avoid recursive tool lookup | unset |
| `SOLDR_ZCCACHE_BIN` | Legacy compatibility variable; it does not replace the embedded service on the normal `soldr cargo ...` path. Use `SOLDR_RUSTC_WRAPPER=/path/to/zccache` for an intentional external-wrapper experiment. | unset |
| `SOLDR_ZCCACHE_LOCAL_DIR` | Legacy compatibility variable from the removed downloaded-zccache flow; ignored by the normal embedded path. | unset |
| `SOLDR_CACHE_DIR` | Override the exact product root owned by this soldr daemon. **On Windows, keep this path short** — see the note below. | official builds: `~/.soldr`; development builds: `~/.soldr-dev` |
| `ZCCACHE_CACHE_SIZE_BYTES` | Exact embedded artifact budget in bytes. Mutually exclusive with `ZCCACHE_CACHE_SIZE_PERCENT`. | dynamic 5%, clamped 40–200 GiB |
| `ZCCACHE_CACHE_SIZE_PERCENT` | Embedded artifact budget as an integer percentage from 1 through 100. Mutually exclusive with `ZCCACHE_CACHE_SIZE_BYTES`. | dynamic 5%, clamped 40–200 GiB |
| `SOLDR_CACHE_LIFECYCLE` | Embedded-cache durability policy for `soldr cargo ...`. `job` leaves normal background persistence to the long-lived Soldr daemon. `command` requests an embedded cache flush before the command exits; it does not stop the root-owning daemon. | `job` |
| `SOLDR_CACHE_SHUTDOWN_TIMEOUT_SECS` | Positive compatibility timeout accepted with command-lifetime mode. Embedded mode uses it to select the flush-on-command path; there is no separate zccache daemon to stop. | `30` |
| `SOLDR_TRUST_INHERITED_ENV` | Advanced escape hatch for CI/action workflows that intentionally inject soldr/zccache workspace-pinned env into `soldr cargo ...`. Truthy values are equivalent to `--trust-inherited-soldr-env`; unset/default means `soldr cargo ...` derives a fresh soldr workspace context from the current cwd/manifest while preserving normal OS, Cargo, Rust, proxy, cert, and CI env. | unset |
| `SOLDR_RELOCATED_EXE` | Internal recursion guard set after Windows self-relocation | unset |
| `SOLDR_ORIGINAL_EXE` | Internal path to the original executable when Windows self-relocation is active | unset |
| `SOLDR_ZCCACHE_SESSION_DIR` | Internal session/report directory passed from `soldr cargo ...` into wrapper mode | unset |
| `SOLDR_ZCCACHE_PRIVATE` | Opt-in private auxiliary session/rust-plan root. When truthy (`1`/`true`/`yes`/`on`), `soldr cargo ...` routes that state to `<cwd>/.zccache` and `soldr save`/`soldr hydrate` (`load` alias) default `--cache-dir` to the same path when omitted. It does **not** relocate the compiler-artifact service embedded in `soldr-daemon`; use `SOLDR_CACHE_DIR` for a fully isolated embedded compiler store. Explicit `ZCCACHE_CACHE_DIR` (front door) or `--cache-dir` (save/load) always wins. | unset |
| `SOLDR_SAVE_PROFILE` | Default payload profile for `soldr save` when `--ci` / `--minimal` is not passed. Values: `full`/`default`/`complete` for historical all-files archives, or `ci`/`minimal` for the CI/minimal profile that excludes runtime-only files, zccache runtime binaries, and reports `excluded_files` / `excluded_bytes`. CLI flags win over the env var. | `full` |
| `ZCCACHE_CACHE_DIR` | Auxiliary zccache front-door/session, rust-plan, and direct-rustfmt cache-root override. It does not relocate the compiler service embedded in `soldr-daemon`; use `SOLDR_CACHE_DIR` for that. `soldr cargo ...` ignores inherited values by default so stale workspace state from setup/action wrappers cannot bleed across projects; pass `--trust-inherited-soldr-env` or set `SOLDR_TRUST_INHERITED_ENV=1` only when intentionally injecting this state. | unset |
| `ZCCACHE_SESSION_ID` | Per-build zccache session identifier set by soldr | unset |
| `SOLDR_NATIVE_CACHE` | Native C/C++ compiler cache toggle. Falsy values (`0`/`false`/`no`/`off`) disable only cc-rs `CC`/`CXX` wrapper injection, leaving rustc-side zccache enabled. Useful when a target cross compiler, such as the managed MinGW `gcc.exe` / `g++.exe` path, must run directly while Rust compilation still uses the cache. | unset (on) |
| `SOLDR_CARGO_WAIT_TIMEOUT_SECS` | Opt-in wall-clock watchdog for the Cargo child. Normal `soldr Cargo ...` invocations have no Soldr-imposed wall-clock deadline and may run for hours. A timeout terminates the process tree, records abort diagnostics, and returns failure without changing compile topology. | unset (no deadline) |
| `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` | Overrides the compile-dispatch reply timeout. Default is 30 min so a legitimate slow release compile is never cut off; set a small value (e.g. `30`) to fail fast instead of waiting out the backstop if the daemon stops responding. `0`/empty/unparseable falls back to the default. | 1800 |
| `ZCCACHE_PATH_REMAP` | zccache path-remap mode. soldr seeds `auto` on the child cargo for managed-zccache builds so multiple git worktrees of the same repo share cache hits (issue #352, Tier L1.x). Caller-supplied values are preserved. Works for non-git checkouts too: since zccache#353, `ZCCACHE_PATH_REMAP=auto` with no `.git/` ancestor falls back to the cwd as the remap root and still injects `--remap-path-prefix=<cwd>=.`, so tarball/zip/git-archive checkouts produce path-independent artifacts and share hits (the `.git/` walk is only how the preferred worktree root is discovered). | unset (soldr injects `auto`) |
| `SOLDR_PATH_REMAP` | Escape hatch for the default `ZCCACHE_PATH_REMAP=auto` injection. `off` (case-insensitive) suppresses the injection; any other value, or unset, keeps the default behavior. | unset (`auto`) |
| `SOLDR_TIMESTAMP_LINES` | Prefix each relayed output line with elapsed seconds since soldr start (`  12.34 `), color-preserving, so per-line cost is visible in a log. A single `# t0=<epoch-seconds>` header anchors absolute time. Applies to the cargo front door and the PEP 517 build backend; both stamp piped/non-TTY output by default and leave an interactive terminal unstamped. `1`/`true`/`on` forces stamping on (e.g. on a TTY); `0`/`false`/`off` forces it off. Diagnostic capture, cargo-JSON parsing, and the archived build log always see the unstamped bytes. | unset (on for non-TTY, off for TTY) |
| `SCCACHE_DIR` | sccache cache-root override soldr injects when `SOLDR_RUSTC_WRAPPER=sccache` and the caller has not set it themselves | `~/.soldr/cache/sccache` |
| `SOLDR_PEP517_STABLE_TARGET_DIR` | PEP 517 backend only: set to `0` / `false` / `no` / `off` to skip pinning `CARGO_TARGET_DIR` to the content-identified `<effective-soldr-root>/cargo-target/pep517/<project-id>` namespace for isolated builds (see [PEP 517 Build Backend](#pep-517-build-backend)). A caller-provided `CARGO_TARGET_DIR` always wins regardless. | unset (pin enabled) |
| `SOLDR_PEP517_PROJECT_ID` | Read-only diagnostic/cache identity exported by the PEP 517 backend. It identifies manifests, lockfile, toolchain/configuration, maturin settings, and build-policy environment; source freshness remains Cargo's responsibility. | content-derived |
| `SOLDR_PEP517_STATS` | PEP 517 wheel/editable build diagnostics: `off` (also `0` / `false`) suppresses stderr statistics; `full` emits the cache-session JSON after the default one-line summary; any other nonempty value selects the one-line summary. When unset, verbose frontends detected through `PIP_VERBOSE` or `UV_VERBOSE` select `full`. | concise summary |
| `SOLDR_PEP517_WHEEL_CACHE` | PEP 517 wheel/editable hooks: reuse the last successful wheel when the metadata fingerprint of sources, staged artifacts, prepared metadata, and build settings matches. `off` (also `0` / `false` / `none`) disables reuse. | unset (on) |
| `SOLDR_PEP517_LINKER` | PEP 517 backend only: `auto` (default) tries the fastest supported linker and caches a verified platform-linker fallback after a linker-availability failure; `none` / `default` / `off` disables the automatic attempt. An explicit `SOLDR_LINKER=fast` remains non-fallbacking. | `auto` |
| `SOLDR_LOG` | Log level | `warn` |
| `SOLDR_OFFLINE` | Disable network access for tool fetches | `false` |
| `SOLDR_RUST_PLAN_MEMO` | Default-on: memoize the target-cache preparation subprocess outputs (`cargo metadata --format-version 1`, `rustc -Vv`, `cargo --version`) in a versioned protobuf memo under `<zccache cache dir>/plans/`, keyed by a content-identity hash over the workspace manifests, `Cargo.lock`, hierarchical `.cargo/config*`, metadata passthrough args, toolchain binary identity (path + size + mtime), rust-toolchain pins, rustup `settings.toml`, and the steering env vars (issue #1540). Any key mismatch, decode error, or discovery error falls back to the authoritative subprocesses. Set to a falsy value (`0` / `false` / `no` / `off`) to disable. | unset (on) |
| `SOLDR_FETCH_OVERLAP` | Kill switch for the `soldr build --target <T>` dependency-prefetch overlap (issue #1543). By default the blessed build spawns a best-effort `cargo fetch --target <T>` concurrently with catalogue/SDK preparation so fresh-build time approaches max(fetch, prepare) instead of their sum; the prefetch is joined before the main cargo build spawns and any prefetch failure is logged and ignored. The overlap is automatically skipped for `--offline` / `--frozen` builds, truthy `CARGO_NET_OFFLINE`, or when no `Cargo.lock` exists. Set to `0` / `false` / `no` / `off` (case-insensitive) to disable. | unset (on) |
| `SOLDR_RUST_PLAN_SKIP_WARM_RESTORE` | Default-on: skip `rust-plan restore` when `target/` is already warm from a prior step in the same GitHub Actions job + attempt (issue #229). Set to a falsy value (`0` / `false` / `no` / `off`) to opt out. | unset (on) |
| `SOLDR_DYLINT_PREPARE_TTL_SECS` | Freshness window (seconds) for the dylint prepared-toolchain marker under `<soldr root>/dylint/prepared/v1/`. A fresh, valid marker lets a warm top-level `soldr cargo dylint` skip the nightly-map HTTP fetch and the `rustup component list` / `rustc -vV` probes entirely. `0` means never trust the marker (every run pays the full cold path). | `86400` (24 h) |
| `SOLDR_DYLINT_REVERIFY` | Truthy (`1`/`true`) bypasses the dylint prepared-toolchain marker and always re-runs the full catalogue-fetch + rustup verification path. Use after manually mutating the nightly toolchain or when diagnosing identity mismatches. | unset |
| `SOLDR_SOURCE_BUILD_CACHE` | Falsy (`0`/`false`/`no`/`off`) restores the historical fully-uncached source-build spawn. By default `soldr build-from-source` and dylint source preparation route compiler work through Soldr so fresh machines can reuse cached objects. | unset (cached) |
| `SOLDR_TOOLCHAIN_BIN_CACHE` | `off` (case-insensitive) disables the in-process memo and on-disk cache (`<soldr root>/cache/toolchain-bins/v2/<rustup-home+host-scope>/<channel>/<tool>.path`) for channel-scoped `rustup which` binary resolution. The cache saves one `rustup which` subprocess spawn per tool per nested cargo-dylint re-entry; entries self-invalidate when the cached path no longer exists, and the v2 scope prevents one toolchain home or host architecture from reusing another's path. | unset (on) |
| `DYLINT_DRIVER_PATH` | Soldr sets this on the dylint child process tree to `<soldr root>/dylint/drivers` (a stable soldr-owned home for cargo-dylint's per-toolchain driver builds) **only when the caller has not already set it** — an explicit caller value always wins. A fixed path means warm runs reuse the already-built driver and CI caches have a deterministic path to restore. | soldr-injected |
| `SOLDR_TARGET_CACHE_MODE` | **Master toggle for target-cache.** `thin` enables thin-slice mode (zccache saves the rmeta/dep-info skeleton, restores it before `cargo build`). `full` enables full-target mode (zccache saves+restores the entire `target/` tree). `off` / `false` / `0` / `no` / empty / unset disables the feature entirely — `maybe_prepare_rust_artifact_plan` short-circuits to `Ok(None)` and no surrounding code (front-door, GC, registry) does any "free" work on this path. Designed for CI runners that can persist `~/.cache/zccache/` across runs; **off by default** because a local dev machine has nothing to save it to. CI workflows that want it set this explicitly (typically via `setup-soldr`). See [Target cache](#target-cache-default-off) for the contract. | unset (off) |
| `SOLDR_TARGET_CACHE_TAR_THREADS` | Reader-thread count for the target-cache tar walk in zccache, AND for soldr's own thin-slice manifest walk (issue #272). `auto` lets each side pick a vCPU-bounded count (capped at 8). `1` disables parallelism (sequential walk). Any positive integer sets an explicit count, clamped to `[1, 8]` on the soldr side. soldr validates the value at the cargo front door and uses it when statting bundle files for the `manifest.v2.json` thin-slice manifest; the bulk multi-GB `target/` tar walk lives in zccache. | unset (`auto`) |
| `SOLDR_LINKER` | Pick the linker injected for `soldr cargo ...` builds (issue #285). Accepted values: `default` (no injection — keep the rust-toolchain default), `ld` (system linker — also no injection on every supported platform), `mold` (Linux only; hard error elsewhere), `rust-lld` (Windows MSVC and Linux/MinGW via `clang -fuse-ld=lld`; **no-op on macOS** — see below), `fast` (mold on Linux when present on `PATH`, otherwise rust-lld; rust-lld on Windows MSVC; **no-op on macOS** — see below). The choice resolves to `CARGO_TARGET_<TRIPLE>_LINKER` and `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` injected into the spawned cargo process; the active target is the same one Cargo would pick (`--target` flag, `CARGO_BUILD_TARGET`, or the host triple). A `linker = "..."` field in `~/.soldr/config.toml` is honored when the env var is unset. On macOS targets `rust-lld` and `fast` fall back silently to the platform default linker (issue #509): Apple clang only accepts `-fuse-ld=lld` when the toolchain wires up an `ld64.lld` shim, and stock macOS toolchains do not — injecting it would break even `cc-rs` build-script compilations. | unset |
| `SOLDR_QUIET_DEFENDER` | Suppress the once-per-day pre-build warning emitted by the cargo front door when Defender is actively scanning the soldr cache directory (issue #358). Truthy values silence the warning; the warning is also automatically suppressed in CI environments. | unset |
| `SOLDR_OPTIMIZE_HELPER_OUTPUT` | Internal: set by the parent soldr process when it re-launches itself elevated via `--as-elevated-helper`. The elevated child writes its JSON status to this path so the parent can read and propagate it. | unset |
| `SOLDR_TARGET_WARN_FREE_GB` | Free-space threshold (in GiB) below which the host-volume disk watchdog (issue #574) emits a one-line stderr warning before `soldr cargo ...` dispatches the build. The watchdog probes the disk hosting the project's `target/` dir, falling back to CWD when `target/` doesn't exist yet. | `10` |
| `SOLDR_TARGET_BLOCK_FREE_GB` | Free-space threshold (in GiB) below which the host-volume disk watchdog (issue #574) refuses to start the build, pointing the user at `soldr gc target --purge`. Always strictly less than `SOLDR_TARGET_WARN_FREE_GB`; inverted thresholds collapse to "block wins". | `5` |
| `SOLDR_TARGET_AUTO_PRUNE_ENABLED` | Master toggle for the host-volume disk watchdog (issue #574). Falsy values (`0`, `false`, `no`, `off`, empty — case-insensitive) disable the watchdog entirely. | `1` |
| `SOLDR_GC_TARGET_ROOT` | Default walk root for `soldr gc target` (issue #574). The `--root <PATH>` flag always takes precedence. | `~/dev` |
| `SOLDR_TEST_DISK_FREE_BYTES` | Test seam for the watchdog: when set to a `u64` byte count (or `error`), overrides the real `fs2::available_space` probe so tests can drive every threshold edge. Internal — never set this in production. | unset |
| `SOLDR_TEST_FORBID_SOURCE_BUILD` | Test-only tripwire (soldr#2436): truthy values make every source-build chokepoint (`build-from-source` cargo install, toolchain plugin install) error with a distinctive message instead of spawning cargo, so containment tests can prove no implicit compile path is reached. Never set in production. | unset |
| `SOLDR_PROFILE_EXTRACT` | Env-var equivalent of `soldr hydrate --profile-extract` (`load` alias) (issue #575). Any non-empty value other than `0` enables the per-phase profile line on stderr after a load (`zstd_decode`, `tar_parse`, `extract_total`, per-worker job counts, per-file `p50`/`p95`/`p99`). Useful for tuning the parallel-extract worker count against real workloads. | unset |
| `SOLDR_LOAD_WORKERS` | Cap on the parallel-extract worker pool used by `soldr hydrate` (`load` alias) (issue #575). Positive integer; wins over the explicit `--threads` flag. When unset, `--threads` (or rayon's `num_cpus` default) is used. | unset |

**Windows: keep `SOLDR_CACHE_DIR` short.** Under the cache root soldr appends a
fixed staging suffix of roughly 143 characters
(`cache/zccache/daemon-state/embedded-v1/v<VERSION>/staging/<session>/<compile>/`)
before the artifact's own filename. Windows' classic `MAX_PATH` is 260, so a deep
cache root leaves too little budget and the **linker** fails part-way through a
build with, for example:

```text
LINK : fatal error LNK1104: cannot open file '...\staging\...\<crate>-<hash>.dll.lib'
```

That error names a file inside soldr's own cache rather than anything in your
project, which is why soldr calls that out explicitly when it detects it
(soldr#1969). The fix is a shorter root — the default `~/.soldr` is well within
budget; a root nested inside a temp/scratch tree may not be.

`RUSTC_WRAPPER=soldr cargo build` remains a low-level passthrough path, but it
does not create infrastructure. A top-level Soldr front door must first
register the exact daemon image/root route and ensure the singleton broker is
available. Every cacheable wrapper invocation then uses that broker SESSION
route. Broker, daemon, transport, version-skew, retirement, initialization,
and protocol failures are hard failures; there is no direct-daemon or
direct-compiler fallback.

When `SOLDR_RUSTC_WRAPPER` is set to a non-empty value such as `sccache`, soldr puts that binary in the wrapper slot instead of itself, bypassing the embedded path entirely. If it is set to `none` or an empty string, soldr leaves `RUSTC_WRAPPER` unset for that build.

Release/LTO musl validation and daemon-failure diagnostics are recorded in
[`DATALAKE_RELEASE_MUSL.md`](DATALAKE_RELEASE_MUSL.md).

On the normal embedded path, `soldr cargo ...` resolves a fresh Soldr
workspace context by default. It preserves normal process environment used by
Cargo, Rust, proxies, certificates, CI, and platform SDKs, but ignores
inherited Soldr/zccache workspace-pinned state such as `ZCCACHE_CACHE_DIR`,
`SOLDR_TARGET_CACHE_*`, `SOLDR_TARGET_REGISTRY_RECORDED`, and `SETUP_SOLDR_*`.
Pass `--trust-inherited-soldr-env` or set `SOLDR_TRUST_INHERITED_ENV=1` only
for advanced workflows that intentionally inject those values. Custom wrapper
modes leave caller-provided wrapper environment alone; when
`SOLDR_RUSTC_WRAPPER=sccache` and the caller has set `SCCACHE_DIR` themselves,
Soldr forwards their value rather than overriding it.

Compile-capable Soldr front doors register the exact daemon image, root, and
route service before starting the build tool. Non-compiling commands do not
request a daemon route.

Daemon recovery uses that registered service definition. Wrappers carry only
the route service name and never locate, place, or start a daemon image.

The singleton broker serializes creation per root/version/image route. It
places the image under its stable user-owned root, launches one child, watches
early exit, and returns only after the route-local endpoint answers an active
BackendHandle probe.

Bootstrap cargo-install paths are intentionally uncached. `soldr build-from-source ...` and `[soldr.plugins]` installs from `soldr toolchain prepare` / `ensure` invoke the directly resolved cargo binary and scrub inherited `RUSTC_WRAPPER` / `RUSTC_WORKSPACE_WRAPPER`. Those commands install dev tools and cross-target helper binaries; routing them through Soldr's wrapper slot would make setup recursively depend on the cache layer it is preparing.

`rustdoc` is intentionally not a zccache driver route today. Direct `soldr rustdoc ...` invocations and `rustdoc` PATH shims resolve the toolchain `rustdoc` binary and run it directly. `soldr cargo doc`, `soldr doc`, and doc tests still run with `RUSTC_WRAPPER=soldr`, so rustc dependency compile units remain cached; only the rustdoc driver phase itself is uncached because the embedded zccache runtime has no rustdoc parser/route.


`rust-analyzer` is launched as the real toolchain language server, not as a cache driver. When caching is enabled, `soldr rust-analyzer ...` gives the server process Soldr's cache policy plus a scoped child PATH shim so language-server child builds can re-enter the broker-owned route. `SOLDR_DISABLE_CHILD_SHIMS=1` keeps the language server as a direct passthrough when an editor owns its build environment.


Set `SOLDR_CACHE_LIFECYCLE=command` for self-build jobs that need embedded
cache state flushed before a following archive or test step. Command mode
requests a structured checkpoint of the
embedded zccache artifact index, depgraph, metadata cache, compiler hash cache,
system-includes cache, and artifact store, then finalizes session statistics.
An incomplete or timed-out checkpoint is reported as a failure. Command mode
does not run `zccache stop`, does not terminate the root-owning Soldr daemon,
and never changes ownership based on `ZCCACHE_CACHE_DIR`. Use
`soldr cache shutdown` when the whole Soldr daemon must exit.

On Windows, soldr may copy the running `soldr.exe` into `SOLDR_CACHE_DIR/runtime/soldr-self/<version-and-hash>/soldr.exe` and re-run the command from that relocated copy before build orchestration starts. This keeps disposable worktree builds from repeatedly using the worktree-local `soldr.exe` as `RUSTC_WRAPPER`. The trampoline sets `SOLDR_RELOCATED_EXE=1` and `SOLDR_ORIGINAL_EXE=<original path>` as a recursion guard and preserves argv, inherited environment, stdio, and exit status. Stale relocated copies are purged by a best-effort runtime GC step that runs periodically and skips copies that cannot be removed because they are still locked.

`SOLDR_RUST_PLAN_SKIP_WARM_RESTORE` is a default-on short-circuit for the `rust-plan restore` step. After a successful `rust-plan save`, soldr writes a sentinel next to the thin-slice bundle recording the plan inputs hash, target dir, `GITHUB_RUN_ID`, `GITHUB_JOB`, `GITHUB_RUN_ATTEMPT`, zccache session id, and a unix timestamp. On the next invocation, if the sentinel exists and every match field equals the current value — and the sentinel is no older than 5 minutes — soldr skips `rust-plan restore` and leaves the already-warm `target/` tree untouched. This avoids invalidating Cargo's mtime-based fingerprints when split CI steps share a checkout but spawn fresh shells per step (issue #229). The flag is enabled when unset; set it to a falsy value (`0`, `false`, `no`, `off`, or empty, case-insensitive) to opt out, and any other value (including the historical truthy spellings `1`, `true`, `yes`, `on`) keeps the short-circuit enabled. The gate is conservative: a missing, stale, or partially-mismatched sentinel falls through to the normal restore, so the short-circuit can never make a build less correct than the default path. Promoted to default-on after the #229 CI validation runs (PRs #247, #257, #260, #261, #262) landed cleanly on `main`.

As of issue #1529, the cache-resident sentinel is only half of the proof. A
successful save also writes a paired `.soldr-warm-restore.json` generation
marker inside the live target directory. The next invocation skips restore
only when both records parse and their unique generation id and plan hash
match. `cargo clean` or whole-target deletion removes the marker naturally;
missing, partial, corrupt, stale, and mismatched markers force the normal
restore without a target-tree walk.

### Target cache (default off)

soldr's **target-cache** (also called the "rust-plan" path) save/restores
`target/` artifacts via zccache so a fresh CI runner with a populated cache root
can skip recompilation. It is **off by default** — the planner short-circuits
the moment `SOLDR_TARGET_CACHE_MODE` is empty/unset and never touches cargo
metadata, the tar walker, or the per-build registry. CI workflows that want it
(typically through `setup-soldr`) set the env var explicitly to `thin` or
`full`.

Activation contract (verified against `rust_plan.rs` + `cargo_front_door/cache_plan.rs`
in soldr#784):

* `rust_artifact_cache_mode_from_env()` returns `Ok(None)` on the unset/off
  branch. `maybe_prepare_rust_artifact_plan` short-circuits to `Ok(None)`
  immediately — no `cargo metadata`, no `RustArtifactPlanContext` allocation,
  no env-var fan-out.
* `restore_rust_artifacts` is `let Some(plan) = … else { return Ok(()) }` — a
  pure stat-free no-op when the plan is absent.
* `save_rust_artifacts` is `if let Some(plan) = …` — same no-op shape.
* `target_dir_for_hooks` falls through to `super::resolve_target_dir_for_hooks`,
  which only walks `--target-dir` / `CARGO_TARGET_DIR` / workspace lookup. The
  watchdog hook at `cargo_front_door/mod.rs` therefore sees the same target
  dir resolution regardless of target-cache state.

The corollary: a local dev machine running `soldr cargo build` with no
`setup-soldr` involvement pays zero overhead for target-cache. The feature is
opt-in by design.

If a contributor runs `setup-soldr` locally (e.g. via `act` or a docker
reproducer), the env vars get exported and the target-cache code DOES fire —
that's intentional ("reproduce CI exactly"). To suppress it manually, unset
`SOLDR_TARGET_CACHE_MODE` or set it to `off` before invoking `soldr cargo …`.

---

## Cache Layout

```text
~/.soldr/
|-- bin/
|   `-- <tool>-<version>/
|-- cache/
|   |-- zccache/
|   |   |-- daemon-state/
|   |   |   `-- embedded-v1/
|   |   |       `-- v<VERSION>/ # embedded artifacts, indexes, logs, journal
|   |   `-- history/<build-id>/ # per-build session reports and journal tails
|   `-- sccache/   # injected when SOLDR_RUSTC_WRAPPER=sccache and SCCACHE_DIR is unset
|-- cargo-target/pep517/ # stable per-project PEP 517 Cargo targets
|-- pep517/wheels/ # last successful wheels, bounded by daemon maintenance
|-- runtime/
|   `-- soldr-self/ # Windows self-relocated soldr.exe copies plus periodic GC marker
|-- config.toml
|-- state.sqlite3             # SQLite state store, including tracked target/ dirs
|-- .gc_warning_marker     # last-emitted timestamp for the stale-target startup warning
`-- daemon.*
```

Both wrapper-cache subdirectories live entirely under the soldr-owned cache root so they never collide with a user-managed `~/.zccache` or the system-default `sccache` location on the same machine.

---

## Linux artifact glibc floors

Which Linux artifact runs on an old distro is **not** one number. The two
download routes are built by different toolchains and land on different
floors, and the release archive's floor is set by binaries soldr does not
compile at all.

Measured with `readelf -V` on the published **v0.8.30** assets:

| artifact | built by | floor |
|---|---|---|
| `soldr-…-x86_64-unknown-linux-gnu.tar.zst` → `soldr` | `soldr build` | GLIBC 2.39 |
| …the same archive's `crgx` | fetched prebuilt | GLIBC 2.39 |
| …the same archive's `cargo-chef` | fetched prebuilt | GLIBC 2.39 |
| `soldr-…-manylinux_2_17_x86_64.whl` → `soldr` | `maturin build --zig` | **GLIBC 2.17** |
| any `*-unknown-linux-musl` artifact | static | n/a |

Three things follow, and the second is the one that surprises people.

**The wheel really is manylinux_2_17.** `release-auto.yml` passes
`--zig --compatibility manylinux_2_17` on the wheel lanes, so cargo-zigbuild
links the embedded binary against a 2.17 baseline. The tag is enforced by
`verify_wheel_glibc.py`, not merely asserted. On a distro too old for the
archive, `pip install soldr` is the route that works.

**The archive's floor is capped by `crgx` and `cargo-chef`.** Those are
fetched prebuilt from the soldr-toolchain catalogue, so no change to how
soldr builds *itself* can lower them — it needs an upstream republish. Even
once `soldr` and `soldr-daemon` improve, an archive bundling 2.39 binaries
does not fully run on RHEL 8 (2.28) or Debian 10 (2.28). Measure the archive,
not just `soldr`, when answering "does the release run here".

**soldr's own binaries improve after v0.8.30.** That release was tagged
before soldr#2157, so its 2.39 reflects the old host-glibc link rather than
current behaviour. GNU Linux targets now select the catalogue-backed glibc
2.17 sysroot. The release ratchet remains until a real release run
confirms the measured floor.

### The directive: glibc 2.17, everywhere

**The target floor for every Linux `-gnu` artifact is glibc 2.17.** The 2.28
and 2.39 numbers above are the *current measured state*, not the goal, and the
ratchets that encode them are waypoints to be lowered — not a settled policy.

This is one requirement spanning three repos, because the archive's floor is
the **highest** floor of any binary inside it:

| repo | obligation |
|---|---|
| `zackees/forge` | recipes build against glibc 2.17 — this is where the tools are compiled |
| `zackees/soldr-toolchain` | catalogued `-gnu` assets measure 2.17 or lower; deps above it get recompiled |
| `zackees/soldr` (here) | own binaries reach 2.17; ratchets lowered once measured |

Fixing only soldr's own binaries does **not** deliver a 2.17 archive:
`crgx` and `cargo-chef` are fetched prebuilt at 2.39 and cap the result on
their own. Those need an upstream rebuild.

**Do not reach for zig or `cargo-zigbuild` to get there.** Both are being
purged in favour of the blessed toolchain, so any 2.17 route built on them is
a dead end. Reaching 2.17 is a matter of *building against an old sysroot*,
which is toolchain-agnostic.

One open question worth settling before designing around either answer:
whether Rust's precompiled `std` blocks 2.17 for a stock build. The evidence
cuts both ways — the wheel lane reaches 2.17 with the same `std`, while
`__cxa_thread_atexit_impl@2.18`, `getrandom@2.25` and `statx@2.28` have been
observed as already-versioned references. Resolve it by building in a
glibc-2.17 container with the pinned rustc and reading `readelf -V`: a 2.17
result means the blessed toolchain needs only an old sysroot, while a 2.28
result means 2.17 requires `-Z build-std` and is a materially larger decision.
Background in soldr#2145 and soldr#1060.

---

## GitHub Actions

```yaml
- name: Build through soldr
  run: soldr cargo build --release
```

For bootstrap verification of another Rust project:

```yaml
- name: Build third-party project through soldr
  run: soldr cargo build --target ${{ matrix.target }}
```

---

## Broker/daemon failure output

A cacheable compile that cannot use its registered broker route fails with an
infrastructure-attributed diagnostic. `soldr doctor`, `soldr status`, and
`soldr logs paths` provide the route, process, timeout, and log evidence. See
[docs/DAEMON_TIMEOUTS.md](DAEMON_TIMEOUTS.md) for bounded recovery steps.

## A restored source file can run a stale binary (soldr#2773)

Restoring a source file so its modification time moves **backwards** makes
`soldr cargo` run the previously compiled binary instead of the source on
disk — no `Compiling` line, no warning, exit 0:

```bash
cp -p src/main.rs main.rs.bak    # backup keeps the OLD mtime
# ... edit src/main.rs, build, observe the new behaviour ...
mv main.rs.bak src/main.rs       # restore; mtime goes backwards
soldr cargo run                  # runs the artifact from the EDITED source
```

The build result and the source tree disagree, and nothing says so.

**Why.** Cargo decides freshness by comparing a source's mtime against its
artifact's. A source that becomes *older* than the artifact looks fresh, so
rustc is never invoked — which means soldr never sees the compile either.
Nothing in soldr's cache is wrong here; the unit is skipped above soldr.

**Which operations do this.** Anything that writes a file with a preserved
timestamp rather than the current time:

| Operation | Moves mtime backwards? |
|---|---|
| `mv` of a `cp -p` / `rsync -a` backup | **yes** |
| `tar -x` (preserves mtimes by default) | **yes** |
| `git checkout` of an older commit | no — sets mtime to now |
| an editor save | no |

So the exposure is concentrated in bisect-style work, stash/restore loops, and
comparing a fix against a backup — exactly the situations where a wrong result
is most likely to be believed.

**Recovering.** Either of these forces the unit to rebuild:

```bash
touch src/main.rs                # cheapest; restores a forward mtime
soldr cargo clean -p <package>   # discards the stale artifact
```

Disabling the compilation cache does **not** help, and it is worth knowing
why: `ZCCACHE_DISABLE=1` (and the deprecated `--no-cache`) govern what happens
*once cargo decides to compile a unit*. Here cargo decides not to, so the unit
never reaches the cache in the first place. Both commands above work because
they change cargo's freshness answer — one by making the source newer than the
artifact, the other by removing the artifact.

**Recognising it.** The tell is a build that prints *no* `Compiling` line when
you expect one, and results that match a version of the source you no longer
have. Diagnostics from the stale artifact are the strongest signal — a warning
naming an import that the file on disk uses, or a test failure that the current
source cannot produce, means the running binary corresponds to no file in the
tree.

Automatic detection is tracked by soldr#2773.

## Summary

The key design rule is simple:

- users build through `soldr cargo ...`
- soldr owns the wrapper slot on the common path
- soldr delegates cache-enabled wrapper invocations over Soldr IPC into the
  zccache service embedded in `soldr-daemon`
- users do not need to manually wire `RUSTC_WRAPPER` for the common path
