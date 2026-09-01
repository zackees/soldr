#!/usr/bin/env python3
"""Install the two-name Cargo boundary used by prescribed host validation.

``CARGO`` points at an explicitly allowed shim which re-enters the source-built
Soldr binary as ``soldr cargo``.  A separately named ``cargo`` shim is placed
first on ``PATH`` and fails closed, exposing code which bypasses ``CARGO`` and
spawns Cargo by its bare name.  ``SOLDR_REAL_CARGO`` remains the escape from
the allowed shim to the absolute rustup Cargo binary.

The caller owns environment scoping.  This helper materializes the shims and
plain-text path records for the platform-specific allowed Cargo and test
runner names; it deliberately does not write ``GITHUB_ENV`` or ``GITHUB_PATH``.
"""

from __future__ import annotations

import argparse
import os
import shlex
from dataclasses import dataclass
from pathlib import Path

TRAP_EXIT_CODE = 86
TRAP_MESSAGE = (
    "soldr ci-test: unexpected bare cargo invocation; "
    "intentional nested Cargo must use $CARGO"
)


@dataclass(frozen=True)
class GuardPaths:
    allowed_cargo: Path
    trap_dir: Path
    test_runner: Path


def _require_absolute_file(path: Path, label: str) -> Path:
    if not path.is_absolute():
        raise ValueError(f"{label} must be an absolute path: {path}")
    if not path.is_file():
        raise ValueError(f"{label} is not a file: {path}")
    return path


def render_posix_allowed(source_soldr: Path) -> str:
    return "#!/bin/sh\n" f'exec {shlex.quote(str(source_soldr))} cargo "$@"\n'


def render_posix_trap() -> str:
    return (
        "#!/bin/sh\n"
        f"printf '%s\\n' {shlex.quote(TRAP_MESSAGE)} >&2\n"
        f"exit {TRAP_EXIT_CODE}\n"
    )


def render_posix_test_runner(allowed_cargo: Path) -> str:
    return (
        "#!/bin/sh\n" f"export CARGO={shlex.quote(str(allowed_cargo))}\n" 'exec "$@"\n'
    )


def _quote_cmd_path(path: Path) -> str:
    raw = str(path)
    if any(character in raw for character in ('"', "%", "\r", "\n")):
        raise ValueError(f"path cannot be represented safely in a cmd shim: {path}")
    return f'"{raw}"'


def render_windows_allowed(source_soldr: Path) -> str:
    return (
        "@echo off\r\n"
        f"{_quote_cmd_path(source_soldr)} cargo %*\r\n"
        "exit /b %ERRORLEVEL%\r\n"
    )


def render_windows_trap() -> str:
    return (
        "@echo off\r\n" f">&2 echo {TRAP_MESSAGE}\r\n" f"exit /b {TRAP_EXIT_CODE}\r\n"
    )


def render_windows_test_runner(allowed_cargo: Path) -> str:
    raw = str(allowed_cargo)
    if any(character in raw for character in ('"', "%", "\r", "\n")):
        raise ValueError(
            f"path cannot be represented safely in a cmd shim: {allowed_cargo}"
        )
    return (
        "@echo off\r\n"
        f'set "CARGO={raw}"\r\n'
        "call %*\r\n"
        "exit /b %ERRORLEVEL%\r\n"
    )


def install_guard(
    *,
    source_soldr: Path,
    real_cargo: Path,
    output_dir: Path,
    platform: str | None = None,
) -> GuardPaths:
    """Materialize an allowed Cargo shim and a fail-closed bare-name trap."""

    source_soldr = _require_absolute_file(source_soldr, "source Soldr")
    _require_absolute_file(real_cargo, "real Cargo")
    if not output_dir.is_absolute():
        raise ValueError(f"output directory must be an absolute path: {output_dir}")

    selected = platform or ("windows" if os.name == "nt" else "posix")
    if selected not in {"posix", "windows"}:
        raise ValueError(f"unsupported shim platform: {selected}")

    allowed_dir = output_dir / "allowed"
    trap_dir = output_dir / "trap"
    allowed_dir.mkdir(parents=True, exist_ok=True)
    trap_dir.mkdir(parents=True, exist_ok=True)

    if selected == "windows":
        allowed_cargo = allowed_dir / "soldr-ci-cargo.cmd"
        trap_cargo = trap_dir / "cargo.cmd"
        test_runner = output_dir / "soldr-ci-test-runner.cmd"
        allowed_cargo.write_text(
            render_windows_allowed(source_soldr), encoding="utf-8", newline=""
        )
        trap_cargo.write_text(render_windows_trap(), encoding="utf-8", newline="")
        test_runner.write_text(
            render_windows_test_runner(allowed_cargo), encoding="utf-8", newline=""
        )
    else:
        allowed_cargo = allowed_dir / "soldr-ci-cargo"
        trap_cargo = trap_dir / "cargo"
        test_runner = output_dir / "soldr-ci-test-runner"
        allowed_cargo.write_text(render_posix_allowed(source_soldr), encoding="utf-8")
        trap_cargo.write_text(render_posix_trap(), encoding="utf-8")
        test_runner.write_text(
            render_posix_test_runner(allowed_cargo), encoding="utf-8"
        )
        allowed_cargo.chmod(0o755)
        trap_cargo.chmod(0o755)
        test_runner.chmod(0o755)

    (output_dir / "allowed-cargo-path").write_text(
        f"{allowed_cargo}\n", encoding="utf-8"
    )
    (output_dir / "test-runner-path").write_text(f"{test_runner}\n", encoding="utf-8")
    return GuardPaths(
        allowed_cargo=allowed_cargo, trap_dir=trap_dir, test_runner=test_runner
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-soldr", required=True, type=Path)
    parser.add_argument("--real-cargo", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--platform", choices=("posix", "windows"))
    args = parser.parse_args()

    try:
        paths = install_guard(
            source_soldr=args.source_soldr,
            real_cargo=args.real_cargo,
            output_dir=args.output_dir,
            platform=args.platform,
        )
    except ValueError as error:
        parser.error(str(error))

    print(f"allowed Cargo shim: {paths.allowed_cargo}")
    print(f"bare Cargo trap directory: {paths.trap_dir}")
    print(f"Nextest Cargo-restoring runner: {paths.test_runner}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
