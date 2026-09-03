#!/usr/bin/env python3
"""Per-PR macOS x86_64 execution lane, hosted in a Recovery guest (soldr#3076).

`e2e-macos-x64` in `ci.yml` cross-builds `x86_64-apple-darwin` on Linux (via
`_ci-cross-build-linux.yml`) and used to replay the packaged nextest archive
inside a hand-baked dockur/macos guest over ssh (soldr#3071). That guest image
was never published and the ssh secret it needed was never set, so the lane
failed at preflight on every run.

`zackees/docker-mac-x64` (https://github.com/zackees/docker-mac-x64) replaces
it: a real x86_64 macOS **Recovery** guest, no baked image, no secret, no
ssh. The tradeoff is the execution model. Recovery boots fresh per action
invocation, runs exactly one script (typed into a GUI Terminal and fetched
back over HTTP), and `/tmp` is a ramdisk that nothing survives past that one
boot. There is no toolchain to provision, no per-command `exec`, and no room
for the ~3.3 GiB decompressed nextest archive `_ci-target-run.yml`'s general
replay uses for every other target.

So this lane does NOT run the general nextest archive replay. It ships in
only the packaged `soldr` binary and runs a small, fixed set of
host-sensitive CLI smoke checks directly against it -- the checks that need
nothing but the binary itself (no daemon, no fixtures, no toolchain). This
mirrors `crates/soldr-cli/tests/guards/cli_startup_smoke.rs`'s
`version_flag_starts_and_prints` / `help_flag_starts_and_prints` in spirit
(same binary, same assertions) without actually replaying the nextest
archive that contains them -- that replay was time-boxed out; see the PR
description this module shipped with for what was tried and why.

Two-phase design, same shape as `ci/smoke_release_artifacts.py`'s Recovery
support and for the same reason (Recovery has no Python to run a verifier
inside):

    emit-guest-script --output PATH
        Write the bash-3.2-compatible script the guest runs. Pure function
        of nothing but the module constants, so it needs no arguments beyond
        where to write it.

    verify-collected --collected DIR --guest-exit-code CODE
        Read the guest's collected results (the action's `collect` tarball,
        already extracted by the workflow) plus its `exit-code` output, and
        fail with a named diagnostic per check.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

GUEST_HTTP_BASE = "http://10.0.2.2:8000"
RESULTS_FILE = "summary.txt"

# soldr#3076: every check this lane runs, and what it proves. All three need
# nothing but the packaged binary -- no daemon, no fixtures, no toolchain --
# which is exactly the subset Recovery's one-shot, no-toolchain guest can
# still exercise.
CHECKS = ("arch", "fetch_soldr", "version", "help")


def build_guest_script() -> str:
    """The bash-3.2 script the Recovery guest runs.

    Never raises on a failed check: every check runs and is recorded in
    `/tmp/results/summary.txt` as a `name=pass[:detail]` /
    `name=fail[:detail]` line, and the script's own exit code (0 only if
    every check passed) is the second, coarser signal `verify-collected`
    checks against the action's `exit-code` output -- the same
    belt-and-suspenders `smoke_release_artifacts.build_release_guest_script`
    uses.
    """
    return "\n".join(
        [
            "#!/bin/sh",
            "set +e",
            "mkdir -p /tmp/results",
            f"SUMMARY=/tmp/results/{RESULTS_FILE}",
            ': > "$SUMMARY"',
            "FAIL=0",
            "",
            "ARCH=$(uname -m)",
            'if [ "$ARCH" = "x86_64" ]; then',
            '  echo "arch=pass:$ARCH" >> "$SUMMARY"',
            "else",
            '  echo "arch=fail:unexpected uname -m $ARCH" >> "$SUMMARY"',
            "  FAIL=1",
            "fi",
            "",
            f"curl -fsS -o /tmp/soldr {GUEST_HTTP_BASE}/soldr",
            "if [ $? -ne 0 ]; then",
            f'  echo "fetch_soldr=fail:curl could not reach {GUEST_HTTP_BASE}/soldr" >> "$SUMMARY"',
            "  FAIL=1",
            "else",
            "  chmod +x /tmp/soldr",
            '  echo "fetch_soldr=pass" >> "$SUMMARY"',
            "fi",
            "",
            "VOUT=$(/tmp/soldr --version 2>&1)",
            'case "$VOUT" in',
            '  "soldr "*)',
            '    echo "version=pass:$VOUT" >> "$SUMMARY" ;;',
            "  *)",
            '    echo "version=fail:$VOUT" >> "$SUMMARY"',
            "    FAIL=1 ;;",
            "esac",
            "",
            "/tmp/soldr --help >/tmp/help.out 2>&1",
            "HRC=$?",
            'if [ "$HRC" -eq 0 ]; then',
            '  echo "help=pass" >> "$SUMMARY"',
            "else",
            '  echo "help=fail:exit $HRC" >> "$SUMMARY"',
            "  FAIL=1",
            "fi",
            "",
            'exit "$FAIL"',
            "",
        ]
    )


def append_github_output_multiline(path: Path, name: str, value: str) -> None:
    """Append a multi-line `name<<EOF / value / EOF` block to `$GITHUB_OUTPUT`.

    Doing this in Python instead of a separate bash heredoc step keeps the
    workflow's inline `run:` footprint down and makes the delimiter handling
    testable instead of hand-typed YAML.
    """
    delimiter = f"GITHUB_OUTPUT_{name.upper()}_EOF"
    with path.open("a", encoding="utf-8") as handle:
        handle.write(f"{name}<<{delimiter}\n{value}{delimiter}\n")


def parse_summary(text: str) -> dict[str, tuple[bool, str]]:
    """Parse the guest's flat `key=value` results file.

    Shared shape with `smoke_release_artifacts.parse_summary`: each line is
    `name=pass[:detail]` or `name=fail[:detail]`. A malformed or truncated
    line (a wedged guest can be killed mid-write) is not silently dropped --
    it fails as its own diagnostic.
    """
    results: dict[str, tuple[bool, str]] = {}
    for lineno, raw in enumerate(text.splitlines(), start=1):
        line = raw.strip()
        if not line:
            continue
        name, sep, rest = line.partition("=")
        if not sep:
            results[f"summary_line_{lineno}"] = (False, f"malformed line: {raw!r}")
            continue
        status, _, detail = rest.partition(":")
        results[name] = (status == "pass", detail)
    return results


def verify_collected(collected_dir: Path, *, guest_exit_code: str) -> int:
    """Read the collected Recovery results and fail with a named diagnostic."""
    summary_path = collected_dir / RESULTS_FILE
    if not summary_path.is_file():
        sys.exit(
            f"ERROR: {summary_path} is missing — the Recovery guest never wrote "
            "results (did the script even reach a shell? check the action's "
            "workdir/results/*.ppm screendumps)."
        )
    results = parse_summary(summary_path.read_text(encoding="utf-8", errors="replace"))

    failures: list[str] = []
    for name in CHECKS:
        if name not in results:
            failures.append(f"{name}: no result recorded (guest script exited early?)")
            continue
        ok, detail = results[name]
        if not ok:
            failures.append(f"{name}: {detail}")

    if guest_exit_code.strip() != "0":
        failures.append(
            f"guest script exit code {guest_exit_code!r} != '0' "
            "(see the per-check results above for which check failed)"
        )

    if failures:
        joined = "\n  - ".join(failures)
        sys.exit(f"ERROR: macOS Recovery target-run smoke failed:\n  - {joined}")

    print(
        f"macOS Recovery target-run smoke OK: {len(results)} checks recorded, all passed"
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="subcommand", required=True)

    emit = subparsers.add_parser(
        "emit-guest-script", help="write the guest script that runs the smoke checks"
    )
    emit.add_argument("--output", required=True, type=Path)
    emit.add_argument(
        "--github-output",
        default=None,
        type=Path,
        help=(
            "also append the guest script to this $GITHUB_OUTPUT file as a "
            "'script' multi-line output, so the workflow needs no separate "
            "heredoc step to hand it to the docker-mac-x64 action's `run:` "
            "input"
        ),
    )

    verify = subparsers.add_parser(
        "verify-collected", help="verify the guest's collected results"
    )
    verify.add_argument("--collected", required=True, type=Path)
    verify.add_argument("--guest-exit-code", required=True)

    args = parser.parse_args(argv)

    if args.subcommand == "emit-guest-script":
        script_text = build_guest_script()
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(script_text, encoding="utf-8")
        print(f"wrote Recovery guest script to {args.output}")
        if args.github_output is not None:
            append_github_output_multiline(args.github_output, "script", script_text)
            print(f"appended 'script' output to {args.github_output}")
        return 0

    return verify_collected(args.collected, guest_exit_code=args.guest_exit_code)


if __name__ == "__main__":
    sys.exit(main())
