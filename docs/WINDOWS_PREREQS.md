# Windows prerequisites for soldr's docker-prebuilt + cross-compile recipes

This page lists the Windows host tooling soldr's reference workflows
assume but does NOT bootstrap. Anyone reproducing the docker-prebuilt
recipe (`bench/cook_in_docker.sh`, `examples/docker-cross-win/build.sh`,
the `dispatch_forge_syslibs.sh` helper in `zackees/soldr-toolchain`,
etc.) on a stock Windows install will hit one of these gaps and see a
confusing low-level error like `cygpath: command not found`. Install
the listed prerequisite first, then re-run the recipe.

Tracking: [soldr#885](https://github.com/zackees/soldr/issues/885).

## The shell layer

The bash-flavored recipes use POSIX paths (`/c/Users/me/...`) and two
Git-Bash–specific knobs that have no PowerShell or stock-cmd
equivalent:

| Tool / env var | What it does | Where it ships |
|---|---|---|
| `bash` | runs the recipe script itself | Git for Windows (Git Bash), MSYS2, Cygwin, or WSL2 |
| `cygpath` | converts `/c/foo` → `C:\foo` so the resolved path can be passed to a Windows-native `docker.exe` | Git Bash + MSYS2 (built in); Cygwin (built in); NOT in stock Windows |
| `MSYS_NO_PATHCONV=1` | disables Git Bash's automatic POSIX→Windows path mangling when calling Windows-native binaries (notably `docker.exe`'s `-v <host>:/work` bind-mount arg) | recognized only by Git Bash's bash; harmless on Linux/macOS bash (treated as a regular env var) |

If `cygpath` is missing, the recipe will fail at the very first
`cygpath -w …` line with `cygpath: command not found`. The cause is
not soldr — it's the missing shell prerequisite.

## Recommended setup

Pick exactly one of these on a fresh Windows host. **Git for Windows
is the easiest** because most developers already have it for `git`.

### Option A — Git for Windows (recommended)

```powershell
winget install --id Git.Git -e --source winget
# or: scoop install git
# or: choco install git
```

Reopen your terminal. `bash`, `cygpath`, and the `MSYS_NO_PATHCONV` env
var all become available. soldr's docker-prebuilt + cross-compile
recipes work as documented.

### Option B — WSL2

```powershell
wsl --install
```

Run the recipes inside the Linux distro. soldr installs cleanly via
`pip install soldr` inside WSL; the recipes are written for Linux
anyway. This is the most reproducible path if you need to match what
CI runs.

### Option C — MSYS2 / Cygwin

Install MSYS2 (`winget install MSYS2.MSYS2`) or Cygwin and put their
`bin` directory ahead of any other shell on PATH. Both ship
`cygpath` + a bash that honors `MSYS_NO_PATHCONV`. Use this if you
already have one of these toolchains for other workflows.

## What soldr DOES bootstrap on Windows

soldr is the strict half of the soldr/setup-soldr pair and bootstraps
**only the Rust toolchain story** — rustup, cargo, the per-project
channel from `rust-toolchain.toml`, the embedded zccache service, the managed
crgx binary, and the catalogue-served C library sysroots. It does NOT
install:

- A POSIX-compatible shell (`bash`, `sh`, `dash`).
- POSIX-path helpers (`cygpath`, `wslpath`).
- Docker Desktop, podman, or any container runtime.
- Git itself.
- The MSVC build tools / Windows SDK (`cl.exe`, `link.exe`).

The first three are docker-prebuilt-recipe prerequisites — install via
Option A/B/C above. Git is needed for soldr's own `cargo install`
fetches; install via winget/scoop/choco. The MSVC build tools are NOT
required when the recipe uses cargo-xwin or cargo-zigbuild (they
materialize their own cross-link toolchains); they ARE required for
native `soldr cargo build` on `*-pc-windows-msvc` — see
[`docs/CROSS_COMPILE.md`](CROSS_COMPILE.md) for the per-recipe matrix.

## Error → fix table

| Error you saw | Probable cause | Fix |
|---|---|---|
| `cygpath: command not found` | running a bash recipe on a Windows host without Git Bash / MSYS2 / Cygwin / WSL | Install via Option A/B/C above |
| `docker: Error response from daemon: the working directory '<Windows-path>' is invalid, it needs to be an absolute path` when bind-mounting | Git Bash mangled your POSIX path; the recipe forgot `MSYS_NO_PATHCONV=1` | Set `MSYS_NO_PATHCONV=1` before the `docker run` invocation (already in soldr's documented recipes; flag it upstream if you found a recipe without it) |
| `bash: command not found` (PowerShell) | running a `.sh` recipe with no bash on PATH | Install via Option A/B/C above, or port the recipe to PowerShell (see soldr#885 option 2) |
| `MSYS_NO_PATHCONV=1` set but path still mangled | the recipe uses an old Git Bash that ignores the env var; works on 2.30+ | Update Git for Windows: `winget upgrade Git.Git` |

## Cross-references

- [`docs/CROSS_COMPILE.md`](CROSS_COMPILE.md) — the cross-compile recipes that depend on this setup.
- [`bench/cook_in_docker.sh`](../bench/cook_in_docker.sh) — uses `cygpath` + `MSYS_NO_PATHCONV`.
- [`examples/docker-cross-win/build.sh`](../examples/docker-cross-win/build.sh) — uses `MSYS_NO_PATHCONV`.
- [`scripts/test_msvc_host_linux.sh`](../scripts/test_msvc_host_linux.sh) — uses `MSYS_NO_PATHCONV`.
