#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# ///
from __future__ import annotations

import argparse
import contextlib
import platform
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Literal

from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "dist"
TRAMPOLINE_ASSETS = ROOT / "src" / "running_process" / "assets"

BuildMode = Literal["dev", "release"]


def preserve_dev_pdb() -> Path:
    """Keep the exact dev-wheel PDB before later Cargo lanes can replace it."""
    from ci.env import host_target_triple

    triple = host_target_triple()
    source = ROOT / "target" / triple / "debug" / "_native.pdb"
    if not source.is_file():
        raise RuntimeError(f"dev native PDB missing after wheel build: {source}")
    destination = ROOT / "target" / "probe-symbols" / triple / "_native.pdb"
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)
    return destination


def build_command(mode: BuildMode, *, rustc_args: list[str] | None = None) -> list[str]:
    cmd = [
        sys.executable,
        "-m",
        "maturin",
        "build",
        "--interpreter",
        sys.executable,
        "--out",
        str(DIST),
    ]
    if mode == "dev":
        cmd.extend(["--profile", "dev"])
    else:
        cmd.append("--release")
        if platform.system() == "Linux":
            # An old-glibc manylinux tag, without zig.
            #
            # This used to pass maturin's zig flag, which links through zig cc to target
            # an older glibc than the build host has. That is a second
            # cross-compile toolchain competing with soldr's blessed one, so
            # `ci/cross_compiler_guard.py` now forbids it.
            #
            # The zig-free way to get an old glibc is to build where that
            # glibc actually lives: CI runs this job inside the
            # `quay.io/pypa/manylinux_2_28_*` image, whose toolchain is
            # glibc 2.28 natively. Nothing here has to ask for a lower
            # baseline than the host provides.
            #
            # 2.28 rather than 2014/2.17 because GitHub's runner Node needs
            # GLIBC_2.28 and cannot execute inside the 2.17 image — the JS
            # actions this build depends on die before any build step.
            #
            # Dropping the flag without the container would have silently
            # tagged wheels against the runner's glibc (2.39 on ubuntu-24.04),
            # so anyone on an older distro would lose prebuilt wheels and fall
            # back to a source build.
            cmd.extend(["--compatibility", "manylinux_2_28"])
        else:
            cmd.extend(["--compatibility", "pypi"])
    if rustc_args:
        cmd.append("--")
        cmd.extend(rustc_args)
    return cmd


def built_wheels() -> list[Path]:
    return sorted(
        DIST.glob("running_process-*.whl"), key=lambda path: path.stat().st_mtime
    )


def latest_wheel() -> Path:
    wheels = built_wheels()
    if not wheels:
        raise RuntimeError(f"no built wheel found in {DIST}")
    return wheels[-1]


def install_wheel(wheel: Path, *, env: dict[str, str]) -> int:
    install = subprocess.run(
        [
            "uv",
            "pip",
            "install",
            "--python",
            sys.executable,
            "--reinstall",
            "--no-deps",
            str(wheel),
        ],
        cwd=ROOT,
        check=False,
        env=env,
    )
    if install.returncode != 0:
        return install.returncode

    # Clean up the stale editable path file if a prior `maturin develop` left one behind.
    for pth in (ROOT / ".venv").glob("**/site-packages/running_process.pth"):
        with contextlib.suppress(OSError):
            pth.unlink()
    return 0


def build_trampoline(mode: BuildMode, *, env: dict[str, str] | None = None) -> int:
    """Build the daemon-trampoline binary and copy it into package assets."""
    import json as json_mod

    profile_args = ["--release"] if mode == "release" else []
    # Wave 7 of #165: daemon-trampoline is now a [[bin]] inside the
    # `running-process` crate; select it by binary name rather than
    # by the old standalone package name.
    result = subprocess.run(
        cargo_command(
            "build",
            "--bin",
            "daemon-trampoline",
            "--message-format=json",
            *profile_args,
        ),
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )
    if result.returncode != 0:
        print(result.stderr, file=sys.stderr, flush=True)
        return result.returncode

    # Parse the JSON output to find the executable path.
    src: Path | None = None
    for line in result.stdout.splitlines():
        line = line.strip()
        if not line or not line.startswith("{"):
            continue
        with contextlib.suppress(json_mod.JSONDecodeError):
            msg = json_mod.loads(line)
            if (
                msg.get("reason") == "compiler-artifact"
                and msg.get("target", {}).get("name") == "daemon-trampoline"
                and msg.get("executable")
            ):
                src = Path(msg["executable"])
                break

    if src is None or not src.exists():
        print(
            f"trampoline binary not found in cargo output (searched {src})",
            file=sys.stderr,
            flush=True,
        )
        print(f"cargo stderr:\n{result.stderr}", file=sys.stderr, flush=True)
        return 1

    dest = TRAMPOLINE_ASSETS / src.name
    TRAMPOLINE_ASSETS.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dest)
    print(f"trampoline: {src} -> {dest}", file=sys.stderr, flush=True)
    return 0


def run_build(mode: BuildMode) -> int:
    from ci.env import build_env
    from ci.tiny_pdb import (
        apply_tiny_pdb_env,
        bundle_windows_tiny_pdb,
        filter_public_pdb,
        filtered_pdb_path,
        final_crate_rustc_args,
        stripped_pdb_path,
    )
    from ci.verify_release_symbols import (
        format_release_artifact_report,
        verify_release_artifact,
    )

    env = build_env()
    rc = build_trampoline(mode, env=env)
    if rc != 0:
        print("trampoline build failed", file=sys.stderr, flush=True)
        return rc

    rustc_args: list[str] = []
    if mode == "release":
        env = apply_tiny_pdb_env(env)
        if platform.system() == "Windows":
            rustc_args = final_crate_rustc_args(ROOT)
    DIST.mkdir(parents=True, exist_ok=True)
    before = {path.name for path in built_wheels()}
    cmd = build_command(mode, rustc_args=rustc_args)
    print(f"build mode: {mode}", file=sys.stderr, flush=True)
    result = subprocess.run(cmd, cwd=ROOT, check=False, env=env)
    if result.returncode != 0:
        return result.returncode
    if mode == "dev" and platform.system() == "Windows":
        preserved = preserve_dev_pdb()
        print(
            f"preserved exact dev-wheel PDB for probe tests: {preserved}",
            file=sys.stderr,
            flush=True,
        )
    if mode == "release" and platform.system() == "Windows":
        tiny_pdb = filter_public_pdb(
            source_pdb=stripped_pdb_path(ROOT),
            destination_pdb=filtered_pdb_path(ROOT),
            root=ROOT,
        )
        new_wheels = [path for path in built_wheels() if path.name not in before]
        for wheel in new_wheels or [latest_wheel()]:
            bundled = bundle_windows_tiny_pdb(wheel, tiny_pdb=tiny_pdb, root=ROOT)
            print(
                f"bundled tiny PDB into {wheel.name}: {', '.join(bundled)}",
                file=sys.stderr,
                flush=True,
            )
            report = verify_release_artifact(wheel)
            print(format_release_artifact_report(report), file=sys.stderr, flush=True)
    if mode != "dev":
        return 0

    wheel = latest_wheel()
    action = (
        "reinstalling existing dev wheel"
        if wheel.name in before
        else "installing dev wheel"
    )
    print(f"{action}: {wheel.name}", file=sys.stderr, flush=True)
    return install_wheel(wheel, env=env)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build running-process")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--dev",
        action="store_true",
        help="build a dev-profile wheel and reinstall it into the active uv environment",
    )
    mode.add_argument(
        "--quick",
        action="store_true",
        help="alias for --dev",
    )
    mode.add_argument(
        "--release",
        action="store_true",
        help="build release wheel(s) into dist/",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None, *, default_mode: BuildMode = "release") -> int:
    args = parse_args(argv)
    mode: BuildMode = default_mode
    if args.dev or args.quick:
        mode = "dev"
    if args.release:
        mode = "release"
    return run_build(mode)


if __name__ == "__main__":
    sys.exit(main())
