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
| Windows MSVC (x64 + ARM64) | `soldr.pdb` | **Yes — for soldr's own release** | MSVC always produces a PDB next to the binary; the release lane hard-fails if it is missing (`expected a soldr PDB sidecar`). But see the caveat below — that guarantee does not extend to builds that go through the compile cache. |
| Linux (gnu + musl, x64 + ARM64) | `soldr.dwp` | No | Split DWARF (`-C split-debuginfo=packed`) is not enabled in the release profile; symbols stay embedded in the binary (then get stripped by size trims). Staging is in place — if the profile ever enables split DWARF, the `.dwp` is packaged and recorded automatically. |
| macOS (x64 + ARM64) | `soldr.dSYM/` | No | Same as Linux: dSYM bundles are only produced by `dsymutil`/split-debuginfo, which the release profile does not run today. Staging handles the bundle when present. |

The asymmetry is deliberate: Windows PDBs are a free by-product of the
MSVC link step and are required for `cdb`/WinDbg crash triage (#780,
#782); enabling split-debug on the Unix targets costs build time and
release size for symbol data nobody currently consumes. Revisit by
flipping `split-debuginfo` in the release profile — no workflow change
is needed, the staging + manifest recording pick the artifacts up.

## Caveat: the Windows guarantee is about soldr's release, not your build

> [!WARNING]
> A Windows build that goes **through soldr's compile cache** currently
> does *not* keep its `.pdb` — the file is produced and then dropped
> (soldr#2148). Only `--no-cache` / `ZCCACHE_DISABLE=1` builds keep it.

The row above is easy to read as "Windows builds always have symbols".
It holds for soldr's published archive, and only because that lane opts
out of the cache for an unrelated reason — `release-auto.yml` passes
`--no-cache` on `*-pc-windows-msvc`:

```yaml
# Native MSVC release runners are already CPU-bound and have no warm
# cache. Keep soldr's toolchain/syslib prep, but avoid routing this one
# release build through the embedded zccache wrapper path that can hang
# the hosted Windows jobs.
*-pc-windows-msvc) soldr_build_args+=(--no-cache) ;;
```

Two consequences worth knowing:

- **The `expected a soldr PDB sidecar` guard gives no coverage for
  soldr#2148.** It only ever inspects the one path that does not have
  the bug, so a regression there would keep it green.
- **soldr ships a symbolizable `soldr.exe` while its users do not.**
  Anyone building their own Windows project through the default cached
  path loses their `.pdb`, which is why this surfaced downstream rather
  than here.

Reproduce either way on any crate with `debug` enabled in the profile:

```console
$ soldr build --release --target x86_64-pc-windows-msvc   # no .pdb
$ soldr --no-cache build --release --target x86_64-pc-windows-msvc   # .pdb
```

Remove this section when soldr#2148 is fixed and the cached path keeps
the sidecar.

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
