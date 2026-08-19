#!/usr/bin/env python3
"""Smoke the combined `tar.zst` release archive (soldr#2469 step 2.2).

Extracted from 105 lines of inline bash. This is the last gate before an
archive reaches the GitHub release, and it exists because of two shipped
regressions:

* **soldr#1140 / #1202** — a release shipped a 332 KiB *stub* where the real
  ~13-15 MB `soldr` belongs. Hence the 2 MiB floor, which is checked on every
  lane including cross-arch ones where the binary cannot be executed at all.
* **soldr#1202 (v0.7.87)** — that stub's `soldr --version` printed
  `soldr 0.0.1`, passing every "starts with soldr " check, while `soldr
  version --json` produced **empty stdout** and broke every downstream JSON
  consumer including setup-soldr's verify step. `--version` and `version
  --json` are distinct code paths (clap's built-in flag versus the
  `Commands::Version` arm), so both are checked.

The decisions are pure functions — which entries an archive must carry, when
the runner may execute the archive's binary, and whether a `version --json`
payload is acceptable — so the incidents above are unit-tested rather than
re-derived by reading bash.

Usage (CI):
    python3 .github/scripts/release_archive_smoke.py \\
        --version v0.9.2 --target x86_64-unknown-linux-gnu \\
        --binary soldr --archive dist/soldr-v0.9.2-...tar.zst \\
        --driver target/release/soldr --runner-os Linux
"""

from __future__ import annotations

import argparse
import platform
import subprocess
import sys
import tempfile
from pathlib import Path

from release_artifacts import (
    binary_suffix,
    normalized_release_version,
    runner_binary_suffix,
    version_json_status,
)

# soldr#1202: real soldr is ~13-15 MB on every platform. Two MiB is far below
# that and far above any conceivable stub, so it rejects the regression
# without becoming a size assertion nobody can maintain.
MIN_SOLDR_BYTES = 2 * 1024 * 1024


class SmokeError(RuntimeError):
    """An archive defect that must stop the release."""


def required_entries(target: str, binary: str) -> list[str]:
    """Every archive ships soldr, its daemon, crgx, cargo-chef, a manifest."""
    suffix = binary_suffix(target)
    return [
        binary,
        f"soldr-daemon{suffix}",
        f"crgx{suffix}",
        f"cargo-chef{suffix}",
        "manifest.json",
    ]


def archive_path(version: str, target: str, dist_dir: Path) -> Path:
    return dist_dir / f"soldr-{version}-{target}.tar.zst"


def driver_path(runner_os: str, driver_dir: Path) -> Path:
    return driver_dir / f"soldr{runner_binary_suffix(runner_os)}"


def native_arch_match(runner_os: str, runner_arch: str, target: str) -> bool:
    """Whether this runner can execute the archive's binary.

    Cross-arch archives cannot run — an aarch64 build on an x86_64 release
    runner — so the dynamic checks are gated on a match and the static ones
    (layout, stub floor) are not.

    `arm64` is listed beside `aarch64` because that is what `uname -m` reports
    on Apple silicon; dropping it would silently skip every dynamic check on
    the macOS ARM lane while still reporting success.
    """
    os_matches = (
        (runner_os == "Linux" and "-unknown-linux-" in target)
        or (runner_os == "macOS" and target.endswith("-apple-darwin"))
        or (runner_os == "Windows" and target.endswith("-pc-windows-msvc"))
    )
    arch_matches = (runner_arch == "x86_64" and target.startswith("x86_64-")) or (
        runner_arch in {"aarch64", "arm64"} and target.startswith("aarch64-")
    )
    return os_matches and arch_matches


def stub_floor_problem(size: int, name: str) -> str | None:
    if size >= MIN_SOLDR_BYTES:
        return None
    return (
        f"ERROR: '{name}' is {size} bytes; expected >= {MIN_SOLDR_BYTES} (2 MiB). "
        "This is the soldr#1140 / soldr#1202 stub-binary regression — the release "
        "archive shipped a placeholder instead of the real soldr binary."
    )


def version_json_problem(output: str, expected_version: str) -> str | None:
    """Validate `soldr version --json` the way the incident requires.

    Empty stdout is the v0.7.87 signature. The normalized version comparison
    lives in ``release_artifacts`` so every release smoke gate follows the
    same contract.
    """
    status = version_json_status(output, expected_version)
    if status == "empty":
        return (
            "ERROR: 'soldr version --json' produced empty stdout — likely a stub "
            "binary (soldr#1140 / soldr#1202)."
        )
    if status in {"mismatch", "invalid"}:
        return (
            "ERROR: 'soldr version --json' output does not include "
            f"soldr_version={expected_version} (soldr#1202)."
        )
    return None


def find_pdb(extract_dir: Path) -> Path | None:
    for name in ("soldr.pdb", "soldr_cli.pdb"):
        candidate = extract_dir / name
        if candidate.is_file():
            return candidate
    return None


def run(command: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, capture_output=True, text=True, check=False)


def smoke(args: argparse.Namespace) -> None:
    archive = Path(args.archive)
    if not archive.is_file():
        raise SmokeError(f"missing archive: {archive}")

    extract = Path(tempfile.mkdtemp(prefix="soldr-archive-smoke-"))
    extracted = run(
        [args.driver, "archive", "--input", str(archive), "--extract-dir", str(extract)]
    )
    if extracted.returncode != 0:
        raise SmokeError(
            f"archive extraction failed:\n{extracted.stdout}\n{extracted.stderr}"
        )
    print("--- extracted layout ---")
    for entry in sorted(extract.iterdir()):
        print(f"  {entry.name}")

    for required in required_entries(args.target, args.binary):
        if not (extract / required).is_file():
            raise SmokeError(f"missing {required} in {archive}")

    suffix = binary_suffix(args.target)
    if suffix and find_pdb(extract) is None:
        raise SmokeError(f"missing soldr PDB sidecar in {archive}")

    print("--- extracted manifest.json ---")
    print((extract / "manifest.json").read_text(encoding="utf-8"))

    soldr = extract / args.binary
    problem = stub_floor_problem(soldr.stat().st_size, str(soldr))
    if problem:
        raise SmokeError(problem)
    print(
        f"archive smoke test — {args.binary} size {soldr.stat().st_size} bytes "
        f">= {MIN_SOLDR_BYTES} floor OK"
    )

    runner_arch = platform.machine()
    if not native_arch_match(args.runner_os, runner_arch, args.target):
        print(
            f"skipping --version (runner {args.runner_os}/{runner_arch} vs target {args.target})"
        )
        return

    version_flag = run([str(soldr), "--version"])
    if version_flag.returncode != 0:
        raise SmokeError(f"`soldr --version` failed:\n{version_flag.stderr}")
    print(version_flag.stdout.strip())

    version_json = run([str(soldr), "version", "--json"])
    problem = version_json_problem(
        version_json.stdout, normalized_release_version(args.version)
    )
    if problem:
        raise SmokeError(problem)
    print(f"soldr version --json output: {version_json.stdout.strip()}")

    for tool, tool_args in (
        (f"soldr-daemon{suffix}", ["--help"]),
        (f"crgx{suffix}", ["--version"]),
        (f"cargo-chef{suffix}", ["--version"]),
    ):
        result = run([str(extract / tool), *tool_args])
        if result.returncode != 0:
            raise SmokeError(f"bundled {tool} failed:\n{result.stderr}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--binary", required=True, help="soldr binary name in archive")
    parser.add_argument("--archive", default=None)
    parser.add_argument("--driver", default=None, help="host soldr used to extract")
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    parser.add_argument("--driver-dir", type=Path, default=Path("target/release"))
    parser.add_argument(
        "--runner-os", required=True, choices=["Linux", "macOS", "Windows"]
    )
    args = parser.parse_args(argv)
    if args.archive is None:
        args.archive = str(archive_path(args.version, args.target, args.dist))
    if args.driver is None:
        args.driver = str(driver_path(args.runner_os, args.driver_dir))
    try:
        smoke(args)
    except SmokeError as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
