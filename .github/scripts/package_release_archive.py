#!/usr/bin/env python3
"""Create a combined Soldr release archive (soldr#2469 step 2.2).

The release archive is written by Soldr's in-process Rust zstd encoder instead
of a runner-local ``zstd`` executable.  The driver follows the *runner* OS,
not the archive target: native Windows builds use ``soldr.exe`` while Linux
cross-builds retain the bootstrap ``soldr`` driver.

soldr#3038: also used, with ``--label symbols --allow-empty``, to package the
separate debug-symbols asset (Linux ``.dwp`` / macOS ``.dSYM``) staged by
``stage_release_binaries.py::stage_debug_symbols`` into its own directory.
That directory is legitimately empty on targets with no split-debug sidecar
(Windows always), so ``--allow-empty`` turns "nothing staged" into a clean
no-op print instead of failing the release on a target that never had
anything to ship there.

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


def archive_path(version: str, target: str, output_dir: Path, label: str = "") -> Path:
    suffix = f"-{label}" if label else ""
    return output_dir / f"soldr-{version}-{target}{suffix}.tar.zst"


def package_dir_is_empty(package_dir: Path) -> bool:
    """True when there is nothing to archive: absent, or present but empty.

    Used by the ``--allow-empty`` symbols path -- ``soldr archive`` itself
    (``build_stage_dir_archive`` in ``archive_cmd.rs``) errors on an empty
    stage dir, which is correct for the main package but wrong for an
    optional per-target debug-symbols directory that legitimately has
    nothing in it (soldr#3038).
    """
    return not package_dir.is_dir() or not any(package_dir.iterdir())


def package_archive(
    *,
    version: str,
    target: str,
    runner_os: str,
    package_dir: Path,
    output_dir: Path,
    driver_dir: Path,
    label: str = "",
    allow_empty: bool = False,
) -> Path | None:
    """Archive staged release files and report the resulting compressed size.

    Returns ``None`` (rather than raising) only when ``allow_empty`` is set
    and ``package_dir`` has nothing staged -- see ``package_dir_is_empty``.
    """
    if allow_empty and package_dir_is_empty(package_dir):
        print(
            f"package_archive: {package_dir} has nothing staged; skipping ({label or 'main'})"
        )
        return None
    driver = driver_path(runner_os, driver_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    output = archive_path(version, target, output_dir, label=label)
    subprocess.run(
        [
            str(driver),
            "archive",
            "--stage-dir",
            str(package_dir),
            "--output",
            str(output),
        ],
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
    parser.add_argument(
        "--runner-os", required=True, choices=["Linux", "macOS", "Windows"]
    )
    parser.add_argument("--package-dir", type=Path, default=Path("dist/package"))
    parser.add_argument("--output-dir", type=Path, default=Path("dist"))
    parser.add_argument("--driver-dir", type=Path, default=Path("target/release"))
    parser.add_argument(
        "--label",
        default="",
        help="Appended to the archive filename as -<label> (soldr#3038: 'symbols').",
    )
    parser.add_argument(
        "--allow-empty",
        action="store_true",
        help="Skip cleanly instead of failing when --package-dir has nothing staged.",
    )
    args = parser.parse_args(argv)
    try:
        package_archive(
            version=args.version,
            target=args.target,
            runner_os=args.runner_os,
            package_dir=args.package_dir,
            output_dir=args.output_dir,
            driver_dir=args.driver_dir,
            label=args.label,
            allow_empty=args.allow_empty,
        )
    except (ArchivePackagingError, OSError, subprocess.CalledProcessError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
