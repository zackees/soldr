#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
"""Iterative Docker Linux dev/test runner for running-process.

Wraps `docker run` against a warmed image (docker/dev/Dockerfile) with named
volumes for target/, CARGO_HOME, RUSTUP_HOME, the uv cache, and soldr state
so cargo's mtime fingerprint survives container restarts. Source is bind-
mounted read-write at /work; everything compile-state-shaped lives in
named volumes.

Why: broker v1 development (#464, #466) runs inside Linux so futex, shm_open,
eventfd, and signal semantics are exercised directly — and so the Windows
host's toolchain/cargo state is never touched. On Windows + WSL2 the named-
volume strategy turns the no-op rebuild from ~minutes (host bind mount)
into ~seconds.

Quick start:

    python ci/dev_docker.py build-image          # build the dev image
    python ci/dev_docker.py cargo build --release
    python ci/dev_docker.py test                 # ./test inside container
    python ci/dev_docker.py lint                 # ./lint inside container
    python ci/dev_docker.py wheel                # uv run build.py
    python ci/dev_docker.py shell                # interactive bash
    python ci/dev_docker.py -- env               # arbitrary command

Volume management:

    python ci/dev_docker.py --status             # show mountpoints
    python ci/dev_docker.py --wipe               # remove all dev volumes

Coexists with the CI-artifact Dockerfiles at repo root (Dockerfile.linux-*);
those build release wheels and lint/test containers for CI and are not
touched by this driver.
"""

from __future__ import annotations

import argparse
import os
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

IMAGE = "running-process-dev:latest"
DOCKERFILE = "docker/dev/Dockerfile"

VOL_TARGET = "running-process-dev-target"
VOL_CARGO = "running-process-dev-cargo"
VOL_RUSTUP = "running-process-dev-rustup"
VOL_UV = "running-process-dev-uv"

ALL_VOLUMES = (VOL_TARGET, VOL_CARGO, VOL_RUSTUP, VOL_UV)


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def ensure_docker() -> None:
    if shutil.which("docker") is None:
        print("error: docker not on PATH", file=sys.stderr)
        sys.exit(2)


def build_image(no_cache: bool = False) -> int:
    """Build the dev image. Cheap when the layer cache is warm."""
    root = repo_root()
    cmd = ["docker", "build", "-f", DOCKERFILE, "-t", IMAGE]
    if no_cache:
        cmd.append("--no-cache")
    cmd.append(str(root))
    print(f"$ {' '.join(cmd)}", flush=True)
    return subprocess.run(cmd, cwd=str(root)).returncode


def ensure_image_built() -> int:
    """Build the image if it doesn't exist yet. No-op when cached."""
    inspect = subprocess.run(
        ["docker", "image", "inspect", IMAGE],
        capture_output=True,
        text=True,
    )
    if inspect.returncode == 0:
        return 0
    return build_image()


def docker_run(argv: list[str], *, interactive: bool = False) -> int:
    """Run a command inside the dev container with all volumes attached."""
    root = repo_root()
    cmd = [
        "docker", "run", "--rm", "--init",
        "-v", f"{root}:/work",
        "-v", f"{VOL_TARGET}:/work/target",
        "-v", f"{VOL_CARGO}:/usr/local/cargo",
        "-v", f"{VOL_RUSTUP}:/usr/local/rustup",
        "-v", f"{VOL_UV}:/uv",
        "-w", "/work",
    ]
    # -it breaks Git-Bash on Windows (mintty fools isatty); opt in via env
    # var or via the explicit `shell` subcommand which always wants a TTY.
    want_tty = interactive or os.environ.get("CLUD_DOCKER_TTY", "").strip() in ("1", "true", "yes")
    if want_tty and sys.stdin.isatty():
        cmd.append("-it")
    cmd.append(IMAGE)
    cmd.extend(argv)

    env = os.environ.copy()
    # MSYS_NO_PATHCONV stops Git-Bash from rewriting /work into a Windows
    # path before docker run sees it.
    env.setdefault("MSYS_NO_PATHCONV", "1")

    print(f"$ {' '.join(cmd)}", flush=True)
    return subprocess.run(cmd, env=env).returncode


def cmd_cargo(args: list[str]) -> int:
    """Forward `cargo <args>` to the container's cargo.

    soldr is intentionally not installed in the dev image; the host's
    force_soldr PreToolUse hook is a host-scope policy and does not apply
    inside the container. The named CARGO_TARGET_DIR volume already gives
    the mtime-fingerprint caching we need across container restarts.
    """
    if ensure_image_built() != 0:
        return 1
    return docker_run(["cargo", *args])


def cmd_test(args: list[str]) -> int:
    """Run the repo's ./test entrypoint inside the container."""
    if ensure_image_built() != 0:
        return 1
    return docker_run(["./test", *args])


def cmd_lint(args: list[str]) -> int:
    """Run the repo's ./lint entrypoint inside the container."""
    if ensure_image_built() != 0:
        return 1
    return docker_run(["./lint", *args])


def cmd_wheel(args: list[str]) -> int:
    """Build the dev wheel inside the container.

    `build.py` has PEP 723 script metadata in its shebang line, which
    makes `uv run build.py` use an ephemeral environment without
    maturin. Use the same `uv sync` + `uv run -- python ...` pattern
    that ./test and ./lint use so the project env (with maturin) is
    populated and reused.
    """
    if ensure_image_built() != 0:
        return 1
    forwarded = " ".join(shlex.quote(a) for a in args)
    cmd = (
        "uv sync --refresh --no-editable"
        " && uv run --no-editable -- python build.py "
        + forwarded
    )
    return docker_run(["bash", "-lc", cmd])


def cmd_shell(args: list[str]) -> int:
    """Drop into an interactive bash shell inside the container."""
    if ensure_image_built() != 0:
        return 1
    return docker_run(["bash", *args], interactive=True)


def cmd_passthrough(args: list[str]) -> int:
    """Run an arbitrary command inside the container (everything after --)."""
    if not args:
        print("error: -- requires a command, e.g. `dev_docker.py -- env`",
              file=sys.stderr)
        return 2
    if ensure_image_built() != 0:
        return 1
    return docker_run(args)


def cmd_wipe() -> int:
    """Remove all dev volumes. Cold rebuild on next run."""
    cmd = ["docker", "volume", "rm", "--force", *ALL_VOLUMES]
    print(f"$ {' '.join(cmd)}", flush=True)
    return subprocess.run(cmd).returncode


def cmd_status() -> int:
    """Show mountpoints for each dev volume (or '(absent)' if missing)."""
    width = max(len(name) for name in ALL_VOLUMES)
    for name in ALL_VOLUMES:
        result = subprocess.run(
            ["docker", "volume", "inspect", "--format", "{{.Mountpoint}}", name],
            capture_output=True,
            text=True,
        )
        location = result.stdout.strip() if result.returncode == 0 else "(absent)"
        print(f"{name:<{width}}  {location}")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="dev_docker.py",
        description="Iterative Docker Linux dev/test runner for running-process.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "Examples:\n"
            "  python ci/dev_docker.py build-image\n"
            "  python ci/dev_docker.py cargo build --release\n"
            "  python ci/dev_docker.py test\n"
            "  python ci/dev_docker.py lint\n"
            "  python ci/dev_docker.py wheel\n"
            "  python ci/dev_docker.py shell\n"
            "  python ci/dev_docker.py -- env\n"
            "  python ci/dev_docker.py --status\n"
            "  python ci/dev_docker.py --wipe\n"
        ),
    )
    parser.add_argument(
        "--wipe",
        action="store_true",
        help="Remove all dev volumes and exit.",
    )
    parser.add_argument(
        "--status",
        action="store_true",
        help="Show volume mountpoints and exit.",
    )

    subparsers = parser.add_subparsers(dest="subcommand")

    p_build = subparsers.add_parser(
        "build-image",
        help="Build the dev image (no-op if cached).",
    )
    p_build.add_argument("--no-cache", action="store_true")

    subparsers.add_parser("cargo", help="Run `soldr cargo ...` in the container.").add_argument(
        "cargo_args", nargs=argparse.REMAINDER, help="Forwarded to cargo."
    )
    subparsers.add_parser("test", help="Run ./test in the container.").add_argument(
        "test_args", nargs=argparse.REMAINDER, help="Forwarded to ./test."
    )
    subparsers.add_parser("lint", help="Run ./lint in the container.").add_argument(
        "lint_args", nargs=argparse.REMAINDER, help="Forwarded to ./lint."
    )
    subparsers.add_parser("wheel", help="Run uv run build.py in the container.").add_argument(
        "wheel_args", nargs=argparse.REMAINDER, help="Forwarded to build.py."
    )
    subparsers.add_parser("shell", help="Interactive bash in the container.").add_argument(
        "shell_args", nargs=argparse.REMAINDER, help="Forwarded to bash."
    )

    return parser


def main(argv: list[str]) -> int:
    ensure_docker()

    # argparse + REMAINDER doesn't handle a leading `--` well, so peel it off
    # ourselves for the passthrough case.
    if argv and argv[0] == "--":
        return cmd_passthrough(argv[1:])

    parser = build_parser()
    args = parser.parse_args(argv)

    if args.wipe:
        return cmd_wipe()
    if args.status:
        return cmd_status()

    if args.subcommand == "build-image":
        return build_image(no_cache=args.no_cache)
    if args.subcommand == "cargo":
        return cmd_cargo(args.cargo_args or [])
    if args.subcommand == "test":
        return cmd_test(args.test_args or [])
    if args.subcommand == "lint":
        return cmd_lint(args.lint_args or [])
    if args.subcommand == "wheel":
        return cmd_wheel(args.wheel_args or [])
    if args.subcommand == "shell":
        return cmd_shell(args.shell_args or [])

    parser.print_help()
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
