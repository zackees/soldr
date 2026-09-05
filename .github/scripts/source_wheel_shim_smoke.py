#!/usr/bin/env python3
"""Build soldr's source wheel with the installed multicall shims first on PATH.

This is the Windows PEP 517 boundary from soldr#1632.  The wheel under test is
installed into an isolated environment, its versioned shims are materialized,
and pip builds this checkout without a CARGO override.  Pip output is inherited
unchanged so environmental warnings remain visibly separate from the command's
exit status and the final wheel-tag assertion.
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import shutil
import subprocess
import sys
import sysconfig
import tempfile
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--wheel",
        nargs="+",
        required=True,
        help="Wheel path or glob. Exactly one wheel must match.",
    )
    parser.add_argument(
        "--source",
        type=Path,
        default=Path.cwd(),
        help="Source checkout passed to pip wheel.",
    )
    parser.add_argument(
        "--timeout",
        type=int,
        default=900,
        help="Timeout in seconds for the source-wheel build.",
    )
    parser.add_argument(
        "--require-free-threaded",
        action="store_true",
        help="Require a CPython build with Py_GIL_DISABLED=1.",
    )
    return parser.parse_args()


def resolve_wheel(patterns: list[str]) -> Path:
    matches: list[Path] = []
    for pattern in patterns:
        expanded = [Path(item) for item in glob.glob(pattern)]
        matches.extend(expanded or [Path(pattern)])
    wheels = [
        path.resolve() for path in matches if path.is_file() and path.suffix == ".whl"
    ]
    if len(wheels) != 1:
        raise SystemExit(
            f"expected exactly one wheel under test, found {len(wheels)}: {wheels}"
        )
    return wheels[0]


def venv_bin(venv: Path) -> Path:
    return venv / ("Scripts" if os.name == "nt" else "bin")


def run(
    args: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    timeout: int | None = None,
) -> None:
    print(f"+ {' '.join(args)}", flush=True)
    subprocess.run(args, cwd=cwd, env=env, timeout=timeout, check=True)


def shims_path(soldr: Path, env: dict[str, str]) -> Path:
    completed = subprocess.run(
        [str(soldr), "shims", "--json"],
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        timeout=60,
        check=True,
    )
    payload = json.loads(completed.stdout)
    if payload.get("schema_version") != 1:
        raise SystemExit(f"unexpected soldr shims schema: {payload}")
    path_entry = Path(payload["path_entry"]).resolve()
    print(f"soldr shims PATH entry: {path_entry}", flush=True)
    return path_entry


def same_path(left: Path, right: Path) -> bool:
    try:
        return left.samefile(right)
    except OSError:
        return os.path.normcase(str(left.resolve())) == os.path.normcase(
            str(right.resolve())
        )


def stop_soldr_daemon(soldr: Path, env: dict[str, str]) -> None:
    try:
        subprocess.run(
            [str(soldr), "daemon", "stop"],
            env=env,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        print(f"warning: best-effort `soldr daemon stop` failed: {exc}", flush=True)


def main() -> int:
    args = parse_args()
    if os.name != "nt":
        raise SystemExit("source-wheel shim smoke is a Windows-only acceptance gate")

    gil_disabled = sysconfig.get_config_var("Py_GIL_DISABLED")
    print(f"interpreter: {sys.executable}", flush=True)
    print(f"version: {sys.version}", flush=True)
    print(f"Py_GIL_DISABLED={gil_disabled!r}", flush=True)
    if args.require_free_threaded and gil_disabled != 1:
        raise SystemExit("expected a free-threaded CPython build (Py_GIL_DISABLED=1)")

    wheel = resolve_wheel(args.wheel)
    source = args.source.resolve()
    if not (source / "pyproject.toml").is_file():
        raise SystemExit(f"source checkout has no pyproject.toml: {source}")

    with tempfile.TemporaryDirectory(
        prefix="soldr-source-wheel-shims-",
        ignore_cleanup_errors=True,
    ) as tmp_str:
        tmp = Path(tmp_str)
        venv = tmp / "venv"
        wheelhouse = tmp / "wheelhouse"
        wheelhouse.mkdir()

        run([sys.executable, "-m", "venv", str(venv)])
        python = venv_bin(venv) / "python.exe"
        run([str(python), "-m", "pip", "install", "--upgrade", "pip"])
        run([str(python), "-m", "pip", "install", "--no-deps", str(wheel)])

        soldr = venv_bin(venv) / "soldr.exe"
        env = os.environ.copy()
        env["SOLDR_CACHE_DIR"] = str(tmp / "soldr-cache")
        path_entry = shims_path(soldr, env)
        cargo_shim = path_entry / "cargo.exe"
        if not cargo_shim.is_file():
            raise SystemExit(f"soldr shims did not materialize {cargo_shim}")

        env["PATH"] = os.pathsep.join(
            [str(path_entry), str(venv_bin(venv)), env.get("PATH", "")]
        )
        env.pop("CARGO", None)
        # soldr#3123: the wheel under test was just built with
        # `maturin build --profile ci-release` into <source>/target. The
        # backend otherwise pins a fresh per-project target dir keyed on
        # SOLDR_CACHE_DIR (which is a temp dir here) and defaults to the dev
        # profile, so this smoke re-compiled the whole graph (~5.5 min) to
        # test a PATH/shim contract that does not depend on the profile.
        # A caller-provided CARGO_TARGET_DIR always wins in src/soldr/__init__.py.
        env["CARGO_TARGET_DIR"] = str(source.resolve() / "target")
        env["SOLDR_PEP517_PROFILE"] = "ci-release"
        resolved_cargo = shutil.which("cargo", path=env["PATH"])
        if resolved_cargo is None or not same_path(Path(resolved_cargo), cargo_shim):
            raise SystemExit(
                f"cargo did not resolve to the installed soldr shim: "
                f"expected {cargo_shim}, got {resolved_cargo}"
            )
        print(f"cargo resolves to: {resolved_cargo}", flush=True)
        print("CARGO is unset", flush=True)

        try:
            run(
                [
                    str(python),
                    "-m",
                    "pip",
                    "wheel",
                    str(source),
                    "--no-deps",
                    # Exercise the wheel under test as the backend. Build
                    # isolation would install pyproject.toml's last-published
                    # bootstrap Soldr, whose rustc shim cannot share the newer
                    # wheel's package-version-gated broker during a release PR.
                    "--no-build-isolation",
                    "--no-cache-dir",
                    "--verbose",
                    "--wheel-dir",
                    str(wheelhouse),
                ],
                env=env,
                timeout=args.timeout,
            )
        finally:
            stop_soldr_daemon(soldr, env)

        built = sorted(wheelhouse.glob("soldr-*-py3-none-win_amd64.whl"))
        all_wheels = sorted(wheelhouse.glob("*.whl"))
        if len(built) != 1 or all_wheels != built:
            raise SystemExit(
                "expected exactly one soldr-*-py3-none-win_amd64.whl, "
                f"found {all_wheels}"
            )
        print(f"source-wheel shim smoke built {built[0].name}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
