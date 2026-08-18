#!/usr/bin/env python3
"""Stage Soldr's executable and debug sidecars for a release archive.

This is the release workflow's narrow packaging boundary: it copies the built
``soldr`` executable to ``dist/package``, creates the ``soldr-daemon`` sidecar,
and stages platform-specific debug information.  The manifest and archive
steps consume exactly this directory, so a missing Windows PDB must fail here
while optional Linux split-DWARF and macOS dSYM sidecars are preserved when
present.

It was extracted from release-auto.yml as part of soldr#2469 step 2.2 so the
platform-specific staging policy can be unit-tested without running a release.

Usage (CI):
    python3 .github/scripts/stage_release_binaries.py \
        --target x86_64-unknown-linux-gnu --package-dir dist/package
"""

from __future__ import annotations

import argparse
import shutil
import sys
from pathlib import Path

from release_artifacts import binary_suffix


class StagingError(RuntimeError):
    """A required release artifact was absent or could not be staged."""


def release_contents(directory: Path) -> str:
    if not directory.is_dir():
        return f"{directory} does not exist"
    entries = sorted(path.name for path in directory.iterdir())
    return "\n".join(f"  {entry}" for entry in entries) or "  (empty)"


def first_file(directory: Path, names: list[str]) -> Path | None:
    return next((directory / name for name in names if (directory / name).is_file()), None)


def first_directory(directory: Path, names: list[str]) -> Path | None:
    return next((directory / name for name in names if (directory / name).is_dir()), None)


def copy_or_link(source: Path, destination: Path) -> None:
    """Prefer a hardlink for the daemon sidecar, then preserve portability."""
    try:
        destination.hardlink_to(source)
    except OSError:
        shutil.copy2(source, destination)


def mark_executable(path: Path) -> None:
    try:
        path.chmod(path.stat().st_mode | 0o755)
    except OSError:
        # The Windows executable bit is irrelevant, and archive creation still
        # carries the regular file.  Preserve the old workflow's best-effort
        # chmod behavior on hosts that reject the mode change.
        pass


def stage_release_binaries(target: str, release_dir: Path, package_dir: Path) -> list[Path]:
    """Populate ``package_dir`` with the executable, daemon, and debug sidecars."""
    suffix = binary_suffix(target)
    source = release_dir / f"soldr{suffix}"
    if not source.is_file():
        raise StagingError(
            f"expected soldr{suffix} in target dir; observed release dir:\n"
            f"{release_contents(release_dir)}"
        )

    package_dir.mkdir(parents=True, exist_ok=True)
    staged: list[Path] = []
    soldr = package_dir / source.name
    shutil.copy2(source, soldr)
    mark_executable(soldr)
    staged.append(soldr)

    daemon = package_dir / f"soldr-daemon{suffix}"
    copy_or_link(soldr, daemon)
    mark_executable(daemon)
    staged.append(daemon)

    if target.endswith("-pc-windows-msvc"):
        pdb = first_file(release_dir, ["soldr.pdb", "soldr_cli.pdb"])
        if pdb is None:
            raise StagingError(
                f"expected a soldr PDB sidecar next to soldr{suffix}; observed release dir:\n"
                f"{release_contents(release_dir)}"
            )
        destination = package_dir / pdb.name
        shutil.copy2(pdb, destination)
        staged.append(destination)
    elif "-unknown-linux-" in target:
        dwp = first_file(release_dir, ["soldr.dwp", "soldr_cli.dwp"])
        if dwp is not None:
            destination = package_dir / dwp.name
            shutil.copy2(dwp, destination)
            staged.append(destination)
            print(f"staged Linux split-DWARF sidecar: {dwp.name}")
    elif target.endswith("-apple-darwin"):
        dsym = first_directory(release_dir, ["soldr.dSYM", "soldr_cli.dSYM"])
        if dsym is not None:
            destination = package_dir / dsym.name
            shutil.copytree(dsym, destination)
            staged.append(destination)
            print(f"staged macOS dSYM sidecar bundle: {dsym.name}")

    print("--- staged release package ---")
    print(release_contents(package_dir))
    return staged


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--package-dir", type=Path, default=Path("dist/package"))
    parser.add_argument("--release-dir", type=Path)
    args = parser.parse_args(argv)
    release_dir = args.release_dir or Path("target") / args.target / "release"
    try:
        stage_release_binaries(args.target, release_dir, args.package_dir)
    except (OSError, StagingError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
