"""One entry point for the CI stages, runnable identically here and on a runner.

``uv run --no-sync -m ci <stage> [args...]``

The value is local-vs-CI symmetry (#516): today a contributor reproduces a
failing job by reading the workflow YAML and reassembling the command by hand.
Every stage below is the *same* module a workflow invokes, so running it here
runs what CI runs.

Deliberately a dispatcher and nothing more. It does not sequence stages, own a
warm-cache policy, or replace ``./lint`` and ``./test`` -- those wrap this same
code and stay the shortest path for the common case. The workflow topology is
untouched, so this does not settle the open #513-vs-#516 question about how CI
should be shaped; it is the part of #516 that has standalone value either way.

Stages map to modules that already exist. Nothing here invents a stage: if a
name is listed, ``python -m ci <name>`` runs the module a workflow would.
"""

from __future__ import annotations

import importlib
import inspect
import sys

# Stage name -> module under `ci`. Kept explicit rather than discovered by
# scanning, so adding a stage is a deliberate act and a typo is a test failure
# rather than a silently missing subcommand.
STAGES: dict[str, str] = {
    "build": "ci.build_wheel",
    "lint": "ci.lint",
    "test": "ci.test",
    "version-check": "ci.version_check",
    "reproducible": "ci.reproducible",
    "servicedef-proof": "ci.servicedef_proof",
    "terminal-capability": "ci.terminal_capability_report",
    "verify-release-symbols": "ci.verify_release_symbols",
    "rust-debug-annotations": "ci.check_rust_debug_annotations",
    # The repo's static guards. Individually cheap and individually useful --
    # `./lint` runs them together, but a contributor chasing one failure wants
    # to run exactly that one.
    "guard-cross-compiler": "ci.cross_compiler_guard",
    "guard-jemalloc": "ci.jemalloc_guard",
    "guard-spawn-path": "ci.spawn_path_guard",
    "guard-docker-manifest": "ci.docker_manifest_guard",
}


def _usage() -> str:
    width = max(len(name) for name in STAGES)
    lines = [f"  {name.ljust(width)}  {module}" for name, module in sorted(STAGES.items())]
    return "usage: python -m ci <stage> [args...]\n\nstages:\n" + "\n".join(lines)


def run_stage(stage: str, argv: list[str]) -> int:
    """Import `stage`'s module and call its `main`, returning its exit code."""
    module = importlib.import_module(STAGES[stage])
    main = module.main

    # Some stage mains take an argv list, others take none. Passing arguments
    # to one that cannot accept them would silently drop them -- which reads
    # as "the flag had no effect" rather than "the flag was never delivered".
    takes_argv = bool(inspect.signature(main).parameters)
    if argv and not takes_argv:
        print(
            f"ci: stage {stage!r} ({STAGES[stage]}) takes no arguments, got {argv}",
            file=sys.stderr,
        )
        return 2
    result = main(argv) if takes_argv else main()
    # A main that returns None succeeded; anything else is its own exit code.
    return 0 if result is None else int(result)


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if not args or args[0] in {"-h", "--help"}:
        print(_usage())
        return 0 if args else 2

    stage = args[0]
    if stage not in STAGES:
        print(f"ci: unknown stage {stage!r}\n\n{_usage()}", file=sys.stderr)
        return 2
    return run_stage(stage, args[1:])


if __name__ == "__main__":
    sys.exit(main())
