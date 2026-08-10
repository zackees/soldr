"""Reproducible-build seam for #392.

When ``RUNNING_PROCESS_REPRODUCIBLE=1`` is set, the build environment is
normalized so two builds of the same commit produce byte-identical
artifacts:

- ``SOURCE_DATE_EPOCH`` is pinned to the HEAD commit timestamp (maturin
  uses it for wheel zip entry timestamps; other tooling honors it too).
- ``RUSTFLAGS`` gains ``--remap-path-prefix`` entries that replace the
  workspace root, ``CARGO_HOME``, and ``RUSTUP_HOME`` with stable tokens
  so debuginfo and panic paths do not leak host-specific absolute paths.
- ``CARGO_INCREMENTAL=0`` because incremental compilation artifacts are
  not stable across runs.

Default builds are unchanged: every helper is a no-op unless the seam
variable is set.

The module doubles as the verification entrypoint used by CI and the
docs recipe (``docs/reproducible-builds.md``)::

    uv run --module ci.reproducible --verify

which builds the ``runpm`` binary twice (cleaning only the workspace
crate between builds, so cached dependency artifacts are reused) and
compares SHA256 digests.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import platform
import subprocess
import sys
from pathlib import Path

from ci.env import cargo_home, clean_env, repo_root, rustup_home
from ci.soldr import cargo_command

REPRODUCIBLE_ENV_VAR = "RUNNING_PROCESS_REPRODUCIBLE"

# Zip timestamps cannot encode dates before 1980-01-01, so never let
# SOURCE_DATE_EPOCH fall below it (matches the common SOURCE_DATE_EPOCH
# floor used by wheel-building tools).
_EPOCH_FLOOR = 315532800

# Stable tokens the host-specific prefixes are remapped to.
_REMAP_WORKSPACE = "/running-process/src"
_REMAP_CARGO_HOME = "/running-process/cargo"
_REMAP_RUSTUP_HOME = "/running-process/rustup"


def reproducible_requested(env: dict[str, str] | None = None) -> bool:
    value = (env if env is not None else os.environ).get(REPRODUCIBLE_ENV_VAR, "")
    return value.strip().lower() in {"1", "true", "yes", "on"}


def head_commit_epoch(root: Path) -> int:
    """Return the HEAD commit time, clamped to the zip epoch floor."""
    epoch = _EPOCH_FLOOR
    try:
        result = subprocess.run(
            ["git", "log", "-1", "--format=%ct"],
            cwd=root,
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError:
        # No git binary (e.g. minimal containers) — fall back to the floor.
        return epoch
    if result.returncode == 0:
        with contextlib.suppress(ValueError):
            epoch = int(result.stdout.strip())
    return max(epoch, _EPOCH_FLOOR)


def _remap_flags(root: Path, env: dict[str, str]) -> list[str]:
    cargo = Path(env["CARGO_HOME"]).expanduser() if env.get("CARGO_HOME") else cargo_home()
    rustup = Path(env["RUSTUP_HOME"]).expanduser() if env.get("RUSTUP_HOME") else rustup_home()
    pairs: list[tuple[Path, str]] = [
        (root, _REMAP_WORKSPACE),
        (cargo, _REMAP_CARGO_HOME),
        (rustup, _REMAP_RUSTUP_HOME),
    ]
    flags: list[str] = []
    for prefix, token in pairs:
        flags.append(f"--remap-path-prefix={prefix}={token}")
    return flags


def apply_reproducible_env(env: dict[str, str], root: Path | None = None) -> dict[str, str]:
    """Normalize ``env`` for deterministic output when the seam is active.

    Returns ``env`` unchanged (same dict) when ``RUNNING_PROCESS_REPRODUCIBLE``
    is not set, so default builds are unaffected.
    """
    if not reproducible_requested(env):
        return env
    root = root if root is not None else repo_root()
    env.setdefault("SOURCE_DATE_EPOCH", str(head_commit_epoch(root)))
    env["CARGO_INCREMENTAL"] = "0"
    rustflags = env.get("RUSTFLAGS", "").split()
    extra = _remap_flags(root, env)
    if platform.system() == "Windows":
        # MSVC link.exe embeds a wall-clock timestamp and a timestamp-seeded
        # signature in the PE debug directory; /Brepro switches both to
        # content-derived hashes.
        extra.append("-Clink-arg=/Brepro")
    for flag in extra:
        if flag not in rustflags:
            rustflags.append(flag)
    env["RUSTFLAGS"] = " ".join(rustflags)
    return env


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _build_runpm(env: dict[str, str], root: Path) -> Path:
    """Build the debug-profile runpm binary and return its path."""
    result = subprocess.run(
        cargo_command(
            "build",
            "-p",
            "running-process",
            "--bin",
            "runpm",
            "--message-format=json",
        ),
        cwd=root,
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )
    if result.returncode != 0:
        print(result.stderr, file=sys.stderr, flush=True)
        raise RuntimeError(f"cargo build failed with exit code {result.returncode}")
    for line in result.stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        with contextlib.suppress(json.JSONDecodeError):
            msg = json.loads(line)
            if (
                msg.get("reason") == "compiler-artifact"
                and msg.get("target", {}).get("name") == "runpm"
                and msg.get("executable")
            ):
                return Path(msg["executable"])
    raise RuntimeError("runpm executable not found in cargo output")


def _clean_workspace_crate(env: dict[str, str], root: Path) -> None:
    result = subprocess.run(
        cargo_command("clean", "-p", "running-process"),
        cwd=root,
        check=False,
        env=env,
    )
    if result.returncode != 0:
        raise RuntimeError(f"cargo clean failed with exit code {result.returncode}")


def verify(root: Path | None = None) -> int:
    """Build runpm twice under the seam and compare SHA256 digests."""
    root = root if root is not None else repo_root()
    env = clean_env()
    env[REPRODUCIBLE_ENV_VAR] = "1"
    env = apply_reproducible_env(env, root)
    print(
        f"reproducible verify: SOURCE_DATE_EPOCH={env['SOURCE_DATE_EPOCH']} "
        f"RUSTFLAGS={env['RUSTFLAGS']!r}",
        flush=True,
    )

    hashes: list[str] = []
    for attempt in (1, 2):
        _clean_workspace_crate(env, root)
        binary = _build_runpm(env, root)
        digest = _sha256(binary)
        hashes.append(digest)
        print(f"build {attempt}: {binary} sha256={digest}", flush=True)

    if hashes[0] == hashes[1]:
        print("REPRODUCIBLE: runpm builds are byte-identical", flush=True)
        return 0
    print("NOT REPRODUCIBLE: runpm builds differ", file=sys.stderr, flush=True)
    return 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Reproducible build seam (#392)")
    parser.add_argument(
        "--verify",
        action="store_true",
        help="build runpm twice under RUNNING_PROCESS_REPRODUCIBLE=1 and compare SHA256",
    )
    args = parser.parse_args(argv)
    if args.verify:
        return verify()
    parser.print_help()
    return 2


if __name__ == "__main__":
    sys.exit(main())
