# Vendored ConPTY sidecars

Per-arch `OpenConsole.exe` + `conpty.dll` binaries from Microsoft's
`Microsoft.Windows.Console.ConPTY` NuGet package, committed into the
repo via **Git LFS** so release-time builds are **hermetic** and
**byte-for-byte reproducible** from any git SHA — independent of what
NuGet happens to be serving on release day.

## Provenance

Extracted from `Microsoft.Windows.Console.ConPTY` NuGet, version pinned
in [`/WINDOWS_CONPTY_VERSION.txt`](../../../WINDOWS_CONPTY_VERSION.txt)
at the time of each refresh commit.

| arch    | conpty.dll path in nupkg                | OpenConsole.exe path in nupkg                |
|---------|------------------------------------------|-----------------------------------------------|
| x64     | `runtimes/win-x64/native/conpty.dll`     | `build/native/runtimes/x64/OpenConsole.exe`   |
| x86     | `runtimes/win-x86/native/conpty.dll`     | `build/native/runtimes/x86/OpenConsole.exe`   |
| arm64   | `runtimes/win-arm64/native/conpty.dll`   | `build/native/runtimes/arm64/OpenConsole.exe` |

The 32-bit ARM (`win-arm`) variant was dropped from the NuGet package
as of 1.24.260512001 and is not vendored.

## Why vendor

1. **Hermetic builds.** `.github/workflows/auto-release.yml` reads
   these bytes at release time instead of fetching from NuGet. The
   release workflow has no network dependency on NuGet at all.
2. **Reproducible builds.** Any contributor can re-run the release
   workflow against a given git SHA and produce byte-identical
   `conpty-sidecar-<arch>.tar.zst` outputs. The vendored bytes are
   part of the input.
3. **Operator-controlled binary swaps.** Microsoft has changed the
   NuGet package layout (issue
   [#500](https://github.com/zackees/running-process/issues/500))
   between 1.24.260402001 (`runtimes/win-<arch>/native/OpenConsole.exe`)
   and 1.24.260512001 (`build/native/runtimes/<arch>/OpenConsole.exe`)
   *without changing the version major-minor*. Vendoring puts each
   binary change behind a reviewed commit instead of letting NuGet
   silently roll the bytes under us.

## Why Git LFS

The three per-arch directories total ~3.4 MB of binary content.
Committing them as plain blobs would inflate every clone of every
checkout and stick the bytes in pack files forever. Git LFS keeps the
repo small and lets `git clone --filter=blob:none` skip the binaries
entirely until needed. CI runners pull them automatically because the
release workflow checks them out via `actions/checkout` with default
LFS support enabled.

## Refreshing

`.github/workflows/dependency-check.yml` runs weekly. When NuGet's
current bytes for the pinned version no longer match the vendored
files, it opens a sub-issue with the SHA-256 deltas. **The drift-check
never auto-commits** — refreshing is always an operator-reviewed PR:

1. `curl -fsSL "https://www.nuget.org/api/v2/package/Microsoft.Windows.Console.ConPTY/<NEW_VERSION>" -o /tmp/conpty.nupkg`
2. `unzip /tmp/conpty.nupkg -d /tmp/conpty-extracted`
3. Copy each per-arch `conpty.dll` + `OpenConsole.exe` over the
   matching `vendor/win32/conpty/<arch>/` file (per the table above).
4. Update `WINDOWS_CONPTY_VERSION.txt` to the new version.
5. Diff the binaries (`git diff vendor/win32/conpty/`); compare
   SHA-256s against any release-notes Microsoft published.
6. Open a PR; `dependency-check.yml` re-verifies each binary on a
   Windows runner before merge.

## License

The Microsoft.Windows.Console.ConPTY NuGet package is distributed
under the [MIT license](https://github.com/microsoft/terminal/blob/main/LICENSE).
Vendoring conforms to that license; the per-arch directories carry
only the binary artifacts plus this README.
