# Cross-compiling with soldr

This guide documents the currently-working recipes for cross-compiling
between Linux and Windows targets through soldr. It addresses the
exploration tracked in [issue #329][issue-329].

soldr is the strict half of the soldr/setup-soldr pair: it will not silently
inject `RUSTFLAGS`, mutate `CC`/`CXX`, or install system packages on your
behalf. The recipes below stay inside soldr's managed environment — your
project pins the cross-compile toolchain via `rust-toolchain.toml` and soldr
materializes it on demand.

If you want a more permissive, CI-side experience that *can* install system
packages, see [`zackees/setup-soldr`][setup-soldr]'s `cross-targets:` input
instead.

**Windows hosts:** the bash-flavored docker-prebuilt + cross-compile recipes
on this page assume Git Bash / MSYS2 / Cygwin / WSL on PATH. soldr does NOT
bootstrap the POSIX shell layer — see [`docs/WINDOWS_PREREQS.md`](WINDOWS_PREREQS.md)
for the install matrix + the common `cygpath: command not found` / docker
bind-mount error → fix mapping ([soldr#885](https://github.com/zackees/soldr/issues/885)).

[issue-329]: https://github.com/zackees/soldr/issues/329
[setup-soldr]: https://github.com/zackees/setup-soldr

---

## TL;DR

| You want | Use |
|---|---|
| Linux → Windows GNU | `cargo-zigbuild` + `ziglang` ([Section 1](#1-linux--windows-gnu-via-cargo-zigbuild-recommended)) |
| Windows → Windows GNU | managed MinGW-w64 GCC ([Section 1b](#1b-windows--windows-gnu-via-managed-mingw-w64-gcc)) |
| Linux → Windows MSVC | `cargo-xwin` ([Section 2](#2-linux--windows-msvc-via-cargo-xwin)) |
| **Windows -> Linux** | `cargo-zigbuild` ([Section 1a](#1a-windows--linux-via-cargo-zigbuild-and-macos-via-soldr-build-soldr988soldr1425)) |
| **Windows/Linux -> Mac** | `soldr build` + target-shaped Apple SDK ([Section 1a](#1a-windows--linux-via-cargo-zigbuild-and-macos-via-soldr-build-soldr988soldr1425)) |
| Declare cross targets up-front | `[toolchain].targets` + `[soldr.plugins]` ([Section 3](#3-pinned-host-triples-per-project-current-state)) |

---

## 1. Linux → Windows GNU via `cargo-zigbuild` (recommended)

`cargo-zigbuild` shells out to `zig cc` as the C linker, which means a single
zig install gives you GNU-ABI Windows binaries from any Linux host without
mingw-w64 or a glibc-version dance.

### `rust-toolchain.toml`

```toml
[toolchain]
channel = "1.94.1"
targets = ["x86_64-pc-windows-gnu"]

[soldr.plugins]
cargo-zigbuild = { version = "0.22", locked = true }
```

### Bootstrap and build

```sh
# Installs the pinned rust toolchain, adds the cross target,
# and `cargo install`s cargo-zigbuild into soldr-managed $CARGO_HOME.
soldr toolchain prepare

# zig itself is not yet soldr-managed; install it system-wide
# (or into a venv) so cargo-zigbuild can shell out to it.
pip install ziglang

# Cross-build through soldr (caches via zccache like a normal build).
soldr cargo zigbuild --release --target x86_64-pc-windows-gnu
```

### Notes

- `pip install ziglang` is **currently a system-level install** — there is
  no soldr-managed `zig` yet. This is tracked as a follow-up to the #329
  exploration.
- `cargo-zigbuild` is a normal `cargo-<sub>` extension, so `soldr cargo
  zigbuild ...` flows through the same cargo front door (and the same
  zccache wrapper) as `soldr cargo build`.
- A pre-built fetch of `cargo-zigbuild` via `known_tools` is deferred:
  upstream ships `.tar.xz` archives and soldr's extractor does not yet
  handle that format. Until then, `[soldr.plugins]` performs a `cargo
  install` on first `soldr toolchain prepare`.

---

## 1a. Windows -> Linux via `cargo-zigbuild` and macOS via `soldr build` (soldr#988/soldr#1425)

Windows contributors can produce Linux and macOS binaries locally instead of
pushing a branch and waiting on CI, but the two target families now use
different blessed build surfaces:

- Linux cross targets still use explicit `soldr cargo zigbuild`.
- macOS targets use `soldr build --target <apple-triple>`. That path resolves
  the target-aware Apple SDK row and injects clang/SDK env internally. Direct
  `soldr cargo zigbuild --target *-apple-darwin` is a legacy/diagnostic path,
  not the default macOS recipe.

### Recipe

```powershell
# Pin the cross targets in rust-toolchain.toml (same shape as Section 1):
#   [toolchain]
#   targets = ["x86_64-unknown-linux-gnu", "x86_64-apple-darwin", "aarch64-apple-darwin"]
#
# Linux target: materialize zig/cargo-zigbuild, then build through zigbuild.
soldr prepare --target x86_64-unknown-linux-gnu
soldr cargo zigbuild --target x86_64-unknown-linux-gnu --release -p soldr-cli

# macOS targets: soldr build is the blessed path. `prepare` is optional and
# useful only when a later external/legacy command needs SDKROOT exported.
soldr prepare --target x86_64-apple-darwin
soldr build --target x86_64-apple-darwin --release -p soldr-cli
soldr build --target aarch64-apple-darwin --release -p soldr-cli
```

### What soldr handles automatically

- `cargo-zigbuild` and `zig` install for Linux zigbuild targets (fetched from
  the soldr-toolchain catalogue per `SOLDR_TOOLCHAIN_ORIGIN`).
- Apple SDK fetch for `*-apple-darwin` targets. Auto shape maps
  `x86_64-apple-darwin` to `darwin-x86_64` and `aarch64-apple-darwin` to
  `darwin-aarch64`; `soldr build` applies the SDK env internally.

### Apple SDK version + shape (soldr-toolchain#14)

The Apple SDK soldr fetches is pinned per-target via two env vars
(both optional):

| Env var | Values | Default | Effect |
|---|---|---|---|
| `SOLDR_APPLE_SDK_VERSION` | `11.3`, `13.3`, `14.5`, `15.2` | `14.5` | Which macOS SDK to vendor (catalogue row selection). |
| `SOLDR_APPLE_SDK_SHAPE` | `universal2`, `thin-x86_64`, `thin-aarch64`, `auto` | `auto` | Whether to fetch the fat universal2 artifact or a lipo-thinned per-arch slice. |

`auto` (the default) picks the **thin variant matching the target
triple's arch** when cross-compiling for one Apple arch, falling
back to `universal2` otherwise. Examples:

```powershell
# Project targets only Apple Silicon: fetch the thin SDK slice
$env:SOLDR_APPLE_SDK_VERSION = "14.5"
$env:SOLDR_APPLE_SDK_SHAPE   = "thin-aarch64"
soldr build --target aarch64-apple-darwin --release -p soldr-cli

# Project targets both Apple archs from one cache: one fat artifact
$env:SOLDR_APPLE_SDK_SHAPE = "universal2"
soldr build --target x86_64-apple-darwin  --release -p soldr-cli
soldr build --target aarch64-apple-darwin --release -p soldr-cli
```

The available `(version, shape)` rows live in the soldr-toolchain
catalogue at `https://zackees.github.io/soldr-toolchain/catalogue.v1.json`
under the URL pattern `/apple-sdk/<version>/<shape-slug>/`. The historical
11.3 row keeps its legacy `/apple-sdk/MacOSX11.3/darwin-universal2/` layout;
modern rows use the bare version plus shape slug.

### CI

The reusable workflow `.github/workflows/_cross-build-windows-host.yml`
runs this exact recipe on a `windows-2022` runner. The
`cross-build-from-windows-x64-linux` job in `ci.yml` exercises it on every
PR with `target = x86_64-unknown-linux-gnu` as the regression test.

[Section 1](#1-linux--windows-gnu-via-cargo-zigbuild-recommended) covers
the Linux → Windows-GNU mirror of this recipe.

---

## 1b. Windows → Windows GNU via managed MinGW-w64 GCC

On Windows x64 hosts, `soldr prepare --target x86_64-pc-windows-gnu`
downloads a pinned WinLibs MinGW-w64 GCC bundle from the soldr-toolchain
catalogue, prepends its `bin/` directory to `PATH`, and exports the
target-scoped Cargo/cc-rs variables needed by build scripts:
`CC_x86_64_pc_windows_gnu`, `CXX_x86_64_pc_windows_gnu`,
`AR_x86_64_pc_windows_gnu`, `RANLIB_x86_64_pc_windows_gnu`,
`WINDRES_x86_64_pc_windows_gnu`, and
`CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER`.

### Recipe

```powershell
rustup target add x86_64-pc-windows-gnu

soldr prepare --target x86_64-pc-windows-gnu
where gcc
gcc --version

soldr build --target x86_64-pc-windows-gnu --release
```

In GitHub Actions, pass `--github-env $env:GITHUB_ENV` so later steps
inherit the same compiler/linker environment:

```powershell
soldr prepare --target x86_64-pc-windows-gnu --github-env $env:GITHUB_ENV
```

Scope is intentionally narrow: first-class managed MinGW provisioning
currently supports only `x86_64-pc-windows-gnu` on Windows x64 hosts.
`i686-pc-windows-gnu`, `aarch64-pc-windows-gnullvm`, and other Windows
GNU-family targets are follow-ups. Linux hosts should keep using
[Section 1](#1-linux--windows-gnu-via-cargo-zigbuild-recommended).

---

## 2. Linux → Windows MSVC via `cargo-xwin`

`cargo-xwin` downloads the Microsoft CRT and Windows SDK headers/libs on
first invocation, then sets up the link step so you can produce MSVC-ABI
binaries from a Linux host.

### `rust-toolchain.toml`

```toml
[toolchain]
channel = "1.94.1"
targets = ["x86_64-pc-windows-msvc"]

[soldr.plugins]
cargo-xwin = { version = "0.18", locked = true }
```

### Bootstrap and build

```sh
soldr toolchain prepare
soldr cargo xwin build --release --target x86_64-pc-windows-msvc
```

### MSVC EULA

`cargo-xwin` auto-downloads the MSVC CRT and Windows SDK from Microsoft's
servers on first run. By using it, **you accept Microsoft's MSVC
license** implicitly. soldr does not display the EULA; if that matters
for your distribution, read upstream's
[license discussion][xwin-license] before adopting the recipe.

[xwin-license]: https://github.com/rust-cross/cargo-xwin#license

### Useful env vars (cargo-xwin)

| Var | Purpose |
|---|---|
| `XWIN_ARCH` | Target architecture set to download (`x86_64`, `aarch64`, `x86`) |
| `XWIN_CACHE_DIR` | Where to cache the downloaded CRT/SDK |
| `XWIN_INCLUDE_ATL` | Include ATL headers (off by default) |
| `XWIN_VARIANT` | Pick a specific SDK variant when multiple match |

See [`rust-cross/cargo-xwin`][cargo-xwin] for the complete and current list.

[cargo-xwin]: https://github.com/rust-cross/cargo-xwin

---

## 3. Pinned host triples per project (current state)

Today, declaring a cross target involves two `rust-toolchain.toml` blocks:

1. **`[toolchain].targets`** — tells `soldr toolchain prepare` to
   `rustup target add` the rust-std for that triple.
2. **`[soldr.plugins]`** — tells `soldr toolchain prepare` to install the
   cargo extension that drives the cross link (`cargo-zigbuild`,
   `cargo-xwin`, etc.).

Putting both in `rust-toolchain.toml` means a fresh clone needs only:

```sh
soldr toolchain prepare
```

…to be ready to cross-compile, with the exact same versions as every other
developer on the project.

### Future enhancement (out of scope for #329)

A unified `[soldr.cross-targets]` block could collapse the two declarations
into one — `soldr toolchain prepare` would derive both the `rust-std`
install and the matching cargo-extension install from a single entry. That
work is intentionally **not** part of this exploration; if you need it,
file a follow-up issue referencing #329.

---

## 3.5. ring + `aarch64-pc-windows-msvc` requires `soldr build`

Crates depending on `ring 0.17.x` (rustls, reqwest's TLS, …) include
hand-written ARM assembly at `pregenerated/sha256-armv8-win64.S` etc.
ring's `build.rs` hardcodes `c.compiler("clang")` for windows-msvc and
shells out to `clang` directly. On `aarch64-pc-windows-msvc` plain
`clang` rejects this — soldr installs a multicall `clang` shim
(hardlink/copy of `soldr`) that intercepts the call and re-execs
`clang-cl` with the same argv.

**Use the blessed surface**: `soldr build --target
aarch64-pc-windows-msvc` (soldr#1012, #882). It auto-dispatches to
cargo-xwin AND installs the shim at `~/.soldr/bin/clang-shim/` ahead
of system clang on `PATH`.

### Direct cargo-xwin path is unsupported

Bypassing `soldr build` (e.g. inside `messense/cargo-xwin:0.23.0`
docker, or a workflow that doesn't go through soldr) means the shim
isn't on `PATH` and the build fails:

```
error occurred in cc-rs: command did not execute successfully
LC_ALL=C clang ... --target=aarch64-pc-windows-msvc ...
  -c ring-0.17.14/pregenerated/sha256-armv8-win64.S
```

To use the shim outside soldr-managed shells: install soldr
(`pip install soldr` / `npm install -g @zackees/soldr`), then run
`soldr build --target aarch64-pc-windows-msvc --help` once to trigger
shim install. After that, add `$HOME/.soldr/bin/clang-shim` to `PATH`
manually and any downstream tool's clang invocation resolves to the
shim.

### Why not upstream?

Upstream-able in principle, but ring's `c.compiler("clang")` is
intentional and cargo-xwin's `CC_*=clang-cl` gets overridden by
cc-rs's `compiler_family()` probe. The shim is the surgical fix that
lives in soldr's toolchain story. See soldr#886.

---

## 4. What soldr deliberately does NOT do

The #329 exploration is explicit about the lines soldr will not cross:

- **No silent `RUSTFLAGS` injection.** soldr does not append linker flags
  or `-C link-arg=...` based on guesswork about your target.
- **No hidden `CC` / `CXX` / linker mutation.** `soldr prepare` may export
  target-scoped env for a supported target, such as managed MinGW-w64 GCC
  for `x86_64-pc-windows-gnu`, but only on an explicit `--target`.
- **No system-package installs** (`mingw-w64`, `binutils-mingw-w64-x86-64`,
  etc.) from inside soldr. Managed toolchains are downloaded into soldr's
  cache instead of being installed into the host OS.

For the CI side, [`zackees/setup-soldr`][setup-soldr] takes liberties that
soldr itself will not — including a `cross-targets:` input that can install
the system bits on a GitHub-hosted runner.

---

## 5. Status of issue #329 sub-items

| Sub-item | Status |
|---|---|
| 1. First-class `zigbuild` path | Works via `[soldr.plugins]` ([Section 1](#1-linux--windows-gnu-via-cargo-zigbuild-recommended)). Pre-built fetch via `known_tools` deferred — needs `.tar.xz` extractor support. |
| 2. Documented `xwin` recipe | This file ([Section 2](#2-linux--windows-msvc-via-cargo-xwin)). |
| 3. Pinned host triples per project | Works via `[toolchain].targets` + `[soldr.plugins]` ([Section 3](#3-pinned-host-triples-per-project-current-state)). A unified `[soldr.cross-targets]` block is deferred. |

---

## See also

- [`README.md`](../README.md) — "Native vs cross targets" covers same-ABI
  cross targets that only need a rust-std install.
- [`docs/API.md`](API.md) — full CLI reference, including `soldr toolchain
  prepare`.
- [`CLAUDE.md`](../CLAUDE.md) — `[soldr.plugins]` manifest example and
  contributor guidance.
