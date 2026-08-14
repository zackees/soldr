#!/usr/bin/env python3
"""Build an installable win-x64 soldr wheel in Docker, with warm caches.

Why this exists: a host whose soldr is wedged cannot build a replacement with
itself. This harness builds one on Linux, cross-compiled for Windows x64, using
a *released* soldr from PyPI as the driver, and drops the wheel where it can be
installed with ``uv pip install``.

Speed comes from two layers:

* **Bake time** -- the Rust toolchain, the Windows std, the blessed MSVC
  SDK/sysroot, and maturin live in image layers, so a cold ``docker run`` does
  not download them.
* **Run time** -- Cargo registry/git, ``target/``, the Rust toolchain root, and
  the soldr home (which holds the compiler cache) are named volumes that
  survive between runs, so a warm rebuild compiles only what changed.

Volumes are named per checkout root, matching ``ci/perf_local.py``, so sibling
checkouts do not fight over one warm ``target/``.

Usage::

    uv run --no-project python ci/win_wheel_local.py
    uv run --no-project python ci/win_wheel_local.py --release
    uv run --no-project python ci/win_wheel_local.py --rebuild-image
    uv run --no-project python ci/win_wheel_local.py --status
    uv run --no-project python ci/win_wheel_local.py --wipe

``--wipe`` is the only destructive operation; it removes the warm volumes.
"""

from __future__ import annotations

import argparse
import hashlib
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
DOCKER_CONTEXT = REPO_ROOT / "ci" / "docker-win-wheel"
IMAGE = "soldr-win-wheel"
VOLUME_PREFIX = "soldr-win-wheel"
DEFAULT_TARGET = "x86_64-pc-windows-msvc"
# Pinned: 0.9.0 is on PyPI but its broker cannot dial its daemon route, so it
# cannot drive a build at all. See ci/docker-win-wheel/Dockerfile.
DEFAULT_SOLDR_VERSION = "0.8.44"


def checkout_suffix() -> str:
    """Short stable id for this checkout, so sibling clones get own volumes."""
    digest = hashlib.sha256(str(REPO_ROOT).encode("utf-8")).hexdigest()
    return digest[:10]


def volume_names() -> dict[str, str]:
    suffix = checkout_suffix()
    # Only two volumes are needed, and neither is `~/.cargo` / `~/.rustup`.
    # soldr keeps its managed CARGO_HOME and RUSTUP_HOME *inside* its own home
    # (`/root/.soldr/cargo`, `/root/.soldr/rustup`) alongside the SDK and the
    # compiler cache, so one volume there carries every warm artifact. Mounting
    # `/root/.cargo` instead -- which an earlier version of this script did --
    # caches nothing and shadows nothing, because bare cargo is not on PATH in
    # this image at all.
    return {
        "soldr": f"{VOLUME_PREFIX}-soldr-{suffix}",
        "target": f"{VOLUME_PREFIX}-target-{suffix}",
    }


def run(command: list[str], *, quiet: bool = False) -> subprocess.CompletedProcess[bytes]:
    """Echo a command and run it, never raising on a non-zero exit.

    `quiet` discards stdout for the noisy bookkeeping calls (`volume create`
    echoes the name it just created); failures still surface via the return
    code, which every caller checks.
    """
    print(f"+ {' '.join(command)}", flush=True)
    return subprocess.run(
        command,
        check=False,
        stdout=subprocess.DEVNULL if quiet else None,
    )


def docker_available() -> bool:
    try:
        return run(["docker", "version", "--format", "{{.Server.Version}}"]).returncode == 0
    except FileNotFoundError:
        return False


def ensure_volumes() -> None:
    for name in volume_names().values():
        run(["docker", "volume", "create", name], quiet=True)


def build_image(*, soldr_version: str, target: str, rebuild: bool) -> int:
    command = [
        "docker",
        "build",
        "--build-arg",
        f"SOLDR_VERSION={soldr_version}",
        "--build-arg",
        f"WHEEL_TARGET={target}",
        "-t",
        IMAGE,
        str(DOCKER_CONTEXT),
    ]
    if rebuild:
        command.insert(2, "--no-cache")
    return run(command).returncode


def build_wheel(*, target: str, release: bool, out_dir: Path, extra: list[str]) -> int:
    volumes = volume_names()
    out_dir.mkdir(parents=True, exist_ok=True)
    command = [
        "docker",
        "run",
        "--rm",
        # The checkout is bind-mounted rather than COPYied so an edit is
        # visible immediately and no layer is invalidated by a source change.
        "-v",
        f"{REPO_ROOT}:/work",
        "-v",
        f"{out_dir}:/out",
        "-v",
        f"{volumes['target']}:/work/target",
        # Seeded from the image on first mount, so the baked toolchain, SDK and
        # sysroot survive into the volume and stay warm from then on.
        "-v",
        f"{volumes['soldr']}:/root/.soldr",
        "-w",
        "/work",
        IMAGE,
        "--target",
        target,
    ]
    if release:
        command.append("--release")
    if extra:
        command.append("--")
        command.extend(extra)
    return run(command).returncode


def status() -> int:
    run(["docker", "images", IMAGE])
    for name in volume_names().values():
        run(["docker", "volume", "inspect", "--format", "{{.Name}} {{.Mountpoint}}", name])
    return 0


def wipe() -> int:
    names = list(volume_names().values())
    return run(["docker", "volume", "rm", "--force", *names]).returncode


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default=DEFAULT_TARGET)
    parser.add_argument("--soldr-version", default=DEFAULT_SOLDR_VERSION)
    parser.add_argument(
        "--release",
        action="store_true",
        help="build the release profile; the default dev profile is much faster "
        "and is what this harness is for",
    )
    parser.add_argument(
        "--out",
        type=Path,
        default=REPO_ROOT / "dist" / "win-wheel",
        help="host directory to receive the wheel",
    )
    parser.add_argument("--rebuild-image", action="store_true", help="bake the image from scratch")
    parser.add_argument("--skip-image", action="store_true", help="reuse the existing image as-is")
    parser.add_argument("--status", action="store_true")
    parser.add_argument("--wipe", action="store_true")
    parser.add_argument("rest", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)

    if not docker_available():
        print(
            "win_wheel_local: docker is not available. Start Docker Desktop (or the "
            "daemon) and retry.",
            file=sys.stderr,
        )
        return 2

    if args.status:
        return status()
    if args.wipe:
        return wipe()

    if not args.skip_image:
        code = build_image(
            soldr_version=args.soldr_version,
            target=args.target,
            rebuild=args.rebuild_image,
        )
        if code != 0:
            return code

    ensure_volumes()
    extra = args.rest[1:] if args.rest and args.rest[0] == "--" else args.rest
    code = build_wheel(
        target=args.target,
        release=args.release,
        out_dir=args.out.resolve(),
        extra=extra,
    )
    if code == 0:
        print()
        print(f"win_wheel_local: wheel(s) in {args.out.resolve()}")
        print("install with:")
        print(f"  uv pip install --python <interpreter> {args.out.resolve()}\\<wheel>.whl")
    return code


if __name__ == "__main__":
    sys.exit(main())
