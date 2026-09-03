#!/usr/bin/env python3
"""Native smoke for a release archive + wheel pair (soldr#2294).

Release binaries are cross-built on Linux; EXECUTION still needs the
target's native OS. The `smoke_macos_x64` and `smoke_windows` jobs in
`release-auto.yml` download the lane's artifacts into `dist/` and run
this script. It:

1. checks the wheel's declared version (soldr#1202);
2. extracts the `.tar.zst` archive (host-side, via `tar` -- soldr ships as a
   Python C-extension module (`soldr._native`, see pyproject.toml
   `[tool.maturin] module-name`), so a *wheel's* console script can only run
   on a Python able to import that native module for its own platform; the
   *archive*, by contrast, is a plain compressed tarball and needs nothing
   but `tar` to open);
3. asserts the required bundle members are present (including the
   required `.pdb` sidecar on Windows, docs/DEBUG_SIDECARS.md);
4. on macOS, asserts the Mach-O architecture matches the target;
5. executes every bundled binary from the archive (soldr#1140 stub guard,
   soldr#1202 dispatch-arm guard, then a smoke pass over the rest).

Runnable locally: `python3 ci/smoke_release_artifacts.py
--target aarch64-pc-windows-msvc --expected-version v0.8.40 --dist dist`.

soldr#3071: no macos-* GitHub Actions runner exists any more, so macOS
binary execution (steps 1's version check now folds into step 5, plus step
4's Mach-O check reads bytes directly) happens inside a dockur/macos x86_64
guest instead of this host -- the guest has neither Python nor Xcode CLT.
`--guest-sync-dest <guest dir>` syncs the extracted archive into the guest
(via `ci/macos_x64_guest.py sync-in`) right after extraction, and
`--exec-prefix` names the argv prefix (shlex-split, e.g.
`python3 ci/macos_x64_guest.py exec --cwd <guest dir> --`) that routes each
archive binary invocation there afterwards, by basename, relative to that
synced `--cwd`. Everything that does not need to *run* a binary (archive
layout, wheel version, the Mach-O magic-byte check) stays entirely
host-side regardless of either flag.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
import zipfile
from pathlib import Path

MIN_SOLDR_BYTES = 2 * 1024 * 1024  # soldr#1140 stub floor

GUEST_SCRIPT = Path(__file__).resolve().parent / "macos_x64_guest.py"

# soldr#3071: `lipo`/`file` need Xcode CLT, which neither a bare Linux host
# nor the dockur guest has. A 64-bit little-endian Mach-O starts with the
# MH_MAGIC_64 magic number, then a mach/machine.h `cputype` (also
# little-endian here since the magic identifies this as the native-endian
# form): CPU_TYPE_X86_64 = 0x01000007, CPU_TYPE_ARM64 = 0x0100000C.
MACHO_MAGIC_64 = b"\xcf\xfa\xed\xfe"
_MACHO_CPUTYPE = {
    "x86_64": b"\x07\x00\x00\x01",
    "arm64": b"\x0c\x00\x00\x01",
}


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
    """Expected Mach-O `cputype` label for darwin targets, else None."""
    if target == "aarch64-apple-darwin":
        return "arm64"
    if target == "x86_64-apple-darwin":
        return "x86_64"
    return None


def check_macho_architecture(binary: Path, expected_arch: str) -> None:
    """Read the Mach-O header directly rather than shelling out to `lipo`."""
    header = binary.read_bytes()[:16]
    if header[:4] != MACHO_MAGIC_64:
        sys.exit(f"ERROR: {binary}: not a 64-bit Mach-O binary (magic={header[:4]!r})")
    expected_bytes = _MACHO_CPUTYPE.get(expected_arch)
    if expected_bytes is None:
        sys.exit(f"ERROR: no known Mach-O cputype for {expected_arch!r}")
    cputype = header[4:8]
    if cputype != expected_bytes:
        sys.exit(
            f"ERROR: {binary}: expected Mach-O cputype for {expected_arch}, "
            f"got {cputype!r}"
        )
    print(f"Mach-O architecture OK: {binary} is {expected_arch}")


def wheel_version(wheel: Path) -> str:
    """Read the `Version:` field from the wheel's own METADATA, unzipped.

    No install, no execution -- a wheel is a zip file, and this needs
    nothing about the host or the wheel's target platform.
    """
    with zipfile.ZipFile(wheel) as archive:
        metadata_names = [
            name for name in archive.namelist() if name.endswith(".dist-info/METADATA")
        ]
        if len(metadata_names) != 1:
            raise RuntimeError(
                f"expected exactly one *.dist-info/METADATA in {wheel}, "
                f"found {metadata_names}"
            )
        metadata = archive.read(metadata_names[0]).decode("utf-8", errors="replace")
    match = re.search(r"^Version:\s*(\S+)\s*$", metadata, re.MULTILINE)
    if not match:
        raise RuntimeError(f"no Version: field in {wheel}'s METADATA")
    return match.group(1)


def extract_archive(archive: Path, dest: Path) -> None:
    """Unpack the release `.tar.zst` with `tar`, not the packaged soldr.

    Modern GNU tar and bsdtar (both ship on the ubuntu-24.04 and
    windows-2025 hosted runners) auto-detect zstd compression from the file
    itself, so no `--zstd` flag or external `unzstd` pipe is required.
    """
    dest.mkdir(parents=True, exist_ok=True)
    subprocess.run(["tar", "-xf", str(archive), "-C", str(dest)], check=True)


def sync_extracted_into_guest(extract: Path, guest_dest: str) -> None:
    """Ship the extracted archive into the guest before anything execs there."""
    subprocess.run(
        [
            sys.executable,
            str(GUEST_SCRIPT),
            "sync-in",
            "--src",
            str(extract),
            "--dest",
            guest_dest,
        ],
        check=True,
    )


def build_argv(exec_prefix: str | None, command: list[str | Path]) -> list[str]:
    """Prepend the guest-routing prefix (if any) to a binary invocation.

    When routed to the guest, every `Path` element is reduced to its
    basename: `--exec-prefix` pairs with `--guest-sync-dest`, which syncs
    the *contents* of the host's `extracted/` directory into that guest
    directory and sets it as the exec's `--cwd`, so a guest-relative
    basename is what actually resolves there -- the host's absolute
    extraction path never existed on the guest. Plain string arguments
    (flags, `--version`, ...) are passed through unchanged.
    """
    if not exec_prefix:
        return [str(part) for part in command]
    argv = [part.name if isinstance(part, Path) else str(part) for part in command]
    return shlex.split(exec_prefix) + argv


def run(
    cmd: list[str | Path], *, exec_prefix: str | None = None
) -> subprocess.CompletedProcess[str]:
    argv = build_argv(exec_prefix, cmd)
    print(f"+ {' '.join(argv)}", flush=True)
    return subprocess.run(argv, check=True, capture_output=True, text=True)


def check_version_output(
    binary: Path, expected: str, label: str, *, exec_prefix: str | None = None
) -> None:
    out = run([binary, "--version"], exec_prefix=exec_prefix).stdout.strip()
    print(f"{label} — soldr --version: {out}")
    if not out.startswith("soldr "):
        sys.exit(
            f"ERROR: {label}: 'soldr --version' output {out!r} does not start "
            "with 'soldr ' — likely a stub binary (soldr#1140)."
        )
    json_out = run(
        [binary, "version", "--json"], exec_prefix=exec_prefix
    ).stdout.strip()
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
    parser.add_argument(
        "--exec-prefix",
        default=None,
        help=(
            "shlex-split argv prefix that routes a binary invocation to "
            "where the target actually runs, e.g. a "
            "'ci/macos_x64_guest.py exec --cwd <dir> --' guest command "
            "(soldr#3071). Omit to run every binary directly on this host."
        ),
    )
    parser.add_argument(
        "--guest-sync-dest",
        default=None,
        help=(
            "guest directory to sync the extracted archive into right after "
            "extraction, via 'ci/macos_x64_guest.py sync-in' (soldr#3071). "
            "Pair with --exec-prefix's matching --cwd."
        ),
    )
    args = parser.parse_args()

    target: str = args.target
    version: str = args.expected_version
    expected = version.removeprefix("v")
    suffix = exe_suffix(target)
    exec_prefix = args.exec_prefix

    wheels = sorted(args.dist.glob("*.whl"))
    if len(wheels) != 1:
        sys.exit(f"expected exactly one wheel in {args.dist}, found {wheels}")

    reported_wheel_version = wheel_version(wheels[0])
    if reported_wheel_version != expected:
        sys.exit(
            f"ERROR: wheel {wheels[0]}: METADATA Version {reported_wheel_version!r} "
            f"!= expected {expected!r} (soldr#1202)."
        )
    print(f"{target} wheel — METADATA Version: {reported_wheel_version}")

    archive = args.dist / f"soldr-{version}-{target}.tar.zst"
    if not archive.is_file():
        sys.exit(f"missing archive: {archive}")
    extract = Path("extracted")
    extract_archive(archive, extract)

    if args.guest_sync_dest:
        sync_extracted_into_guest(extract, args.guest_sync_dest)

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
        check_macho_architecture(soldr_bin, arch)

    check_version_output(
        soldr_bin, expected, f"{target} archive", exec_prefix=exec_prefix
    )
    run([extract / f"soldr-daemon{suffix}", "--help"], exec_prefix=exec_prefix)
    run([extract / f"crgx{suffix}", "--version"], exec_prefix=exec_prefix)
    run([extract / f"cargo-chef{suffix}", "--version"], exec_prefix=exec_prefix)
    print(f"native smoke OK: {target} {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
