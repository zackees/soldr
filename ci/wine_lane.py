"""The `wine` lane: soldr's Windows-MSVC unit tests under Wine, in a container.

zackees/ci.yml#202 (non-native platform lanes) and #198 (ci-attestations),
soldr#3534 phase 3. It attests `rust/x86_64-pc-windows-msvc/unit`: the
library unit tests of the crates below, cross-built on this Linux host with
soldr (xwin), then executed by Wine inside `docker/wine-test` -- never on the
host (GATE-005; the container sees the tree read-only).

Scope is evidence-based: the pilot ran every workspace test binary under
Wine 10 and these crates passed in full. Crates with Wine API gaps
(directory ACLs, named pipes, junctions, a Windows toolchain inside the
prefix) are not claimed here; native Windows runners keep covering them,
and a release always runs them natively.
"""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TARGET = "x86_64-pc-windows-msvc"
CRATES = ("soldr-platform", "soldr-fetch", "soldr-nextest-wrapper")
IMAGE_DIR = ROOT / "docker" / "wine-test"
TEST_THREADS = "4"


@dataclass(frozen=True)
class TestExe:
    crate: str
    path: Path


@dataclass(frozen=True)
class ExeResult:
    exe: TestExe
    returncode: int
    secs: float
    log: Path


def _run_to_file(argv: list[str], log: Path, err: Path | None = None) -> int:
    """Output goes to files, never a pipe (zackees/ci.yml PY-003). With
    `err`, stderr is kept apart so `log` holds only the tool's stdout."""
    with open(log, "wb") as out:
        if err is None:
            return subprocess.run(
                argv, cwd=ROOT, stdout=out, stderr=subprocess.STDOUT, check=False
            ).returncode
        with open(err, "wb") as errfh:
            return subprocess.run(
                argv, cwd=ROOT, stdout=out, stderr=errfh, check=False
            ).returncode


def build(out_dir: Path) -> list[TestExe]:
    messages = out_dir / "build.jsonl"
    # --lib --bins: soldr-nextest-wrapper's unit tests live in its binary.
    argv = [
        "soldr",
        "cargo",
        "test",
        "--no-run",
        "--lib",
        "--bins",
        "--target",
        TARGET,
        "--message-format=json",
    ]
    for crate in CRATES:
        argv += ["-p", crate]
    print(f"wine lane: building {', '.join(CRATES)} for {TARGET}", flush=True)
    errors = out_dir / "build.stderr"
    code = _run_to_file(argv, messages, errors)
    if code != 0:
        sys.stdout.write(errors.read_text(encoding="utf-8", errors="replace")[-8000:])
        raise SystemExit(f"wine lane: cross-build failed (exit {code})")
    exes: list[TestExe] = []
    for line in messages.read_text(encoding="utf-8", errors="replace").splitlines():
        if not line.startswith("{"):
            continue
        try:
            msg = json.loads(line)
        except ValueError:
            continue  # a non-message line; artifacts are checked for below
        if msg.get("reason") != "compiler-artifact" or not msg.get("executable"):
            continue
        if not (msg.get("profile") or {}).get("test"):
            continue
        name = Path(msg.get("manifest_path", "")).parent.name
        if name in CRATES:
            exes.append(TestExe(name, Path(msg["executable"])))
    missing = sorted(set(CRATES) - {e.crate for e in exes})
    if missing:
        raise SystemExit(f"wine lane: no test executable for {', '.join(missing)}")
    return exes


def image_tag() -> str:
    digest = hashlib.sha256((IMAGE_DIR / "Dockerfile").read_bytes()).hexdigest()[:12]
    return f"soldr-wine-test:{digest}"


def ensure_image(out_dir: Path) -> str:
    tag = image_tag()
    log = out_dir / "image.log"
    if _run_to_file(["docker", "image", "inspect", tag], log) != 0:
        print(f"wine lane: building image {tag}", flush=True)
        if _run_to_file(["docker", "build", "-t", tag, str(IMAGE_DIR)], log) != 0:
            sys.stdout.write(log.read_text(encoding="utf-8", errors="replace")[-8000:])
            raise SystemExit("wine lane: image build failed")
    return tag


def run_exe(tag: str, exe: TestExe, out_dir: Path) -> ExeResult:
    log = out_dir / f"{exe.crate}.log"
    argv = [
        "docker",
        "run",
        "--rm",
        "--network=none",
        "-v",
        f"{ROOT}:{ROOT}:ro",
        "-w",
        str(ROOT),
        tag,
        "wine",
        str(exe.path),
        f"--test-threads={TEST_THREADS}",
    ]
    start = time.monotonic()
    code = _run_to_file(argv, log)
    return ExeResult(exe, code, time.monotonic() - start, log)


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="soldr-wine-lane-") as tmp:
        out_dir = Path(tmp)
        exes = build(out_dir)
        tag = ensure_image(out_dir)
        failed: list[ExeResult] = []
        for exe in exes:
            result = run_exe(tag, exe, out_dir)
            summary = next(
                (
                    ln
                    for ln in result.log.read_text(
                        encoding="utf-8", errors="replace"
                    ).splitlines()
                    if ln.startswith("test result:")
                ),
                "no libtest summary",
            )
            print(
                f"wine lane: {exe.crate}: exit {result.returncode} in {result.secs:.0f}s -- {summary}",
                flush=True,
            )
            if result.returncode != 0:
                failed.append(result)
        for result in failed:
            sys.stdout.write(f"\n===== {result.exe.crate} (wine) =====\n")
            sys.stdout.write(
                result.log.read_text(encoding="utf-8", errors="replace")[-12000:]
            )
        return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
