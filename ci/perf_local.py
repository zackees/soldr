#!/usr/bin/env python3
"""Run arbitrary `cargo` commands against soldr's warmed Docker volumes.

Mirrors the `ci/perf_local.py` pattern in zackees/zccache (zccache PR
#475). The volumes (`soldr-perf-target`, `soldr-perf-cargo-home`) live
on Linux-native ext4 inside Docker's VFS so cargo's mtime-based
fingerprint check actually succeeds — Windows + Docker Desktop's WSL2
9P bind-mount layer rewrites mtimes on every container start, which
would otherwise force cargo to rebuild the entire workspace each
invocation.

Measured on zccache's 21-crate workspace (#593): 6 min no-op rebuild
with host bind mounts → 1.09 s no-op with named volumes. Same lever
applies here.

## Usage

    uv run python ci/perf_local.py cargo build --release
    uv run python ci/perf_local.py cargo test --workspace
    uv run python ci/perf_local.py cargo clippy --workspace -- -D warnings

Anything after the literal `cargo` token is forwarded verbatim to
cargo inside the container — `argparse` is intentionally NOT used past
the `cargo` boundary so flag handling stays unambiguous.

## Volumes

* `soldr-perf-target` → `/work/target` (cargo build state)
* `soldr-perf-cargo-home` → `/root/.cargo` (cargo registry + downloaded
  crates)
* `soldr-perf-soldr-home` → `/root/.soldr` (kept warm; distinct from
  the test-harness `cook-soldr-home` volume that `bench/cook_in_docker.sh`
  wipes between runs).

Wipe deliberately if the fingerprint state ever gets corrupted:

    docker volume rm soldr-perf-target soldr-perf-cargo-home soldr-perf-soldr-home

## Migration

Switching to this script orphans the old host-side `target/` directory
under the repo root. Reclaim disk with:

    rm -rf target/

The first run after the switch is a full cold build (~5-8 min) into
the fresh volume; subsequent runs are seconds.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

IMAGE = "soldr-cook-dev"
DOCKERFILE = "docker/cook-shared-cache/Dockerfile"
VOLUME_TARGET = "soldr-perf-target"
VOLUME_CARGO_HOME = "soldr-perf-cargo-home"
VOLUME_SOLDR_HOME = "soldr-perf-soldr-home"

USAGE = """\
usage: python ci/perf_local.py cargo <args...>
       python ci/perf_local.py --wipe                # remove the perf volumes
       python ci/perf_local.py --status              # show volume sizes / existence

Every argument after `cargo` is forwarded verbatim to cargo inside the
container. See the module docstring for full notes.
"""


def main(argv: list[str]) -> int:
    repo_root = Path(__file__).resolve().parent.parent
    os.chdir(repo_root)

    if not shutil.which("docker"):
        print("error: docker not on PATH", file=sys.stderr)
        return 2

    if not argv:
        print(USAGE, file=sys.stderr)
        return 2

    # Tiny pre-argparse subcommands. Anything else falls through to the
    # `cargo` forwarder so flags after `cargo` don't get eaten.
    if argv[0] in ("-h", "--help"):
        print(USAGE)
        return 0
    if argv[0] == "--wipe":
        return wipe()
    if argv[0] == "--status":
        return status()
    if argv[0] != "cargo":
        print(
            f"error: expected `cargo` as the first arg, got {argv[0]!r}\n\n{USAGE}",
            file=sys.stderr,
        )
        return 2

    # Build the image (cached; ~5 s when warm).
    build = subprocess.run(
        ["docker", "build", "-f", DOCKERFILE, "-t", IMAGE, "."],
        check=False,
    )
    if build.returncode != 0:
        return build.returncode

    cmd = [
        "docker",
        "run",
        "--rm",
        "--init",
        "-v",
        f"{repo_root}:/work",
        "-v",
        f"{VOLUME_SOLDR_HOME}:/root/.soldr",
        "-v",
        f"{VOLUME_TARGET}:/work/target",
        "-v",
        f"{VOLUME_CARGO_HOME}:/root/.cargo",
        "-w",
        "/work",
        IMAGE,
        *argv,
    ]
    # No `-it` by default. Git Bash / mintty on Windows fools
    # sys.stdin.isatty() into returning True, which makes `docker run
    # -it` error with "the input device is not a TTY" because the
    # underlying console isn't a real ConPTY. Pipes work fine for
    # `cargo build`/`test`/`clippy` — cargo's progress bar gracefully
    # downgrades to line-buffered output when stderr is a pipe. Power
    # users who need a real TTY can set `SOLDR_PERF_LOCAL_TTY=1`.
    if os.environ.get("SOLDR_PERF_LOCAL_TTY", "").strip() in ("1", "true", "yes"):
        cmd.insert(2, "-it")
    completed = subprocess.run(cmd, check=False)
    return completed.returncode


def wipe() -> int:
    out = subprocess.run(
        ["docker", "volume", "rm", "--force", VOLUME_TARGET, VOLUME_CARGO_HOME, VOLUME_SOLDR_HOME],
        check=False,
    )
    return out.returncode


def status() -> int:
    rows: list[tuple[str, str]] = []
    for name in (VOLUME_TARGET, VOLUME_CARGO_HOME, VOLUME_SOLDR_HOME):
        out = subprocess.run(
            ["docker", "volume", "inspect", "--format", "{{.Mountpoint}}", name],
            capture_output=True,
            text=True,
            check=False,
        )
        if out.returncode == 0:
            rows.append((name, out.stdout.strip() or "(no mountpoint)"))
        else:
            rows.append((name, "(absent)"))
    width = max(len(name) for name, _ in rows)
    for name, info in rows:
        print(f"{name:<{width}}  {info}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
