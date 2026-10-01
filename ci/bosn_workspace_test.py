#!/usr/bin/env python3
"""Run Bosn's workspace tests after handing the cache route to source Soldr.

The published Soldr bootstrap exists only to compile the checkout.  Source
integration tests execute ``target/debug/soldr`` and must not inherit the
bootstrap daemon route: their wire protocol may legitimately differ.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Step:
    argv: list[str]
    env: dict[str, str]
    # Variables removed from the inherited environment (the image's
    # CARGO_BUILD_JOBS cap, which CI's ci-test lane also unsets).
    unset: tuple[str, ...] = ()
    # Prefixes removed likewise: the image's dev-loop CARGO_PROFILE_*
    # overrides (release at opt-level 0, incremental) make a build differ
    # from CI's -- a Dylint library built that way lost its `dylint_version`
    # export (zackees/ci.yml#172).
    unset_prefixes: tuple[str, ...] = ()


def container_path(path: Path) -> str:
    """Render a Linux container path even when the contract test runs on Windows."""
    return path.as_posix()


def workspace_test_plan(*, target: Path, bootstrap: Path) -> list[Step]:
    """Return the ordered bootstrap-to-source validation handoff."""
    source = target / "debug" / "soldr"
    source_text = container_path(source)
    base_env = {"CARGO_TARGET_DIR": container_path(target)}
    return [
        Step(
            [
                container_path(bootstrap),
                "cargo",
                "build",
                "-p",
                "soldr-cli",
                "--bin",
                "soldr",
                "-p",
                "soldr-nextest-wrapper",
                "--bin",
                "soldr-nextest-wrapper",
            ],
            base_env,
        ),
        Step(
            [
                container_path(bootstrap),
                "cache",
                "shutdown",
                "--shutdown-timeout-seconds",
                "30",
            ],
            {},
        ),
        Step([container_path(bootstrap), "broker", "remove"], {}),
        Step([source_text, "daemon", "start"], {}),
        # zackees/ci.yml#168/#172: exactly the two *test* stages of the frozen
        # DAG CI's build-linux-x64 lane runs (`soldr ci-test --explain-plan`:
        # `nextest` and `doctests`). Its lint stages -- rustfmt, Clippy,
        # Dylint, dependency policy -- are host-safe and run on the host in
        # the local gate's `rust` lane (`soldr lint rust`, `soldr lint deps`);
        # only the tests start soldr daemons, so only they need isolation.
        # Doctests first: nextest's integration tests relink target/debug/soldr
        # (the bin under the test feature set) in place, after which a later
        # compile runs through a different soldr image than the daemon
        # started above, which refuses it ("root ownership is busy ... a
        # different Soldr version or daemon image").
        # nextest, not a bare `cargo test --workspace`: libtest's shared
        # process let one panicking test poison a crate-wide env lock and fail
        # six unrelated tests, which nextest's process-per-test model cannot.
        Step(
            [
                "sh",
                "-c",
                f'"{source_text}" cargo test --workspace --doc'
                f' && "{source_text}" cargo nextest run --no-fail-fast --workspace --lib --tests',
            ],
            base_env,
            unset=("CARGO_BUILD_JOBS", "SOLDR_JOBS"),
            unset_prefixes=("CARGO_PROFILE_",),
        ),
        Step(
            [source_text, "cache", "shutdown", "--shutdown-timeout-seconds", "30"],
            {},
        ),
        Step([source_text, "broker", "remove"], {}),
    ]


def run_step(step: Step, *, repo: Path) -> None:
    env = os.environ.copy()
    for name in step.unset:
        env.pop(name, None)
    for name in [n for n in env if n.startswith(step.unset_prefixes)]:
        env.pop(name)
    env.update(step.env)
    subprocess.run(step.argv, cwd=repo, env=env, check=True)


def cleanup(steps: list[Step], *, repo: Path, preserve_primary: bool) -> None:
    """Run all source-route cleanup steps without hiding a test failure."""
    failures: list[subprocess.CalledProcessError | OSError] = []
    for step in steps:
        try:
            run_step(step, repo=repo)
        except (subprocess.CalledProcessError, OSError) as error:
            failures.append(error)
            print(f"cleanup failed: {' '.join(step.argv)}", file=sys.stderr)
    if failures and not preserve_primary:
        raise failures[0]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path("/repo"))
    parser.add_argument("--target", type=Path, default=Path("/target"))
    parser.add_argument(
        "--bootstrap", type=Path, default=Path("/opt/soldr-bootstrap/bin/soldr")
    )
    args = parser.parse_args(argv)

    plan = workspace_test_plan(target=args.target, bootstrap=args.bootstrap)
    setup, source_start, validation, teardown = plan[:3], plan[3], plan[4], plan[5:]
    for step in setup:
        run_step(step, repo=args.repo.resolve())
    try:
        run_step(source_start, repo=args.repo.resolve())
        run_step(validation, repo=args.repo.resolve())
    except BaseException:
        cleanup(teardown, repo=args.repo.resolve(), preserve_primary=True)
        raise
    cleanup(teardown, repo=args.repo.resolve(), preserve_primary=False)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
