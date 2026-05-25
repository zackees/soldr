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

[issue-329]: https://github.com/zackees/soldr/issues/329
[setup-soldr]: https://github.com/zackees/setup-soldr

---

## TL;DR

| You want | Use |
|---|---|
| Linux → Windows GNU | `cargo-zigbuild` + `ziglang` ([Section 1](#1-linux--windows-gnu-via-cargo-zigbuild-recommended)) |
| Linux → Windows MSVC | `cargo-xwin` ([Section 2](#2-linux--windows-msvc-via-cargo-xwin)) |
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

## 4. What soldr deliberately does NOT do

The #329 exploration is explicit about the lines soldr will not cross:

- **No silent `RUSTFLAGS` injection.** soldr does not append linker flags
  or `-C link-arg=...` based on guesswork about your target.
- **No automatic `CC` / `CXX` / linker mutation** without an explicit
  opt-in. If a link fails because the wrong C compiler is on `PATH`,
  the error stays pointing at the missing linker.
- **No system-package installs** (mingw-w64, `binutils-mingw-w64-x86-64`,
  etc.) from inside soldr. Those belong to your distro's package manager
  or to a more permissive CI shim.

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
