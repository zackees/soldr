"""Run a packaged target command under the target execution architecture."""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path

EXECUTION_MODES = frozenset({"native", "x86_64-rosetta", "x86_64-dockur"})

# soldr#3071: forwarded into the dockur guest so the packaged target command
# sees the same cache/toolchain/replay configuration the host job set up for
# it. Prefix match, not an exact-name allowlist, because callers add new
# SOLDR_*/NEXTEST_* knobs routinely and a forwarding allowlist would silently
# stop covering them.
FORWARDED_ENV_PREFIXES = (
    "SOLDR_",
    "NEXTEST_",
    "CARGO",
    "RUSTC",
    "RUSTUP_",
    "REPLAY_",
    "TMPDIR",
)

DOCKUR_PATH_MAP_ENV = "SOLDR_DOCKUR_PATH_MAP"
GUEST_SCRIPT = Path(__file__).resolve().parents[2] / "ci" / "macos_x64_guest.py"


def command_argv(execution: str, command: Sequence[str]) -> list[str]:
    """Return the exact process argv for the declared execution contract.

    Only the two execution modes that run in-place on the same host (native,
    x86_64-rosetta) fit this shape: they are a static argv rewrite of the
    packaged command. x86_64-dockur instead ships the command to a remote
    guest, which needs the caller's environment and working directory too --
    see `dockur_exec_argv` and the dispatch in `main()`.
    """
    if execution not in EXECUTION_MODES:
        raise ValueError(f"unsupported target execution mode: {execution!r}")
    if not command:
        raise ValueError("target command is required")
    if execution == "x86_64-rosetta":
        return ["arch", "-x86_64", *command]
    if execution == "x86_64-dockur":
        raise ValueError(
            "x86_64-dockur has no static argv rewrite; use dockur_exec_argv"
        )
    return list(command)


def strip_remainder_delimiter(command: Sequence[str]) -> list[str]:
    """Remove argparse's one separator without corrupting command arguments."""
    values = list(command)
    return values[1:] if values[:1] == ["--"] else values


def parse_path_map(raw: str) -> list[tuple[str, str]]:
    """Parse `SOLDR_DOCKUR_PATH_MAP` (`host=guest;host2=guest2`) into pairs.

    Sorted longest-host-prefix-first so a nested mapping (e.g. a temp dir
    inside the workspace) resolves before its broader parent does.
    """
    pairs: list[tuple[str, str]] = []
    for entry in raw.split(";"):
        entry = entry.strip()
        if not entry:
            continue
        host, sep, guest_path = entry.partition("=")
        if not sep:
            raise ValueError(f"malformed path map entry: {entry!r}")
        pairs.append((host, guest_path))
    return sorted(pairs, key=lambda pair: len(pair[0]), reverse=True)


def map_path(value: str, mappings: Sequence[tuple[str, str]]) -> str:
    """Rewrite one string's host path prefix to its guest equivalent, if any."""
    for host, guest_path in mappings:
        host_stripped = host.rstrip("/")
        if value == host_stripped:
            return guest_path
        if value.startswith(host_stripped + "/"):
            return guest_path.rstrip("/") + value[len(host_stripped) :]
    return value


def map_host_paths(
    argv: Sequence[str], mappings: Sequence[tuple[str, str]]
) -> list[str]:
    """Apply `map_path` to every argv element, including `--flag=/path` forms."""
    mapped: list[str] = []
    for arg in argv:
        if arg.startswith("--") and "=" in arg:
            flag, _, value = arg.partition("=")
            mapped.append(f"{flag}={map_path(value, mappings)}")
        else:
            mapped.append(map_path(arg, mappings))
    return mapped


def forwarded_env(
    environ: Mapping[str, str], mappings: Sequence[tuple[str, str]]
) -> dict[str, str]:
    """Env vars to forward into the guest, with their values path-mapped."""
    return {
        key: map_path(value, mappings)
        for key, value in environ.items()
        if key.startswith(FORWARDED_ENV_PREFIXES)
    }


def dockur_exec_argv(
    command: Sequence[str],
    *,
    cwd: str,
    env: Mapping[str, str],
    guest_script: Path = GUEST_SCRIPT,
    python_exe: str = sys.executable,
) -> list[str]:
    """Build the `ci/macos_x64_guest.py exec` invocation for a mapped command."""
    argv = [python_exe, str(guest_script), "exec", "--cwd", cwd]
    for key, value in sorted(env.items()):
        argv += ["--env", f"{key}={value}"]
    argv.append("--")
    argv += list(command)
    return argv


def dockur_preflight_argv(
    guest_script: Path = GUEST_SCRIPT, python_exe: str = sys.executable
) -> list[str]:
    return [python_exe, str(guest_script), "exec", "--", "/usr/bin/true"]


def _run_dockur(
    command: Sequence[str], *, preflight: bool, environ: Mapping[str, str]
) -> list[str]:
    if preflight:
        return dockur_preflight_argv()
    mappings = parse_path_map(environ.get(DOCKUR_PATH_MAP_ENV, ""))
    if not mappings:
        raise ValueError(
            f"{DOCKUR_PATH_MAP_ENV} is required (and must be non-empty) in "
            "x86_64-dockur mode"
        )
    workspace = environ.get("GITHUB_WORKSPACE")
    if not workspace:
        raise ValueError("GITHUB_WORKSPACE is required in x86_64-dockur mode")
    mapped_command = map_host_paths(command, mappings)
    cwd = map_path(workspace, mappings)
    env = forwarded_env(environ, mappings)
    return dockur_exec_argv(mapped_command, cwd=cwd, env=env)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execution", choices=sorted(EXECUTION_MODES), required=True)
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = strip_remainder_delimiter(args.command)

    if args.execution == "x86_64-dockur":
        if not args.preflight and not command:
            parser.error("target command is required")
        try:
            argv = _run_dockur(command, preflight=args.preflight, environ=os.environ)
        except ValueError as error:
            parser.error(str(error))
        subprocess.run(argv, check=True)
        return 0

    if args.preflight:
        command = ["/usr/bin/true"]
    try:
        argv = command_argv(args.execution, command)
    except ValueError as error:
        parser.error(str(error))
    subprocess.run(argv, check=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
