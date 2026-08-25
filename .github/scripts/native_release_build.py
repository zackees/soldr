#!/usr/bin/env python3
"""Run release builds that must use a native host toolchain.

The pinned Soldr remains the Rust front door, but ``soldr rustup run ...
cargo`` deliberately avoids repeating blessed target preparation. This is
required for native macOS (where the blessed path adds its SDK directory as a bare
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
CONTRACT_PATH = (
    Path(__file__).resolve().parents[2] / "contracts" / "zccache-runtime.v1.json"
)
MATURIN_CONTRACT = json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))["maturin"]
SOLDR_MATURIN_NO_BINARY = str(MATURIN_CONTRACT["pypi_package"])
SOLDR_MATURIN_REQUIREMENT = (
    f"{SOLDR_MATURIN_NO_BINARY}=={MATURIN_CONTRACT['managed_version']}"
)


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


def matrix_driver() -> Path:
    """The just-built release driver, by runner OS."""
    suffix = ".exe" if os.environ.get("RUNNER_OS") == "Windows" else ""
    return Path("target") / "release" / f"soldr{suffix}"


def restore_version_manifests() -> None:
    """Undo any manifest edits a previous step left behind.

    Best-effort exactly as the inline `|| true` was: on a fresh checkout
    there is nothing to restore and git exits non-zero, which is not a
    failure of this step.
    """
    subprocess.run(
        [
            "git",
            "restore",
            "--",
            "Cargo.toml",
            "Cargo.lock",
            "crates/soldr-cli/Cargo.toml",
        ],
        check=False,
    )


def build_matrix_binary(driver: Path, target: str) -> None:
    """Build the release binary for a matrix target through `soldr build`.

    Distinct from `build_binary` on purpose. That one drives
    `soldr rustup run <toolchain> cargo build --locked` and serves the two
    native lanes (ARM64 musl, macOS) that must bypass target preparation.
    This one drives soldr's blessed `soldr build` surface, does *not* pass
    `--locked`, and runs `soldr prepare` for GNU Linux. Those differences are
    load-bearing for the six targets this lane covers, so the two paths are
    extracted separately rather than merged -- unifying them would silently
    change what every release binary is built with. Whether they *should*
    converge is a question for soldr#2469 Phase 3, with the candidate
    workflow available to prove it.

    The profile environment (`CARGO_PROFILE_RELEASE_*`, job bounds) stays in
    the workflow: those values are matrix expressions -- MSVC takes
    `lto=false`/`codegen-units=16` and everything else `thin`/`1` -- and
    moving them here would mean reimplementing the matrix in Python.
    """
    restore_version_manifests()

    clean = [str(driver), "cargo", "clean", "-p", "soldr-cli", "--target", target]
    clean += ["--release"]
    print(f"release helper: $ {shlex.join(clean)}", flush=True)
    subprocess.run(
        clean, check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )

    # `--no-cache` avoids the resident embedded daemon behind the old
    # Linux-hosted cross-build memory collision.
    run(
        [
            str(driver),
            "--no-cache",
            "build",
            "--release",
            "--target",
            target,
            "--package",
            "soldr-cli",
            "--bin",
            "soldr",
        ]
    )

    if target.endswith("-unknown-linux-gnu"):
        github_env = os.environ.get("GITHUB_ENV")
        if github_env:
            run(
                [str(driver), "prepare", "--target", target, "--github-env", github_env]
            )

    output = Path("target") / target / "release"
    print(f"=== post-build diagnostic: {output}/ ===", flush=True)
    if output.is_dir():
        for entry in sorted(output.iterdir())[:20]:
            print(f"  {entry.name}", flush=True)
    else:
        print(f"  (missing: {output})", flush=True)
    print("===================================================", flush=True)


def wheel_environment(
    base: Mapping[str, str], *, driver: Path, cargo_bridge: Path
) -> dict[str, str]:
    env = dict(base)
    env.pop("RUSTC_WRAPPER", None)
    env["SOLDR_RELEASE_DRIVER"] = str(driver)
    env["SOLDR_RELEASE_TOOLCHAIN"] = TOOLCHAIN
    env["CARGO"] = str(cargo_bridge)
    return env


def resolve_toolchain_rustc(driver: Path, base: Mapping[str, str]) -> Path:
    """Resolve the pinned real rustc for host build-backend probes."""

    completed = subprocess.run(
        [str(driver), "rustup", "which", "rustc", "--toolchain", TOOLCHAIN],
        check=True,
        env=dict(base),
        stdout=subprocess.PIPE,
        text=True,
    )
    rustc = Path(completed.stdout.strip())
    if not rustc.is_file():
        raise RuntimeError(f"Soldr resolved a missing rustc: {rustc}")
    return rustc


def host_tool_environment(
    base: Mapping[str, str], *, driver: Path, cargo_bridge: Path, rustc: Path
) -> dict[str, str]:
    """Remove wheel-target state while source-building the host Maturin tool."""

    env = wheel_environment(base, driver=driver, cargo_bridge=cargo_bridge)
    exact = {
        "AR",
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "CC",
        "CFLAGS",
        "CXX",
        "CXXFLAGS",
        "LDFLAGS",
        "RANLIB",
        "RUSTC",
        "RUSTFLAGS",
    }
    for key in list(env):
        if key in exact or key.startswith(("AR_", "CC_", "CXX_", "RANLIB_")):
            env.pop(key, None)
        elif key.startswith("CARGO_TARGET_"):
            env.pop(key, None)
    rustc = rustc.resolve()
    env["PATH"] = f"{rustc.parent}{os.pathsep}{env.get('PATH', '')}"
    env["RUSTC"] = str(rustc)
    env["RUSTUP_TOOLCHAIN"] = TOOLCHAIN
    env["CARGO_BUILD_JOBS"] = "2"
    env["SOLDR_JOBS"] = "2"
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
            f"soldr-cli version {actual_version!r} does not match release {expected_version!r}"
        )

    dist = Path("dist")
    dist.mkdir(exist_ok=True)
    for wheel in dist.glob("*.whl"):
        wheel.unlink()

    venv = Path(".venv-release-wheel")
    run(["uv", "python", "install", "3.13"])
    run(["uv", "venv", "--python", "3.13", "--clear", str(venv)])
    python = venv / "bin" / "python"

    cargo_bridge = Path(".github/scripts/cargo_via_soldr_rustup.sh").resolve()
    cargo_bridge.chmod(cargo_bridge.stat().st_mode | stat.S_IXUSR)
    rustc = resolve_toolchain_rustc(driver, os.environ)
    host_env = host_tool_environment(
        os.environ,
        driver=driver,
        cargo_bridge=cargo_bridge,
        rustc=rustc,
    )
    run(
        [
            "uv",
            "pip",
            "install",
            "--python",
            str(python),
            "--no-cache",
            "--no-binary",
            SOLDR_MATURIN_NO_BINARY,
            SOLDR_MATURIN_REQUIREMENT,
            "patchelf; platform_system == 'Linux'",
        ],
        env=host_env,
    )
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

    matrix_binary = subparsers.add_parser("matrix-binary")
    matrix_binary.add_argument("--target", required=True)

    wheel = subparsers.add_parser("musl-wheel")
    wheel.add_argument("--driver", type=Path, required=True)
    wheel.add_argument("--target", choices=MUSL_TARGETS, required=True)
    wheel.add_argument("--expected-version", required=True)

    args = parser.parse_args(argv)
    # `matrix-binary` derives its driver from RUNNER_OS rather than taking one:
    # the workflow previously spelled that `case "$RUNNER_OS" in Windows) ...`
    # at every call site, which is exactly the kind of duplication this
    # extraction exists to remove.
    driver = (
        matrix_driver() if args.command == "matrix-binary" else args.driver
    ).resolve()
    if not driver.is_file():
        parser.error(f"pinned Soldr driver does not exist: {driver}")
    if args.command == "matrix-binary":
        build_matrix_binary(driver, args.target)
    elif args.command == "binary":
        build_binary(driver, args.target)
    else:
        build_musl_wheel(driver, args.target, args.expected_version)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
