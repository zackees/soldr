#!/usr/bin/env python3
"""Live, network-touching proof that `SOLDR_LINKER=reld` fetches and links
successfully on the *current* host (soldr#3276 §6/§ live-acceptance gap).

Unlike `.github/scripts/reld_dogfood_proof.py` (which proves the linker
*injection* is well-formed against this repo's own build), this script
proves the *end-to-end* story on a fresh host: a scratch crate, built with
`SOLDR_LINKER=reld` and no `SOLDR_RELD_BIN` override, actually downloads the
pinned `reld` release asset for this platform, links successfully, and the
resulting binary runs. It is deliberately platform-agnostic: reld supports
Linux (native ELF backend), Windows MSVC (bridges to lld-link), and macOS
(bridges to ld64.lld) per `crates/soldr-cli/src/linker.rs`.

Usage:
    uv run --no-project --python 3.13 python ci/reld_live_fetch_probe.py \
        --soldr-bin target/release/soldr[.exe]
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--soldr-bin", required=True, help="Path to a built soldr binary."
    )
    return parser.parse_args()


def find_reld_under(home: pathlib.Path) -> list[pathlib.Path]:
    hits: list[pathlib.Path] = []
    for candidate_root in (home / ".soldr", home / ".soldr-dev"):
        if not candidate_root.is_dir():
            continue
        for path in candidate_root.rglob("reld*"):
            if path.is_file() and os.access(path, os.X_OK):
                hits.append(path)
    return hits


def linked_executable(cargo_stdout: str) -> pathlib.Path | None:
    """The executable Cargo reports for the scratch crate, from its JSON messages.

    Cargo's output directory depends on whether `--target` was passed (soldr
    passes the host triple explicitly on Windows, so the binary lands under
    `target/<triple>/debug`), so the probe asks Cargo instead of guessing.
    """
    for line in cargo_stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (
            isinstance(message, dict)
            and message.get("reason") == "compiler-artifact"
            and message.get("target", {}).get("name") == "reld-live-probe"
            and message.get("executable")
        ):
            return pathlib.Path(message["executable"])
    return None


def reld_linked_outputs(invocation_log: pathlib.Path) -> list[str]:
    """File names of outputs reld reports having linked successfully."""
    if not invocation_log.is_file():
        return []
    names: list[str] = []
    for line in invocation_log.read_text(encoding="utf-8").splitlines():
        record = json.loads(line)
        if record.get("status") == "success" and isinstance(record.get("output"), str):
            # reld records the path as the linker saw it; split on either
            # separator so a Windows path parses on any host.
            names.append(re.split(r"[\\/]", record["output"])[-1])
    return names


def main() -> int:
    args = parse_args()
    soldr_bin = pathlib.Path(args.soldr_bin).resolve()
    if not soldr_bin.is_file():
        raise SystemExit(f"reld_live_fetch_probe: soldr binary not found: {soldr_bin}")

    home = pathlib.Path.home()
    before = set(find_reld_under(home))

    # `ignore_cleanup_errors=True`: soldr's daemon can still hold an open
    # handle inside this directory on Windows when the `with` block exits,
    # which otherwise raises `PermissionError` during cleanup and masks
    # whatever real failure (or success) happened above it.
    with tempfile.TemporaryDirectory(
        prefix="soldr-reld-live-probe-", ignore_cleanup_errors=True
    ) as tmp:
        project = pathlib.Path(tmp) / "probe"
        project.mkdir()
        (project / "Cargo.toml").write_text(
            '[package]\nname = "reld-live-probe"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (project / "src").mkdir()
        (project / "src" / "main.rs").write_text(
            'fn main() { println!("soldr-reld-live-probe ok"); }\n'
        )
        # soldr refuses an unpinned PATH-rustc fallback (cache-contract
        # safety); pin the scratch crate to stable so the probe doesn't need
        # to know this repo's exact toolchain version.
        (project / "rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "stable"\n'
        )

        invocation_log = pathlib.Path(tmp) / "reld-invocations.jsonl"
        env = dict(os.environ)
        env["SOLDR_LINKER"] = "reld"
        env.pop("SOLDR_RELD_BIN", None)
        # reld's own evidence that it performed the link: the route line on
        # stderr and a JSONL record per successful invocation.
        env["RELD_LOG_ENGINE"] = "1"
        env["RELD_INVOCATION_LOG"] = str(invocation_log)
        result = subprocess.run(
            [
                str(soldr_bin),
                "cargo",
                "build",
                "--message-format=json-render-diagnostics",
            ],
            cwd=project,
            env=env,
            capture_output=True,
            text=True,
            timeout=600,
            check=False,
        )
        # Always surface the build's own output: a returncode of 0 with a
        # missing binary is just as diagnosable as a nonzero returncode, and
        # printing only on the latter hid the real cause once already.
        print(result.stdout, file=sys.stderr)
        print(result.stderr, file=sys.stderr)
        if result.returncode != 0:
            raise SystemExit(
                "reld_live_fetch_probe: SOLDR_LINKER=reld build of a scratch crate "
                f"failed (exit {result.returncode}) on this host."
            )
        built_bin = linked_executable(result.stdout)
        if built_bin is None or not built_bin.is_file():
            raise SystemExit(
                "reld_live_fetch_probe: cargo reported no linked executable for "
                f"reld-live-probe (reported: {built_bin}; build exited "
                f"{result.returncode} with no reported error)"
            )
        linked = reld_linked_outputs(invocation_log)
        print(f"reld_live_fetch_probe: reld invocation records: {linked}")
        if not any(name.startswith("reld_live_probe") for name in linked):
            raise SystemExit(
                "reld_live_fetch_probe: reld recorded no successful link of "
                f"reld-live-probe in {invocation_log}; the binary was not linked by reld."
            )
        run_result = subprocess.run(
            [str(built_bin)], capture_output=True, text=True, timeout=30, check=False
        )
        if (
            run_result.returncode != 0
            or "soldr-reld-live-probe ok" not in run_result.stdout
        ):
            raise SystemExit(
                "reld_live_fetch_probe: the reld-linked scratch binary did not run "
                f"correctly: stdout={run_result.stdout!r} stderr={run_result.stderr!r}"
            )

    after = set(find_reld_under(home))
    fetched = after - before
    if not fetched and not before:
        raise SystemExit(
            "reld_live_fetch_probe: build succeeded but no reld executable was found "
            "under ~/.soldr or ~/.soldr-dev -- cannot confirm a live fetch happened "
            "rather than reusing a pre-seeded binary."
        )

    print(
        "reld_live_fetch_probe: OK -- SOLDR_LINKER=reld built and ran a scratch "
        f"crate on this host. reld executable(s) present: {sorted(str(p) for p in after)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
