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
import os
import pathlib
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


def main() -> int:
    args = parse_args()
    soldr_bin = pathlib.Path(args.soldr_bin).resolve()
    if not soldr_bin.is_file():
        raise SystemExit(f"reld_live_fetch_probe: soldr binary not found: {soldr_bin}")

    home = pathlib.Path.home()
    before = set(find_reld_under(home))

    with tempfile.TemporaryDirectory(prefix="soldr-reld-live-probe-") as tmp:
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

        env = dict(os.environ)
        env["SOLDR_LINKER"] = "reld"
        env.pop("SOLDR_RELD_BIN", None)

        result = subprocess.run(
            [str(soldr_bin), "cargo", "build"],
            cwd=project,
            env=env,
            capture_output=True,
            text=True,
            timeout=600,
        )
        if result.returncode != 0:
            print(result.stdout, file=sys.stderr)
            print(result.stderr, file=sys.stderr)
            raise SystemExit(
                "reld_live_fetch_probe: SOLDR_LINKER=reld build of a scratch crate "
                f"failed (exit {result.returncode}) on this host."
            )

        exe_suffix = ".exe" if os.name == "nt" else ""
        built_bin = project / "target" / "debug" / f"reld-live-probe{exe_suffix}"
        if not built_bin.is_file():
            raise SystemExit(
                f"reld_live_fetch_probe: expected linked binary missing: {built_bin}"
            )
        run_result = subprocess.run(
            [str(built_bin)], capture_output=True, text=True, timeout=30
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
