# Debug-symbol sidecars in release archives

Policy for the debug-symbol files (`.pdb`, `.dSYM/`, `.dwp`) that ride
along inside the per-target release archives
(`soldr-vX.Y.Z-<triple>.tar.zst`). Established by issue #786; the
staging implementation lives in `release-auto.yml` and the manifest
format in `crates/soldr-cli/src/release_sidecar.rs`.

## Contract

- **The archive carries every debug-symbol sidecar the platform build
  actually emits** for the released `soldr` binary, colocated with the
  binary so debuggers resolve them without a symbol server.
- **`manifest.json` records each sidecar** under `soldr.debug_info` as
  `{ "name", "sha256", "format" }`, where `format` is one of `pdb`,
  `dsym`, `dwp` (`release_sidecar::DebugSidecarFormat::as_manifest_str`).
- **An empty `debug_info: []` is a valid state**, not an error: it means
  the toolchain emitted no split-debug artifact for that target (see the
  per-platform table). Consumers that only need the runtime binaries can
  ignore the field entirely — archives without sidecar metadata remain
  consumable (backward compatibility with pre-#786 archives).

## Per-platform policy

| Platform | Sidecar | Emitted today? | Why |
|---|---|---|---|
| Windows MSVC (x64 + ARM64) | `soldr.pdb` | **Yes — always** | MSVC always produces a PDB next to the binary; the release lane hard-fails if it is missing (`expected a soldr PDB sidecar`). |
| Linux (gnu + musl, x64 + ARM64) | `soldr.dwp` | No | Split DWARF (`-C split-debuginfo=packed`) is not enabled in the release profile; symbols stay embedded in the binary (then get stripped by size trims). Staging is in place — if the profile ever enables split DWARF, the `.dwp` is packaged and recorded automatically. |
| macOS (x64 + ARM64) | `soldr.dSYM/` | No | Same as Linux: dSYM bundles are only produced by `dsymutil`/split-debuginfo, which the release profile does not run today. Staging handles the bundle when present. |

The asymmetry is deliberate: Windows PDBs are a free by-product of the
MSVC link step and are required for `cdb`/WinDbg crash triage (#780,
#782); enabling split-debug on the Unix targets costs build time and
release size for symbol data nobody currently consumes. Revisit by
flipping `split-debuginfo` in the release profile — no workflow change
is needed, the staging + manifest recording pick the artifacts up.

## Verifying an archive

```bash
tar --zstd -tf soldr-vX.Y.Z-x86_64-pc-windows-msvc.tar.zst
# → soldr.exe, soldr-daemon.exe, cargo-chef.exe, crgx.exe,
#   manifest.json, soldr.pdb

tar --zstd -xOf … manifest.json | jq .soldr.debug_info
# → [{ "name": "soldr.pdb", "sha256": "…", "format": "pdb" }]
```

The sha256 in `debug_info` is computed over the packaged sidecar file
and can be used to pair a crash dump's binary with the exact symbol
file from the matching release.

## Bundled third-party binaries

`zccache` is embedded in the `soldr` binary itself (soldr#1368), so its
symbols are part of `soldr.pdb`. `crgx` and `cargo-chef` are prebuilt
upstream fetches — soldr does not (and cannot) generate symbol files
for them; they are out of scope for `debug_info`.

## Local debugging without release sidecars

zccache is linked into the locally built `soldr-daemon`, so build Soldr from a
checkout whose `_vender/zccache` submodule contains the code under test. There
is no external zccache daemon or `SOLDR_ZCCACHE_LOCAL_DIR` symbol-copy path in
the embedded architecture.

> **Windows: a cached build produces no `.pdb` today (soldr#2148).**
>
> This section used to say "debug the resulting Soldr binary and its normal
> `.pdb`". On Windows that instruction silently does not work: a build through
> the compilation cache emits the `.exe` without its `.pdb`, so a minidump
> resolves to `module+0xNNNN` and nothing else.
>
> To get a symbolizable binary, disable the cache for that build:
>
> ```console
> ZCCACHE_DISABLE=1 soldr cargo build --release
> ```
>
> The `.exe` still carries an `RSDS` CodeView record naming the `.pdb` it
> expects, so the file was emitted and then dropped — which is why the failure
> reads as "debug info was never enabled" rather than "debug info was
> discarded". Verified by isolating one variable at a time: `ZCCACHE_DISABLE=1`
> restores the `.pdb` **with soldr's rustc wrapper still active**, and the
> `SOLDR_LINKER` choice makes no difference, so this is the cache layer and not
> the wrapper or the blessed-prep linker substitution.
>
> The cause is in the vendored zccache submodule. The rustc path already
> handles *multiple* outputs — the daemon carries `rustc_all_outputs` and
> stages each one — so this is a missing entry, not a missing capability.
>
> The lever is `rustc_expected_output_paths` in
> `zccache-daemon-core/src/daemon/server/rustc.rs`. It enumerates what a rustc
> invocation is expected to produce (the link output, the `--emit` products,
> explicit emit paths) and that list drives staging, capture and replay. It has
> no `.pdb` entry, so the file rustc writes beside the binary is never
> redirected into staging and never stored. The same function already appends a
> conditional extra product for Dylint cdylibs
> (`dylint_library_sidecar_output_path`), which is the shape a `.pdb` entry
> would follow.
>
> Two things still need deciding before writing it, and they are the reason
> this note is not already stale: what happens when a declared output is not
> produced (a build with debuginfo off must not start failing), and keeping the
> `.exe` and `.pdb` stored as a set — a stale `.pdb` beside a fresh `.exe`
> would be worse than none.
>
> Remove this note when soldr#2148 closes.
