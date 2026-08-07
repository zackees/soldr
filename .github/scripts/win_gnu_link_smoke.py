#!/usr/bin/env python3
"""Scheduled win-gnu link smoke: cross-build a fixture to
`x86_64-pc-windows-gnu` and assert the output is a real PE.

soldr#2336's last checklist item — a scheduled-only (never path-triggered)
win-gnu CI lane, with a Windows-host and a Linux-host lane. #2341 landed the
consumption path (Windows host → WinLibs gcc; Linux x64 host → conda-forge mingw
*cross* gcc; PE-link-proven on forge). This is the regression canary for it.

Rather than parse `soldr build` logs, the check is end-to-end and objective:
build a minimal crate through the blessed surface and assert the emitted binary
is a **PE32+ x86-64** image. The PE parse is pure and unit-tested by
`test_win_gnu_link_smoke.py`; the `smoke` subcommand wires it to a real build.

`--no-cache` is deliberate: it exercises the *toolchain/link* path, which is
what this lane guards, and side-steps an unrelated embedded-cache defect on
win-gnu output (no `.pdb` is produced, so the cache's compiler-output
materialization trips). That defect is tracked separately; conflating it with a
link regression here would make the canary lie.

Subcommands
-----------
  smoke --soldr <bin> [--target x86_64-pc-windows-gnu] [--summary <file>]
        create a fixture, `soldr build --no-cache --target ...`, assert the
        output is a PE32+ x86-64 executable. Exit non-zero on any failure.
  assert-pe --path <file>
        assert one existing file is a PE32+ x86-64 image (used in tests/ad hoc).
"""

from __future__ import annotations

import argparse
import os
import struct
import subprocess
import sys
import tempfile

TARGET = "x86_64-pc-windows-gnu"
TOOLCHAIN = "1.94.1"
IMAGE_FILE_MACHINE_AMD64 = 0x8664


def is_pe_amd64(path: str) -> tuple[bool, str]:
    """Return (ok, reason). `ok` is True only for a PE whose COFF machine is
    AMD64 (x86-64). Parses the DOS stub `e_lfanew` -> `PE\\0\\0` -> machine word,
    the minimal structure that distinguishes a real PE from an ELF/Mach-O or a
    truncated file — no external `file`/objdump dependency."""
    try:
        with open(path, "rb") as fh:
            data = fh.read(0x400)
    except OSError as exc:
        return False, f"cannot read {path}: {exc}"
    if len(data) < 0x40 or data[:2] != b"MZ":
        return False, "missing 'MZ' DOS signature (not a PE)"
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if e_lfanew + 6 > len(data):
        return False, f"PE header offset {e_lfanew:#x} outside the read window"
    if data[e_lfanew : e_lfanew + 4] != b"PE\x00\x00":
        return False, "missing 'PE\\0\\0' signature at e_lfanew"
    machine = struct.unpack_from("<H", data, e_lfanew + 4)[0]
    if machine != IMAGE_FILE_MACHINE_AMD64:
        return (
            False,
            f"COFF machine {machine:#06x} is not AMD64 ({IMAGE_FILE_MACHINE_AMD64:#06x})",
        )
    return True, "PE32+ x86-64"


def _write_fixture(root: str) -> str:
    """Materialize a minimal buildable bin crate, pinned so soldr's toolchain
    contract is satisfied without an `--allow-unpinned` escape hatch."""
    crate = os.path.join(root, "wg_smoke")
    os.makedirs(os.path.join(crate, "src"), exist_ok=True)
    with open(os.path.join(crate, "Cargo.toml"), "w", encoding="utf-8") as fh:
        fh.write(
            "[package]\n"
            'name = "wg_smoke"\n'
            'version = "0.0.0"\n'
            'edition = "2021"\n\n'
            "[profile.dev]\n"
            "debug = false\n"
        )
    with open(os.path.join(crate, "src", "main.rs"), "w", encoding="utf-8") as fh:
        fh.write('fn main() {\n    println!("win-gnu link smoke");\n}\n')
    with open(os.path.join(crate, "rust-toolchain.toml"), "w", encoding="utf-8") as fh:
        fh.write(f'[toolchain]\nchannel = "{TOOLCHAIN}"\n')
    return crate


def _output_exe(crate: str, target: str) -> str:
    # CI leaves CARGO_TARGET_DIR unset -> per-crate `target/`. Some dev
    # containers export a shared one; honor it so the smoke works in both.
    base = os.environ.get("CARGO_TARGET_DIR") or os.path.join(crate, "target")
    return os.path.join(base, target, "debug", "wg_smoke.exe")


def cmd_smoke(args: argparse.Namespace) -> int:
    with tempfile.TemporaryDirectory() as tmp:
        crate = _write_fixture(tmp)
        cmd = [args.soldr, "--no-cache", "build", "--target", args.target]
        print(f"$ {' '.join(cmd)}  (cwd={crate})", flush=True)
        proc = subprocess.run(cmd, cwd=crate, check=False)
        if proc.returncode != 0:
            print(f"FAIL: soldr build exited {proc.returncode}", file=sys.stderr)
            return proc.returncode or 1
        exe = _output_exe(crate, args.target)
        ok, reason = is_pe_amd64(exe)
        summary = (
            f"## win-gnu link smoke ({args.target})\n\n"
            f"- build: `soldr --no-cache build --target {args.target}`\n"
            f"- output: `{os.path.basename(exe)}`\n"
            f"- verdict: {'✅ ' if ok else '❌ '}{reason}\n"
        )
        if args.summary:
            with open(args.summary, "a", encoding="utf-8") as fh:
                fh.write(summary)
        print(summary)
        if not ok:
            print(f"FAIL: {reason}", file=sys.stderr)
            return 1
        print("OK: produced a PE32+ x86-64 win-gnu executable.")
        return 0


def cmd_assert_pe(args: argparse.Namespace) -> int:
    ok, reason = is_pe_amd64(args.path)
    print(reason)
    return 0 if ok else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    p_smoke = sub.add_parser("smoke", help="build a fixture and assert a PE")
    p_smoke.add_argument("--soldr", required=True, help="path to the soldr binary")
    p_smoke.add_argument("--target", default=TARGET)
    p_smoke.add_argument("--summary", default=None)
    p_smoke.set_defaults(func=cmd_smoke)

    p_pe = sub.add_parser("assert-pe", help="assert a file is a PE32+ x86-64 image")
    p_pe.add_argument("--path", required=True)
    p_pe.set_defaults(func=cmd_assert_pe)

    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
