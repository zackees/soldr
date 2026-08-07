#!/usr/bin/env python3
"""Native smoke for a release archive + wheel pair (soldr#2294).

Release binaries are cross-built on Linux; EXECUTION still needs the
target's native OS. The `smoke_macos_arm64` and `smoke_windows` jobs in
`release-auto.yml` download the lane's artifacts into `dist/` and run
this script on a native runner. It:

1. installs the wheel into a fresh venv and checks `soldr --version`
   (stub guard, soldr#1140) and `soldr version --json` (dispatch-arm
   guard, soldr#1202) report the expected version;
2. extracts the `.tar.zst` archive with the installed soldr;
3. asserts the required bundle members are present (including the
   required `.pdb` sidecar on Windows, docs/DEBUG_SIDECARS.md);
4. on macOS, asserts the Mach-O architecture matches the target;
5. executes every bundled binary.

Runnable locally: `python3 ci/smoke_release_artifacts.py
--target aarch64-apple-darwin --expected-version v0.8.40 --dist dist`.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import venv
from pathlib import Path

MIN_SOLDR_BYTES = 2 * 1024 * 1024  # soldr#1140 stub floor


def exe_suffix(target: str) -> str:
    return ".exe" if target.endswith("-pc-windows-msvc") else ""


def required_members(target: str) -> list[str]:
    """Bundle members whose absence fails the smoke."""
    suffix = exe_suffix(target)
    return [
        f"soldr{suffix}",
        f"soldr-daemon{suffix}",
        f"crgx{suffix}",
        f"cargo-chef{suffix}",
        "manifest.json",
    ]


def macho_arch(target: str) -> str | None:
    """Expected `lipo -archs` token for darwin targets, else None."""
    if target == "aarch64-apple-darwin":
        return "arm64"
    if target == "x86_64-apple-darwin":
        return "x86_64"
    return None


def run(cmd: list[str | Path]) -> subprocess.CompletedProcess[str]:
    printable = " ".join(str(part) for part in cmd)
    print(f"+ {printable}", flush=True)
    return subprocess.run(
        [str(part) for part in cmd],
        check=True,
        capture_output=True,
        text=True,
    )


def check_version_output(binary: Path, expected: str, label: str) -> None:
    out = run([binary, "--version"]).stdout.strip()
    print(f"{label} — soldr --version: {out}")
    if not out.startswith("soldr "):
        sys.exit(
            f"ERROR: {label}: 'soldr --version' output {out!r} does not start "
            "with 'soldr ' — likely a stub binary (soldr#1140)."
        )
    json_out = run([binary, "version", "--json"]).stdout.strip()
    if not json_out:
        sys.exit(
            f"ERROR: {label}: 'soldr version --json' produced empty stdout "
            "(soldr#1202)."
        )
    print(f"{label} — soldr version --json: {json_out}")
    reported = json.loads(json_out).get("soldr_version")
    if reported != expected:
        sys.exit(
            f"ERROR: {label}: soldr_version {reported!r} != expected {expected!r} "
            "(soldr#1202)."
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, help="rust target triple")
    parser.add_argument(
        "--expected-version", required=True, help="release version, vX.Y.Z"
    )
    parser.add_argument("--dist", default="dist", type=Path)
    args = parser.parse_args()

    target: str = args.target
    version: str = args.expected_version
    expected = version.removeprefix("v")
    suffix = exe_suffix(target)

    wheels = sorted(args.dist.glob("*.whl"))
    if len(wheels) != 1:
        sys.exit(f"expected exactly one wheel in {args.dist}, found {wheels}")

    venv_dir = Path(".smoke-venv")
    venv.create(venv_dir, with_pip=True)
    bin_dir = venv_dir / ("Scripts" if sys.platform == "win32" else "bin")
    python = bin_dir / ("python.exe" if sys.platform == "win32" else "python")
    run([python, "-m", "pip", "install", "--disable-pip-version-check", wheels[0]])
    wheel_soldr = bin_dir / f"soldr{suffix}"
    check_version_output(wheel_soldr, expected, f"{target} wheel")

    archive = args.dist / f"soldr-{version}-{target}.tar.zst"
    if not archive.is_file():
        sys.exit(f"missing archive: {archive}")
    extract = Path("extracted")
    extract.mkdir()
    run([wheel_soldr, "archive", "--input", archive, "--extract-dir", extract])

    for member in required_members(target):
        if not (extract / member).is_file():
            sys.exit(f"missing {member} in {archive}")
    if suffix:
        # docs/DEBUG_SIDECARS.md: the PDB sidecar is REQUIRED on Windows.
        pdbs = [p for p in ("soldr.pdb", "soldr_cli.pdb") if (extract / p).is_file()]
        if not pdbs:
            sys.exit(f"missing soldr PDB sidecar in {archive}")
        print(f"PDB sidecar present: {pdbs}")

    soldr_bin = extract / f"soldr{suffix}"
    size = soldr_bin.stat().st_size
    if size < MIN_SOLDR_BYTES:
        sys.exit(
            f"ERROR: {soldr_bin} is {size} bytes, expected >= {MIN_SOLDR_BYTES} "
            "(soldr#1140 / soldr#1202 stub-binary floor)."
        )

    arch = macho_arch(target)
    if arch is not None:
        file_out = run(["file", soldr_bin]).stdout.strip()
        print(f"archive architecture: {file_out}")
        if f"Mach-O 64-bit executable {arch}" not in file_out:
            sys.exit(f"expected a Mach-O {arch} binary, got: {file_out}")
        lipo_out = run(["lipo", "-archs", soldr_bin]).stdout.strip()
        print(f"lipo architectures: {lipo_out}")
        if arch not in lipo_out.split():
            sys.exit(f"expected a thin {arch} binary, lipo reports: {lipo_out}")

    check_version_output(soldr_bin, expected, f"{target} archive")
    run([extract / f"soldr-daemon{suffix}", "--help"])
    run([extract / f"crgx{suffix}", "--version"])
    run([extract / f"cargo-chef{suffix}", "--version"])
    print(f"native smoke OK: {target} {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
