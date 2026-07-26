#!/usr/bin/env python3
"""Measure cold vs warm `soldr cargo dylint` wall time (soldr#1788 Phase 1).

Drives `bench/dylint_fixture/` — a small, self-contained cargo workspace
with a real custom Dylint 6.0.1 lint (`lints/ban_forbidden_fn`) — through
cold and warm `soldr cargo dylint --all --workspace` runs and reports wall
time for each scenario.

Usage::

    # Docker mode (default): drives the soldr-perf-local container the
    # same way ci/perf_local.py does (named volumes for CARGO_HOME/soldr
    # home, /repo bind mount).
    uv run --no-project python bench/dylint_perf.py

    # Host mode: run `soldr cargo dylint` directly on this machine.
    uv run --no-project python bench/dylint_perf.py --host

    # Also run the warm-clean-target scenario.
    uv run --no-project python bench/dylint_perf.py --warm-clean-target

    # Prove the lint still fires (swaps in violation.rs.disabled, asserts
    # failure + diagnostic text, restores main.rs, always).
    uv run --no-project python bench/dylint_perf.py --expect-fail

Scenarios (wall-clock via time.monotonic):

    cold               - wipe the fixture's target/, the dylint driver
                         cache, and the soldr state root, then run the
                         lint command.
    warm               - immediate rerun, nothing cleared.
    warm-clean-target  - (opt-in via --warm-clean-target) wipe only the
                         fixture's target/, keep toolchain/driver/soldr
                         state warm.

Exit code is non-zero if the lint command itself fails on a non-expect-fail
scenario, or if --expect-fail does not observe the expected failure.
"""

from __future__ import annotations

import argparse
import importlib.util
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path
from types import ModuleType

REPO_ROOT = Path(__file__).resolve().parent.parent
FIXTURE_DIR = Path(__file__).resolve().parent / "dylint_fixture"
APP_MAIN = FIXTURE_DIR / "app" / "src" / "main.rs"
VIOLATION_FILE = FIXTURE_DIR / "app" / "src" / "violation.rs.disabled"

DYLINT_CMD = ["soldr", "cargo", "dylint", "--all", "--workspace"]

# What `cold` clears, and where. Docker defaults match the paths the
# soldr-perf-local container actually uses (root home, named /root/.soldr
# volume); host defaults are the ordinary user-home locations. Override via
# --soldr-home / --dylint-driver-dir if your setup differs.
DOCKER_DEFAULT_SOLDR_HOME = "/root/.soldr"
DOCKER_DEFAULT_DYLINT_DRIVER_DIR = "/root/.dylint_drivers"
HOST_DEFAULT_SOLDR_HOME = "~/.soldr"
HOST_DEFAULT_DYLINT_DRIVER_DIR = "~/.dylint_drivers"

DIAGNOSTIC_MARKERS = ("forbidden_marker_fn", "ban_forbidden_fn", "BAN_FORBIDDEN_FN")


def load_perf_local() -> ModuleType:
    """Import ci/perf_local.py by path — it's a script, not a package."""
    spec = importlib.util.spec_from_file_location(
        "perf_local", REPO_ROOT / "ci" / "perf_local.py"
    )
    if spec is None or spec.loader is None:
        raise RuntimeError("unable to load ci/perf_local.py")
    module = importlib.util.module_from_spec(spec)
    # Register before exec: @dataclass resolves annotations through
    # sys.modules[cls.__module__], which is None for an unregistered module.
    # perf_local grew a frozen `Runner` dataclass in #1835, so an
    # unregistered import now dies with "'NoneType' object has no attribute
    # '__dict__'". tests/test_perf_local.py carries the same two lines.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class HostRunner:
    """Runs the lint command directly on the host."""

    def __init__(self) -> None:
        self.target_dir = FIXTURE_DIR / "target"

    def display_paths(self, soldr_home: str, driver_dir: str) -> tuple[str, str, str]:
        return (str(self.target_dir), soldr_home, driver_dir)

    def run(self, argv: list[str]) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = str(self.target_dir)
        return subprocess.run(
            argv,
            cwd=FIXTURE_DIR,
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )

    @staticmethod
    def _rm_rf(path: Path) -> None:
        if path.exists():
            shutil.rmtree(path, ignore_errors=True)

    def clear_cold_state(self, soldr_home: str, driver_dir: str) -> None:
        self._rm_rf(self.target_dir)
        self._rm_rf(Path(os.path.expanduser(soldr_home)))
        self._rm_rf(Path(os.path.expanduser(driver_dir)))

    def clear_target_only(self) -> None:
        self._rm_rf(self.target_dir)


class DockerRunner:
    """Runs the lint command inside the soldr-perf-local container."""

    def __init__(self, perf_local: ModuleType, source_root: Path) -> None:
        self.perf_local = perf_local
        self.source_root = source_root
        # Runners are per-checkout-root since #1835, so the container name has
        # to be derived rather than read off a module-level constant.
        self.container = perf_local.runner_for(source_root).container
        self.container_fixture_dir = perf_local.container_workdir(source_root, FIXTURE_DIR)
        self.target_dir = f"{self.container_fixture_dir}/target"

    def display_paths(self, soldr_home: str, driver_dir: str) -> tuple[str, str, str]:
        return (self.target_dir, soldr_home, driver_dir)

    def _exec(
        self, argv: list[str], extra_env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        command = ["docker", "exec"]
        for key, value in (extra_env or {}).items():
            command.extend(["-e", f"{key}={value}"])
        command.extend(["-w", self.container_fixture_dir, self.container, *argv])
        return subprocess.run(command, capture_output=True, text=True, check=False)

    def run(self, argv: list[str]) -> subprocess.CompletedProcess[str]:
        return self._exec(argv, extra_env={"CARGO_TARGET_DIR": self.target_dir})

    def _rm_rf(self, container_path: str) -> None:
        self._exec(["rm", "-rf", container_path])

    def detect_soldr_home(self) -> str | None:
        """Ask the in-container soldr for its actual state root.

        Official builds use ~/.soldr, development builds ~/.soldr-dev, so
        guessing is wrong half the time — and passing an explicit
        `--soldr-home /root/...` from Git Bash on Windows gets mangled by
        MSYS path conversion (`C:/Program Files/Git/root/...`). Asking the
        binary sidesteps both problems.
        """
        result = self._exec(["soldr", "status"])
        if result.returncode != 0:
            return None
        for line in result.stdout.splitlines():
            if line.startswith("root dir:"):
                root = line.split(":", 1)[1].strip()
                if root.startswith("/"):
                    return root
        return None

    def clear_cold_state(self, soldr_home: str, driver_dir: str) -> None:
        self._rm_rf(self.target_dir)
        self._rm_rf(soldr_home)
        self._rm_rf(driver_dir)

    def clear_target_only(self) -> None:
        self._rm_rf(self.target_dir)


def _run_and_time(runner, label: str) -> tuple[float, subprocess.CompletedProcess[str]]:
    print(f"[{label}] running: {' '.join(DYLINT_CMD)}", file=sys.stderr)
    start = time.monotonic()
    result = runner.run(DYLINT_CMD)
    elapsed = time.monotonic() - start
    return elapsed, result


def _fail(label: str, result: subprocess.CompletedProcess[str]) -> int:
    print(f"error: [{label}] soldr cargo dylint failed (exit {result.returncode})", file=sys.stderr)
    print(result.stdout, file=sys.stderr)
    print(result.stderr, file=sys.stderr)
    return 1


def run_bench(runner, soldr_home: str, driver_dir: str, include_warm_clean_target: bool) -> int:
    target_display, soldr_home_display, driver_dir_display = runner.display_paths(
        soldr_home, driver_dir
    )
    scenarios: list[tuple[str, float]] = []

    print(
        f"[cold] clearing: {target_display}, {soldr_home_display}, {driver_dir_display}",
        file=sys.stderr,
    )
    runner.clear_cold_state(soldr_home, driver_dir)
    elapsed, result = _run_and_time(runner, "cold")
    if result.returncode != 0:
        return _fail("cold", result)
    scenarios.append(("cold", elapsed))

    elapsed, result = _run_and_time(runner, "warm")
    if result.returncode != 0:
        return _fail("warm", result)
    scenarios.append(("warm", elapsed))

    if include_warm_clean_target:
        print(f"[warm-clean-target] clearing: {target_display}", file=sys.stderr)
        runner.clear_target_only()
        elapsed, result = _run_and_time(runner, "warm-clean-target")
        if result.returncode != 0:
            return _fail("warm-clean-target", result)
        scenarios.append(("warm-clean-target", elapsed))

    print_table(scenarios)
    return 0


def print_table(scenarios: list[tuple[str, float]]) -> None:
    label_width = max(len(name) for name, _ in scenarios)
    print()
    print(f"{'scenario':<{label_width}}  wall seconds")
    for name, elapsed in scenarios:
        print(f"{name:<{label_width}}  {elapsed:>12.2f}")

    by_name = dict(scenarios)
    cold = by_name.get("cold")
    warm = by_name.get("warm")
    if cold is not None and warm is not None and warm > 0:
        print(f"\ncold/warm ratio: {cold / warm:.2f}x")


def run_expect_fail(runner) -> int:
    original = APP_MAIN.read_text()
    violation = VIOLATION_FILE.read_text()
    try:
        APP_MAIN.write_text(violation)
        elapsed, result = _run_and_time(runner, "expect-fail")
        combined = f"{result.stdout}\n{result.stderr}"
        if result.returncode == 0:
            print(
                "error: expected soldr cargo dylint to fail with the "
                "ban_forbidden_fn diagnostic, but it exited 0",
                file=sys.stderr,
            )
            print(combined, file=sys.stderr)
            return 1
        if not any(marker in combined for marker in DIAGNOSTIC_MARKERS):
            print(
                "error: soldr cargo dylint failed, but none of the expected "
                f"diagnostic markers {DIAGNOSTIC_MARKERS!r} appeared in its "
                "output — it may have failed for an unrelated reason",
                file=sys.stderr,
            )
            print(combined, file=sys.stderr)
            return 1
        print(f"expect-fail OK ({elapsed:.2f}s): ban_forbidden_fn fired as expected")
        print(combined)
        return 0
    finally:
        APP_MAIN.write_text(original)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument(
        "--docker",
        action="store_true",
        help="run inside the soldr-perf-local Docker container (default)",
    )
    mode.add_argument(
        "--host", action="store_true", help="run directly on the host, bypassing Docker"
    )
    parser.add_argument(
        "--expect-fail",
        action="store_true",
        help="swap in violation.rs.disabled, assert the lint fires, restore main.rs, then exit",
    )
    parser.add_argument(
        "--warm-clean-target",
        action="store_true",
        help="also run the warm-clean-target scenario (wipe only fixture target/)",
    )
    parser.add_argument(
        "--soldr-home",
        default=None,
        help="soldr state root to clear on `cold` "
        f"(default: {DOCKER_DEFAULT_SOLDR_HOME} in Docker, {HOST_DEFAULT_SOLDR_HOME} on host)",
    )
    parser.add_argument(
        "--dylint-driver-dir",
        default=None,
        help="dylint driver cache to clear on `cold` "
        f"(default: {DOCKER_DEFAULT_DYLINT_DRIVER_DIR} in Docker, "
        f"{HOST_DEFAULT_DYLINT_DRIVER_DIR} on host)",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    use_docker = not args.host

    if use_docker:
        if not shutil.which("docker"):
            print("error: docker not on PATH (use --host to skip Docker)", file=sys.stderr)
            return 2
        perf_local = load_perf_local()
        source_root = perf_local.shared_source_root(REPO_ROOT)
        soldr_home = args.soldr_home or DOCKER_DEFAULT_SOLDR_HOME
        driver_dir = args.dylint_driver_dir or DOCKER_DEFAULT_DYLINT_DRIVER_DIR
        with perf_local.runner_lock(source_root):
            try:
                image_id = perf_local.ensure_image(REPO_ROOT)
            except RuntimeError as error:
                print(f"error: {error}", file=sys.stderr)
                return 1
            perf_local.ensure_runner(source_root, image_id)
            runner = DockerRunner(perf_local, source_root)
            if args.soldr_home is None:
                detected = runner.detect_soldr_home()
                if detected:
                    soldr_home = detected
                    print(
                        f"[setup] detected in-container soldr root: {soldr_home}", file=sys.stderr
                    )
            if args.expect_fail:
                return run_expect_fail(runner)
            return run_bench(runner, soldr_home, driver_dir, args.warm_clean_target)

    runner = HostRunner()
    soldr_home = args.soldr_home or HOST_DEFAULT_SOLDR_HOME
    driver_dir = args.dylint_driver_dir or HOST_DEFAULT_DYLINT_DRIVER_DIR
    if args.expect_fail:
        return run_expect_fail(runner)
    return run_bench(runner, soldr_home, driver_dir, args.warm_clean_target)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
