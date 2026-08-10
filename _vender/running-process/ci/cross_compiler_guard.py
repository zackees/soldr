"""Guard: cross-compilation goes through soldr, and nothing else.

`soldr build --target <triple>` is soldr's documented *blessed-default*
surface: it prepares the target sysroot plus the compiler and linker
environment itself, including the managed xwin cache with clang/lld for the
MSVC targets. Its own help text is explicit that this happens "without routing
the default path through `cargo xwin`".

So a workflow that installs `cargo-zigbuild`, `cargo-xwin`, `cross`, or a
`ziglang` toolchain is not merely redundant — it is a *second* cross-compile
story competing with the blessed one. Two consequences, both bad:

1. **Release artifacts stop being reproducible through soldr.** Whatever
   toolchain that ad-hoc backend resolves is what ships, and soldr's pinning
   guarantees no longer describe the binary.
2. **The failure surface doubles.** Each extra backend brings its own version
   pin, its own cache, and its own way of breaking on a runner image bump.

This checker is deliberately narrow: it bans the *installation and invocation*
of competing cross-compilers, not the words. Documentation may name them —
this file does, at length.

`soldr lint` does not cover this. Its suites are `rust`, `deps`, and `all`
(formatting, Clippy, Dylint, dependency policy); there is no CI-workflow
suite, so this is a repo-local checker rather than a soldr invocation.

Run alone with:
    uv run --no-project python -m ci.cross_compiler_guard
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Invocations and installs of cross-compilers that compete with `soldr build`.
#
# Each pattern targets a *use*, not a mention: `cargo install cargo-zigbuild`,
# `cargo zigbuild ...`, `cargo xwin ...`, `cross build ...`, and pulling in the
# `ziglang` toolchain package.
BANNED = [
    (
        re.compile(r"cargo\s+install\s+[^\n]*\bcargo-(zigbuild|xwin)\b"),
        "installs a competing cross-compiler backend",
    ),
    (
        re.compile(r"\bcargo\s+(zigbuild|xwin)\b"),
        "invokes a competing cross-compiler backend",
    ),
    (
        re.compile(r"\bsoldr\s+cargo\s+(zigbuild|xwin)\b"),
        "routes a competing backend through soldr; use `soldr build --target`",
    ),
    (
        re.compile(r"(pip|pipx|uv pip)\s+install\s+[^\n]*\bziglang\b"),
        "installs the zig toolchain as a cross-compiler",
    ),
    (
        re.compile(r"cargo\s+install\s+[^\n]*\bcross\b(?!-)"),
        "installs `cross`, a competing cross-compile driver",
    ),
    # maturin's zig integration. Easy to miss because it is a flag and a
    # dependency extra rather than a separate tool, but it is the same thing:
    # linking through zig cc to target a glibc older than the build host's.
    #
    # The replacement is to build where that glibc actually lives — the
    # `quay.io/pypa/manylinux2014_*` image — so nothing has to fake a
    # baseline. See ci/build_wheel.py.
    (
        re.compile(r"--zig\b"),
        "builds wheels through zig; build in the manylinux container instead",
    ),
    (
        re.compile(r"maturin\[zig\]"),
        "pulls in maturin's zig extra",
    ),
]

# Files scanned: anything that can drive a build.
SUFFIXES = {".yml", ".yaml", ".sh", ".py", ".toml"}

# This guard spells out every pattern it forbids in order to search for them,
# and the docs explain the policy. Neither is a build driver.
EXEMPT = {
    "ci/cross_compiler_guard.py",
    "tests/test_ci_lint.py",
    "ci/lint.py",
}


def _relative(path: Path) -> str:
    return path.relative_to(ROOT).as_posix()


def _iter_files() -> list[Path]:
    """Git-tracked and new files, honouring .gitignore.

    Matches `ci/jemalloc_guard.py`: a filesystem walk would descend into the
    vendored `.cargo/` and `.rustup/` trees, and tracked-only would miss a
    newly added workflow until after it was committed.
    """
    result = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        raise SystemExit(
            "cross-compiler-guard: `git ls-files` failed; this guard scans "
            f"tracked files.\n{result.stderr.decode(errors='replace')}"
        )
    found: list[Path] = []
    for entry in result.stdout.decode(errors="replace").split("\0"):
        if not entry:
            continue
        path = ROOT / entry
        if path.suffix in SUFFIXES and path.is_file():
            found.append(path)
    return found


def check() -> list[str]:
    failures: list[str] = []
    for path in _iter_files():
        rel = _relative(path)
        if rel in EXEMPT:
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        for pattern, why in BANNED:
            for match in pattern.finditer(text):
                line = text[: match.start()].count("\n") + 1
                failures.append(
                    f"{rel}:{line}: {why}\n"
                    f"    found: {match.group(0).strip()}\n"
                    "    Cross-compilation must go through soldr's blessed "
                    "surface:\n"
                    "        soldr build --release --target <triple>\n"
                    "    It prepares the sysroot and the compiler/linker "
                    "environment itself."
                )
    return failures


def main() -> int:
    failures = check()
    if failures:
        print("cross-compiler-guard: FAILED", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("cross-compiler-guard: ok — cross-compilation goes through soldr only.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
