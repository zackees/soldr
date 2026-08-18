#!/usr/bin/env python3
"""Create a combined Soldr release archive (soldr#2469 step 2.2).

The release archive is written by Soldr's in-process Rust zstd encoder instead
of a runner-local ``zstd`` executable.  The driver follows the *runner* OS,
not the archive target: native Windows builds use ``soldr.exe`` while Linux
cross-builds retain the bootstrap ``soldr`` driver.

Usage (CI):
    python3 .github/scripts/package_release_archive.py \
        --version v0.9.2 --target x86_64-unknown-linux-gnu --runner-os Linux
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

from release_artifacts import runner_binary_suffix


class ArchivePackagingError(RuntimeError):
    """The release archive could not be created or was not materialized."""


def driver_path(runner_os: str, driver_dir: Path) -> Path:
    """Return the checked-out Soldr driver executable for a CI runner."""
    return driver_dir / f"soldr{runner_binary_suffix(runner_os)}"


def archive_path(version: str, target: str, output_dir: Path) -> Path:
    return output_dir / f"soldr-{version}-{target}.tar.zst"


def package_archive(
    *,
    version: str,
    target: str,
    runner_os: str,
    package_dir: Path,
    output_dir: Path,
    driver_dir: Path,
) -> Path:
    """Archive staged release files and report the resulting compressed size."""
    driver = driver_path(runner_os, driver_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    output = archive_path(version, target, output_dir)
    subprocess.run(
        [str(driver), "archive", "--stage-dir", str(package_dir), "--output", str(output)],
        check=True,
    )
    if not output.is_file():
        raise ArchivePackagingError(f"archive command did not create {output}")
    print(f"archive: {output} ({output.stat().st_size} bytes)")
    print(f"compressed_size_bytes={output.stat().st_size}")
    return output


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--runner-os", required=True, choices=["Linux", "macOS", "Windows"])
    parser.add_argument("--package-dir", type=Path, default=Path("dist/package"))
    parser.add_argument("--output-dir", type=Path, default=Path("dist"))
    parser.add_argument("--driver-dir", type=Path, default=Path("target/release"))
    args = parser.parse_args(argv)
    try:
        package_archive(
            version=args.version,
            target=args.target,
            runner_os=args.runner_os,
            package_dir=args.package_dir,
            output_dir=args.output_dir,
            driver_dir=args.driver_dir,
        )
    except (ArchivePackagingError, OSError, subprocess.CalledProcessError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
