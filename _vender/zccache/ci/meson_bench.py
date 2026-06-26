#!/usr/bin/env python3
"""Meson+Ninja full-project benchmark: bare clang vs sccache vs zccache.

Builds FastLED10 (a large C++ library) with meson+ninja, measuring both
meson setup time (compiler probes) and ninja build time (actual compilation).

Usage:
    uv run python ci/meson_bench.py ~/dev/fastled10
    uv run python ci/meson_bench.py ~/dev/fastled10 --scenarios bare,zccache-cold,zccache-warm
    uv run python ci/meson_bench.py ~/dev/fastled10 --jobs 8
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# ── Tool path resolution ─────────────────────────────────────────────

HOME = Path.home()
REPO_ROOT = Path(__file__).resolve().parent.parent

# Defaults — overridden by resolve_tools() based on the project dir
TOOL_PATHS: dict[str, str] = {}

ALL_SCENARIOS = ["bare", "sccache-cold", "sccache-warm", "zccache-cold", "zccache-warm"]


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def resolve_msys2_path(p: Path) -> Path:
    r"""Convert MSYS2/Cygwin-style paths (e.g. /c/Users/...) to native Windows paths.

    When running under MSYS2/Git Bash, the shell expands ~ to /c/Users/...
    but Python's pathlib on Windows doesn't understand that mount format.
    On Windows, Path('/c/Users/...') becomes '\\c\\Users\\...' so we check
    both separators.
    """
    s = str(p)
    for sep in ("/", "\\"):
        if len(s) >= 3 and s[0] == sep and s[1].isalpha() and s[2] == sep:
            return Path(f"{s[1].upper()}:{s[2:]}")
    return p


def resolve_tools(project_dir: Path) -> dict[str, str]:
    """Discover tool paths from the project directory and system."""
    tools: dict[str, str] = {}

    # Compilers — ctc-clang launchers in the project's .cached dir
    clang_dir = project_dir / ".cached" / "clang-native"
    tools["c"] = str(clang_dir / "ctc-clang.exe")
    tools["cpp"] = str(clang_dir / "ctc-clang++.exe")

    # Archiver
    tools["ar"] = str(HOME / ".clang-tool-chain/clang/win/x86_64/bin/llvm-ar.exe")

    # sccache — in the project's venv
    tools["sccache"] = str(project_dir / ".venv/Scripts/sccache.EXE")

    # zccache — from this repo's release build
    tools["zccache"] = str(REPO_ROOT / "target/release/zccache.exe")

    # meson + ninja — from the project's venv
    tools["meson"] = str(project_dir / ".venv/Scripts/meson.exe")
    tools["ninja"] = str(project_dir / ".venv/Scripts/ninja.exe")

    return tools


def validate_tools(tools: dict[str, str]) -> None:
    """Check that all required tools exist."""
    missing = []
    for name, path in tools.items():
        if not Path(path).exists():
            missing.append(f"  {name}: {path}")
    if missing:
        log("ERROR: Missing tools:")
        for m in missing:
            log(m)
        sys.exit(1)


# ── Native file generation ───────────────────────────────────────────

def write_native_file(
    path: Path,
    c_compiler: str,
    cpp_compiler: str,
    ar: str,
    wrapper: str | None = None,
) -> None:
    """Write a meson native file.

    If wrapper is set, compilers are invoked as [wrapper, compiler].
    """
    if wrapper:
        c_entry = f"['{wrapper}', '{c_compiler}']"
        cpp_entry = f"['{wrapper}', '{cpp_compiler}']"
    else:
        c_entry = f"['{c_compiler}']"
        cpp_entry = f"['{cpp_compiler}']"

    content = f"""\
[binaries]
c = {c_entry}
cpp = {cpp_entry}
ar = ['{ar}']

[host_machine]
system = 'windows'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
"""
    path.write_text(content, encoding="utf-8")


# ── Cache management ─────────────────────────────────────────────────

def clear_sccache(tools: dict[str, str]) -> None:
    """Stop sccache server, wipe its local cache, and restart."""
    sccache = tools["sccache"]
    subprocess.run([sccache, "--stop-server"], capture_output=True)
    time.sleep(1)
    cache_dir = Path(os.environ.get("LOCALAPPDATA", "")) / "Mozilla" / "sccache"
    if cache_dir.exists():
        shutil.rmtree(cache_dir, ignore_errors=True)
    subprocess.run([sccache, "--start-server"], capture_output=True)
    time.sleep(1)
    # Verify server is ready
    subprocess.run([sccache, "--show-stats"], capture_output=True)


def clear_zccache(tools: dict[str, str]) -> None:
    """Stop zccache daemon, wipe its cache, and pre-start daemon."""
    zccache = tools["zccache"]
    subprocess.run([zccache, "stop"], capture_output=True)
    time.sleep(0.5)
    cache_dir = Path.home() / ".zccache"
    if cache_dir.exists():
        shutil.rmtree(cache_dir, ignore_errors=True)
    # Pre-start daemon so meson probes don't race on auto-start
    subprocess.run([zccache, "start"], capture_output=True)
    time.sleep(1)


def ensure_zccache_daemon(tools: dict[str, str]) -> None:
    """Ensure zccache daemon is running (for warm scenarios)."""
    zccache = tools["zccache"]
    result = subprocess.run([zccache, "status"], capture_output=True, text=True)
    if result.returncode != 0:
        subprocess.run([zccache, "start"], capture_output=True)
        time.sleep(1)


def noop_clear(tools: dict[str, str]) -> None:  # noqa: ARG001
    """No cache to clear."""
    del tools


# ── Scenario execution ───────────────────────────────────────────────

def time_command(
    cmd: list[str],
    cwd: str,
    label: str,
    env: dict[str, str] | None = None,
) -> float:
    """Run a command, return wall-clock seconds. Raises on failure."""
    run_env = {**os.environ, **(env or {})}
    log(f"  [{label}] {' '.join(cmd[:6])}{'...' if len(cmd) > 6 else ''}")

    start = time.perf_counter()
    result = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, env=run_env)
    elapsed = time.perf_counter() - start

    if result.returncode != 0:
        log(f"  FAILED ({label}):")
        log(f"  stdout: {result.stdout[-1000:]}")
        log(f"  stderr: {result.stderr[-1000:]}")
        raise RuntimeError(f"{label} failed with exit code {result.returncode}")

    return elapsed


def run_scenario(
    name: str,
    project_dir: Path,
    tools: dict[str, str],
    native_file: Path,
    clear_fn,
    ninja_jobs: int | None = None,
    build_dir_name: str | None = None,
) -> dict[str, float]:
    """Run a single benchmark scenario.

    Returns dict with setup_s, build_s, total_s.
    """
    # Use a stable build dir name so cold/warm pairs share the same include
    # paths.  Meson generates relative -I flags resolved against the build dir;
    # different dir names produce different absolute paths and cause spurious
    # cache misses in tools that key on include paths (like zccache).
    dir_name = build_dir_name or f"bench-{name}"
    build_dir = project_dir / ".build" / dir_name
    log(f"\n{'='*60}")
    log(f"  Scenario: {name}")
    log(f"  Build dir: {build_dir}")
    log(f"{'='*60}")

    # 1. Clean build directory
    if build_dir.exists():
        shutil.rmtree(build_dir)

    # 2. Clear cache
    clear_fn(tools)

    # Put the project venv on PATH so meson can find ninja
    venv_scripts = str(project_dir / ".venv" / "Scripts")
    path_env = {"PATH": venv_scripts + os.pathsep + os.environ.get("PATH", "")}

    # 3. meson setup
    setup_cmd = [
        tools["meson"], "setup",
        "--native-file", str(native_file),
        str(build_dir),
        "-Dbuild_mode=quick",
        "-Denable_examples=false",
        "-Denable_unit_tests=false",
    ]
    setup_s = time_command(setup_cmd, str(project_dir), f"{name} meson setup", env=path_env)
    log(f"  meson setup: {setup_s:.2f}s")

    # 4. ninja build
    ninja_cmd = [tools["ninja"], "-C", str(build_dir)]
    if ninja_jobs is not None:
        ninja_cmd.extend(["-j", str(ninja_jobs)])
    build_s = time_command(ninja_cmd, str(project_dir), f"{name} ninja build", env=path_env)
    log(f"  ninja build: {build_s:.2f}s")

    total_s = setup_s + build_s
    log(f"  total: {total_s:.2f}s")

    return {"setup_s": setup_s, "build_s": build_s, "total_s": total_s}


# ── Main ──────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(
        description="Benchmark meson+ninja builds: bare vs sccache vs zccache"
    )
    parser.add_argument(
        "project_dir",
        type=Path,
        help="Path to the FastLED10 project (e.g. ~/dev/fastled10)",
    )
    parser.add_argument(
        "--scenarios",
        type=str,
        default=",".join(ALL_SCENARIOS),
        help=f"Comma-separated scenarios to run (default: all). Options: {','.join(ALL_SCENARIOS)}",
    )
    parser.add_argument(
        "--jobs", "-j",
        type=int,
        default=None,
        help="Number of ninja parallel jobs (default: ninja's default)",
    )
    args = parser.parse_args()

    project_dir = resolve_msys2_path(args.project_dir.expanduser()).resolve()
    if not (project_dir / "meson.build").exists():
        log(f"ERROR: {project_dir} does not contain meson.build")
        sys.exit(1)

    scenarios = [s.strip() for s in args.scenarios.split(",")]
    for s in scenarios:
        if s not in ALL_SCENARIOS:
            log(f"ERROR: Unknown scenario '{s}'. Options: {', '.join(ALL_SCENARIOS)}")
            sys.exit(1)

    # Resolve and validate tools
    tools = resolve_tools(project_dir)
    validate_tools(tools)
    log("Tools validated:")
    for name, path in tools.items():
        log(f"  {name}: {path}")

    # Generate native files in a temp directory
    tmpdir = Path(tempfile.mkdtemp(prefix="zccache_meson_bench_"))
    log(f"\nNative files dir: {tmpdir}")

    # Normalize paths for meson (forward slashes)
    c = tools["c"].replace("\\", "/")
    cpp = tools["cpp"].replace("\\", "/")
    ar = tools["ar"].replace("\\", "/")
    sccache = tools["sccache"].replace("\\", "/")
    zccache = tools["zccache"].replace("\\", "/")

    native_bare = tmpdir / "native_bare.ini"
    write_native_file(native_bare, c, cpp, ar)

    native_sccache = tmpdir / "native_sccache.ini"
    write_native_file(native_sccache, c, cpp, ar, wrapper=sccache)

    native_zccache = tmpdir / "native_zccache.ini"
    write_native_file(native_zccache, c, cpp, ar, wrapper=zccache)

    # Map scenario names to (native_file, clear_fn, build_dir_name).
    # Cold/warm pairs share a build dir so meson's relative -I flags resolve
    # to the same absolute paths, giving the cache tool a fair shot.
    scenario_config = {
        "bare": (native_bare, noop_clear, "bench-bare"),
        "sccache-cold": (native_sccache, clear_sccache, "bench-sccache"),
        "sccache-warm": (native_sccache, noop_clear, "bench-sccache"),
        "zccache-cold": (native_zccache, clear_zccache, "bench-zccache"),
        "zccache-warm": (native_zccache, ensure_zccache_daemon, "bench-zccache"),
    }

    # Run scenarios in order
    results: dict[str, dict[str, float]] = {}
    for name in scenarios:
        native_file, clear_fn, build_dir_name = scenario_config[name]
        results[name] = run_scenario(
            name, project_dir, tools, native_file, clear_fn,
            ninja_jobs=args.jobs,
            build_dir_name=build_dir_name,
        )

    # Cleanup temp dir
    shutil.rmtree(tmpdir, ignore_errors=True)

    # Cleanup cache servers
    subprocess.run([tools["sccache"], "--stop-server"], capture_output=True)
    subprocess.run([tools["zccache"], "stop"], capture_output=True)

    # ── Results table ─────────────────────────────────────────────────
    baseline = results.get("bare", {}).get("total_s", 0)

    print("\n## Meson+Ninja Benchmark Results\n")
    print("**Project**: FastLED10 (large C++ library)")
    print("**Build mode**: quick (-O0 -g1), examples and tests disabled")
    print("**Platform**: Windows x86_64")
    print()
    print("| Scenario       | meson setup | ninja build | Total   | vs bare  |")
    print("|----------------|-------------|-------------|---------|----------|")

    for name in scenarios:
        r = results[name]
        setup = r["setup_s"]
        build = r["build_s"]
        total = r["total_s"]
        if baseline > 0:
            ratio = total / baseline
            vs_bare = f"{ratio:.2f}x"
        else:
            vs_bare = "—"
        print(f"| {name:<14} | {setup:>9.1f}s | {build:>9.1f}s | {total:>5.1f}s | {vs_bare:>8} |")

    print()

    # Cleanup build dirs
    log("\nCleaning up benchmark build dirs...")
    cleaned = set()
    for name in scenarios:
        _, _, dir_name = scenario_config[name]
        if dir_name not in cleaned:
            build_dir = project_dir / ".build" / dir_name
            if build_dir.exists():
                shutil.rmtree(build_dir, ignore_errors=True)
            cleaned.add(dir_name)
    log("Done.")


if __name__ == "__main__":
    main()
