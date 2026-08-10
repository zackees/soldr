# GNU-Windows bridge (#580)

Windows builds of this workspace are effectively pinned to
`x86_64-pc-windows-msvc` (see the "Windows Native Build Rules" section of
[`CLAUDE.md`](../CLAUDE.md)). The `running-process-win-gnu-bridge` crate is
the build seam that lets `x86_64-pc-windows-gnu` builds reach the same
**MSVC-obligatory** Windows API surface the rest of the workspace depends
on, without changing the MSVC path.

The guiding principle is **direct where possible, bridge only where
necessary**:

1. **Direct** — where the GNU toolchain can already link the symbol, use it
   as-is. This is the case for the whole ConPTY surface: `windows-sys`
   bundles a per-target import library (`windows-targets` →
   `windows_x86_64_gnu`), so the GNU linker resolves
   `CreatePseudoConsole` / `ResizePseudoConsole` / `ClosePseudoConsole`
   with no Windows SDK and no MSVC `link.exe`.
2. **Bridge** — where a symbol cannot link directly, provide a thin bridge:
   a generated import library (`dlltool` + a `.def` file) or a small C shim
   compiled with the GNU/clang C compiler, exposing a stable Rust-facing
   FFI. No in-scope symbol needs this today; it is the documented fallback.

## Mechanism per API surface

| Surface | Mechanism | Status |
| --- | --- | --- |
| ConPTY (`CreatePseudoConsole` / `ResizePseudoConsole` / `ClosePseudoConsole`) | **direct** (`windows-sys` bundled `-gnu` import lib) | in scope, done |
| `retour` inline detours / DLL injection (`running-process-probe-interposer-windows`) | **direct** (`retour` + `iced-x86` + `windows-sys` all link under `-gnu`) | in scope, done |
| `libsqlite3-sys` bundled (daemon feature) | **direct** (`cc` crate finds MinGW-w64 `gcc.exe` and builds the sqlite amalgamation) | in scope, done |
| `procdump` / DbgHelp minidump (`test-watchdog`) | dev-only, not on the shipped path | out of scope |

## Building and checking the GNU target

```bash
# One-time: install the GNU std for the target.
soldr rustup target add x86_64-pc-windows-gnu

# The bridge crate — the linkability proof point.
soldr cargo check -p running-process-win-gnu-bridge --target x86_64-pc-windows-gnu

# The ConPTY consumer path (client feature).
soldr cargo check -p running-process --no-default-features --features client \
    --target x86_64-pc-windows-gnu

# The daemon path. This is a full build because bundled libsqlite3-sys compiles
# sqlite's C amalgamation and links it into the Rust crate.
soldr cargo build -p running-process --features daemon \
    --target x86_64-pc-windows-gnu

# The file-hook observer interposer path.
soldr cargo build -p running-process-probe-interposer-windows \
    --target x86_64-pc-windows-gnu
soldr cargo build -p running-process-probe --features embed-helper \
    --target x86_64-pc-windows-gnu
soldr cargo test -p running-process-probe --features embed-helper \
    --test interposer_integration_windows \
    --target x86_64-pc-windows-gnu
```

A full **build** (rather than `check`) of the GNU target needs a MinGW-w64
`gcc.exe` on `PATH` for C-dependency build scripts. The daemon feature takes
that path through bundled `libsqlite3-sys`: `rusqlite` enables the sqlite
amalgamation, `libsqlite3-sys` invokes the `cc` crate, and the GNU target
links the resulting object with no vcpkg, Windows SDK, or MSVC `link.exe`.
Use a shell where `gcc --version` reports a MinGW-w64 compiler (for example
MSYS2 `mingw64` or a Chocolatey MinGW install). `check` does not compile or
link that C code, so use the daemon `build` command above when validating
sqlite support. The bridge crate's unit test `conpty_entry_points_are_bound`
asserts the ConPTY entry points resolve to non-null addresses — a runtime
linkability check on any Windows host (MSVC or GNU).

The Windows interposer smoke test builds the DLL and
`testbin-createfilew-probe` for the same active target triple as the test
binary. Under `x86_64-pc-windows-gnu`, it injects the GNU-built DLL via
`CreateRemoteThread(LoadLibraryW)` and waits for a real `RPP_HOOK file-open`
line emitted by the `retour::RawDetour` `CreateFileW` hook. That validates
the GNU ABI path for the inline trampoline, the `iced-x86` prologue decode,
the `VirtualProtect`-backed patch install, and the deferred `DllMain` worker
thread pattern.

## What remains MSVC-only

No shipped `running-process` surface in this bridge list remains MSVC-only
for `x86_64-pc-windows-gnu`: the core/client path, ConPTY bridge proof,
daemon feature with bundled sqlite, and file-hook observer interposer all
build under GNU when the target std and MinGW-w64 `gcc.exe` are available.

The dev-only `test-watchdog` procdump / DbgHelp minidump path remains out of
scope for this bridge work and is not part of the shipped GNU support claim.
The default Windows release/wheel toolchain is still MSVC unless a build
explicitly selects `--target x86_64-pc-windows-gnu`.
