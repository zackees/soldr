#!/usr/bin/env python3
"""CI proof for soldr#3276 §6: dogfooding `linker = "reld"`.

Builds `soldr-cli` through the locally built `soldr` binary with verbose
rustc invocations captured, then asserts two things about the *same*
rustc invocation that links the final `soldr` binary:

1. The linker soldr injected resolves to an existing, absolute-path
   executable derived from a managed/fetched `reld` -- never the bare
   literal `reld` name that clang's `--ld-path=` rejects
   (`clang: error: invalid linker name in argument '--ld-path=reld'`,
   the soldr#3277-shaped symptom this proof exists to catch).
2. `--cfg tokio_unstable` (from this repo's own `.cargo/config.toml`
   `[build] rustflags`) is still present on that same invocation, proving
   the reld linker injection did not silently replace the project's
   `[build] rustflags` the way a naive `CARGO_TARGET_*_RUSTFLAGS`
   injection would (soldr#3277).

Run from the repository root with the workspace already declaring
`linker = "reld"` in `[workspace.metadata.soldr]`:

    uv run --no-project --python 3.13 python .github/scripts/reld_dogfood_proof.py \
        --soldr-bin target/debug/soldr

Exits non-zero with a descriptive message on any failure. Intentionally a
narrow, single-purpose script per soldr repo convention: complex CI logic
belongs in `ci/*.py` / `.github/scripts/*.py`, not inline workflow YAML.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import subprocess
import sys
import time

# A `-C linker=<path>` value soldr injects must always be an absolute path:
# either the content-addressed clang driver shim (Linux) or the direct
# resolved `reld` executable (Windows/macOS). It must never be a bare name.
BARE_RELD_NAMES = {"reld", "reld.exe"}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--soldr-bin",
        default="target/debug/soldr",
        help="Path to the soldr binary to drive the build with.",
    )
    parser.add_argument(
        "--package",
        default="soldr-cli",
        help="Cargo package to build (must produce a linked binary).",
    )
    parser.add_argument(
        "--repo-root",
        default=".",
        help="Repository root (defaults to the current directory).",
    )
    return parser.parse_args()


def force_rebuild(repo_root: pathlib.Path) -> None:
    """Change the CLI binary's entry point so the link step always reruns.

    A bare `touch()` is not reliable here: soldr's own compile cache keys on
    content (blake3), and in CI this script typically runs immediately after
    a step that already built the same package, so cargo's own fingerprint
    may also see nothing new to do. Appending a harmless, uniquely-timestamped
    comment changes the file's content hash, guaranteeing both cargo and
    soldr's cache see a real change and actually re-invoke rustc for the
    final link -- without it, an already-cached build produces zero rustc
    invocations under `-v` and the proof would vacuously "pass" by finding
    nothing to check.
    """
    entry = repo_root / "crates" / "soldr-cli" / "src" / "main.rs"
    marker = f"\n// reld_dogfood_proof cache-bust: {time.time_ns()}\n"
    with entry.open("a", encoding="utf-8") as handle:
        handle.write(marker)


def run_verbose_build(
    soldr_bin: pathlib.Path, package: str, repo_root: pathlib.Path
) -> str:
    result = subprocess.run(
        [str(soldr_bin), "cargo", "build", "-p", package, "-v"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        timeout=900,
        check=False,
    )
    combined = result.stdout + result.stderr
    if result.returncode != 0:
        print(combined, file=sys.stderr)
        raise SystemExit(
            f"reld_dogfood_proof: `soldr cargo build -p {package} -v` failed "
            f"(exit {result.returncode}) -- this is the exact dogfooding build "
            "soldr#3276 §6 requires to succeed."
        )
    return combined


def final_link_invocations(build_log: str, package_bin_crate: str) -> list[str]:
    """Every rustc invocation line that links a `bin` crate-type artifact."""
    lines = []
    for line in build_log.splitlines():
        if "Running `" not in line:
            continue
        if "--crate-type bin" not in line:
            continue
        if (
            f"--crate-name {package_bin_crate}" not in line
            and "--crate-name soldr " not in line
        ):
            continue
        lines.append(line)
    return lines


def extract_linker_path(invocation: str) -> str | None:
    match = re.search(r"-C ?linker=(\S+)", invocation)
    return match.group(1) if match else None


def main() -> int:
    args = parse_args()
    repo_root = pathlib.Path(args.repo_root).resolve()
    soldr_bin = (repo_root / args.soldr_bin).resolve()
    if not soldr_bin.is_file():
        raise SystemExit(f"reld_dogfood_proof: soldr binary not found at {soldr_bin}")

    force_rebuild(repo_root)
    build_log = run_verbose_build(soldr_bin, args.package, repo_root)

    invocations = final_link_invocations(build_log, "soldr")
    if not invocations:
        raise SystemExit(
            "reld_dogfood_proof: no `--crate-type bin` rustc invocation was "
            "captured under `-v` -- the build may have been fully cached. "
            "This script must run against a build that actually relinks."
        )

    failures: list[str] = []
    for invocation in invocations:
        linker = extract_linker_path(invocation)
        if linker is None:
            # No -C linker= at all means soldr injected nothing for this
            # target -- acceptable only if the project itself declared a
            # linker some other way, but soldr's own Cargo.toml declares
            # `linker = "reld"`, so soldr must always inject one here.
            failures.append(f"no `-C linker=` on invocation: {invocation[:200]}...")
            continue

        bare_name = pathlib.Path(linker).name
        if bare_name in BARE_RELD_NAMES and not pathlib.Path(linker).is_absolute():
            failures.append(
                f"linker value is a bare unresolved `{linker}` "
                f"(clang would reject --ld-path={linker}): {invocation[:200]}..."
            )
            continue
        if not pathlib.Path(linker).is_absolute():
            failures.append(f"linker value is not an absolute path: {linker}")
            continue
        if not pathlib.Path(linker).is_file():
            failures.append(f"injected linker path does not exist on disk: {linker}")
            continue

        # On Linux this is a generated clang driver shim; read it back and
        # confirm the embedded --ld-path= is itself an absolute, existing
        # reld executable -- not a second layer of indirection to a bare name.
        if "linker-shims" in linker:
            shim_body = pathlib.Path(linker).read_text(encoding="utf-8")
            shim_match = re.search(r"--ld-path=(\S+)", shim_body)
            if shim_match is None:
                failures.append(f"linker shim has no --ld-path=: {shim_body}")
                continue
            reld_path = shim_match.group(1).strip("'\"")
            if not pathlib.Path(reld_path).is_absolute():
                failures.append(f"shim's --ld-path= is not absolute: {reld_path}")
                continue
            if not pathlib.Path(reld_path).is_file():
                failures.append(f"shim's --ld-path= target does not exist: {reld_path}")
                continue

        if "--cfg tokio_unstable" not in invocation:
            failures.append(
                "the reld linker invocation lost `--cfg tokio_unstable` -- "
                "the project's own [build] rustflags in .cargo/config.toml "
                f"were clobbered (soldr#3277-shaped regression): {invocation[:200]}..."
            )

    if failures:
        print("reld_dogfood_proof: FAILED", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(
        f"reld_dogfood_proof: OK -- {len(invocations)} link invocation(s) used a "
        "resolved absolute reld linker path and retained --cfg tokio_unstable."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
