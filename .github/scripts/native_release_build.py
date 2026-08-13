#!/usr/bin/env python3
"""Run release builds that must use a native host toolchain.

The pinned Soldr remains the Rust front door, but ``soldr rustup run ...
cargo`` deliberately avoids repeating blessed target preparation. This is
required for native macOS (where 0.8.44 adds its SDK directory as a bare
linker input) and native ARM64 musl (whose catalogue compiler is i386-hosted).
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import stat
import subprocess
from collections.abc import Mapping, Sequence
from pathlib import Path

TOOLCHAIN = "1.95.0"
ARM64_MUSL = "aarch64-unknown-linux-musl"
MUSL_TARGETS = ("x86_64-unknown-linux-musl", ARM64_MUSL)


def cargo_command(driver: Path, *args: str) -> list[str]:
    """Return a Cargo command routed through pinned Soldr and rustup."""

    return [str(driver), "rustup", "run", TOOLCHAIN, "cargo", *args]


def run(command: Sequence[str], *, env: Mapping[str, str] | None = None) -> None:
    print(f"release helper: $ {shlex.join(command)}", flush=True)
    subprocess.run(command, check=True, env=None if env is None else dict(env))


def soldr_cli_version(metadata: str) -> str:
    document = json.loads(metadata)
    versions = [
        package["version"]
        for package in document.get("packages", [])
        if package.get("name") == "soldr-cli"
    ]
    if len(versions) != 1:
        raise RuntimeError(f"expected one soldr-cli package, found {len(versions)}")
    return str(versions[0])


def release_build_environment(base: Mapping[str, str]) -> dict[str, str]:
    env = dict(base)
    env.pop("RUSTC_WRAPPER", None)
    env.update(
        {
            "SOLDR_RELEASE_CI": "1",
            "CARGO_PROFILE_RELEASE_DEBUG": "0",
            "CARGO_PROFILE_RELEASE_LTO": "thin",
            "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "1",
            "CARGO_BUILD_JOBS": "2",
            "SOLDR_JOBS": "2",
        }
    )
    return env


def build_binary(driver: Path, target: str) -> None:
    env = release_build_environment(os.environ)
    clean = cargo_command(
        driver, "clean", "-p", "soldr-cli", "--target", target, "--release"
    )
    print(f"release helper: $ {shlex.join(clean)}", flush=True)
    subprocess.run(
        clean,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
    )
    run(
        cargo_command(
            driver,
            "build",
            "--release",
            "--locked",
            "--target",
            target,
            "--package",
            "soldr-cli",
            "--bin",
            "soldr",
        ),
        env=env,
    )
    output = Path("target") / target / "release"
    if not (output / "soldr").is_file():
        raise RuntimeError(f"release binary was not produced under {output}")
    print(f"release helper: produced {output / 'soldr'}", flush=True)


def wheel_environment(
    base: Mapping[str, str], *, driver: Path, cargo_bridge: Path
) -> dict[str, str]:
    env = dict(base)
    env.pop("RUSTC_WRAPPER", None)
    env["SOLDR_RELEASE_DRIVER"] = str(driver)
    env["SOLDR_RELEASE_TOOLCHAIN"] = TOOLCHAIN
    env["CARGO"] = str(cargo_bridge)
    return env


def build_musl_wheel(driver: Path, target: str, expected_version: str) -> None:
    if target not in MUSL_TARGETS:
        raise ValueError(f"not a release musl target: {target}")
    metadata = subprocess.run(
        cargo_command(
            driver,
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
            "crates/soldr-cli/Cargo.toml",
        ),
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout
    actual_version = soldr_cli_version(metadata)
    if actual_version != expected_version:
        raise RuntimeError(
            f"soldr-cli version {actual_version!r} does not match release "
            f"{expected_version!r}"
        )

    dist = Path("dist")
    dist.mkdir(exist_ok=True)
    for wheel in dist.glob("*.whl"):
        wheel.unlink()

    venv = Path(".venv-release-wheel")
    run(["uv", "python", "install", "3.13"])
    run(["uv", "venv", "--python", "3.13", str(venv)])
    python = venv / "bin" / "python"
    run(["uv", "pip", "install", "--python", str(python), "maturin>=1.7,<2"])

    cargo_bridge = Path(".github/scripts/cargo_via_soldr_rustup.sh").resolve()
    cargo_bridge.chmod(cargo_bridge.stat().st_mode | stat.S_IXUSR)
    env = wheel_environment(os.environ, driver=driver, cargo_bridge=cargo_bridge)
    run(
        [
            str(venv / "bin" / "maturin"),
            "build",
            "--release",
            "--locked",
            "--target",
            target,
            "--target-dir",
            "target",
            "--out",
            "dist",
            "--compatibility",
            "musllinux_1_2",
        ],
        env=env,
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    binary = subparsers.add_parser("binary")
    binary.add_argument("--driver", type=Path, required=True)
    binary.add_argument("--target", required=True)

    wheel = subparsers.add_parser("musl-wheel")
    wheel.add_argument("--driver", type=Path, required=True)
    wheel.add_argument("--target", choices=MUSL_TARGETS, required=True)
    wheel.add_argument("--expected-version", required=True)

    args = parser.parse_args(argv)
    driver = args.driver.resolve()
    if not driver.is_file():
        parser.error(f"pinned Soldr driver does not exist: {driver}")
    if args.command == "binary":
        build_binary(driver, args.target)
    else:
        build_musl_wheel(driver, args.target, args.expected_version)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
