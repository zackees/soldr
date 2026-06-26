#!/usr/bin/env python3
"""Performance benchmark: bare clang vs sccache vs zccache.

Compiles a realistic C++ file (template-heavy, multiple TUs) measuring:
  - Cold build (empty cache)
  - Warm build (cache populated)
  - With response files (many -I flags)

Outputs a markdown table.
"""

from __future__ import annotations

import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# ── Paths ──────────────────────────────────────────────────────────────
HOME = Path.home()
CLANG = str(HOME / ".clang-tool-chain/clang/win/x86_64/bin/clang++.exe")
SCCACHE = r"C:\tools\python13\Scripts\sccache.exe"
ZCCACHE = str(Path(__file__).resolve().parent.parent / "target/release/zccache.exe")
ZCCACHE_DAEMON = str(Path(__file__).resolve().parent.parent / "target/release/zccache-daemon.exe")

ITERATIONS = 5  # runs per measurement
WARMUP = 1      # discarded warmup runs


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


# ── Test file generation ───────────────────────────────────────────────

def create_test_files(workdir: Path) -> tuple[Path, Path]:
    """Create a realistic C++ source and a response file with many -I paths."""

    # Create 100 fake include directories (simulates ESP32-level complexity)
    inc_dirs: list[Path] = []
    for i in range(100):
        d = workdir / "includes" / f"lib_{i:03d}" / "include"
        d.mkdir(parents=True, exist_ok=True)
        # Write a small header in each
        (d / f"lib_{i:03d}.h").write_text(
            f"#pragma once\n"
            f"namespace lib_{i:03d} {{ inline int value() {{ return {i}; }} }}\n"
        )
        inc_dirs.append(d)

    # Source file that uses templates and includes — takes meaningful compile time
    src = workdir / "main.cpp"
    includes = "\n".join(f'#include "lib_{i:03d}.h"' for i in range(100))
    src.write_text(f"""\
{includes}
#include <vector>
#include <string>
#include <map>
#include <algorithm>
#include <functional>
#include <memory>
#include <numeric>

template<typename T, int N>
struct Matrix {{
    T data[N][N];
    Matrix operator+(const Matrix& o) const {{
        Matrix r;
        for (int i = 0; i < N; i++)
            for (int j = 0; j < N; j++)
                r.data[i][j] = data[i][j] + o.data[i][j];
        return r;
    }}
    T trace() const {{
        T s = T{{}};
        for (int i = 0; i < N; i++) s += data[i][i];
        return s;
    }}
}};

template<typename T>
T fibonacci(T n) {{
    if (n <= 1) return n;
    T a = 0, b = 1;
    for (T i = 2; i <= n; i++) {{ T c = a + b; a = b; b = c; }}
    return b;
}}

int main() {{
    // Use all 100 libraries
    int sum = 0;
    {"".join(f"    sum += lib_{i:03d}::value();{chr(10)}" for i in range(100))}

    // Template instantiations
    Matrix<double, 8> m1{{}}, m2{{}};
    auto m3 = m1 + m2;
    volatile double t = m3.trace();

    Matrix<float, 16> m4{{}}, m5{{}};
    auto m6 = m4 + m5;
    volatile float t2 = m6.trace();

    // STL heavy usage
    std::vector<int> v(1000);
    std::iota(v.begin(), v.end(), 0);
    std::sort(v.begin(), v.end(), std::greater<int>());

    std::map<std::string, std::vector<int>> registry;
    for (int i = 0; i < 50; i++)
        registry[std::to_string(i)].push_back(fibonacci(i));

    return static_cast<int>(t) + static_cast<int>(t2) + sum + v[0];
}}
""")

    # Response file with -I flags (forward slashes for Windows compatibility)
    rsp = workdir / "includes.rsp"
    lines = []
    for d in inc_dirs:
        p = str(d).replace("\\", "/")
        lines.append(f'-I"{p}"')
    rsp.write_text("\n".join(lines) + "\n")

    return src, rsp


# ── Timing helpers ─────────────────────────────────────────────────────

def time_command(cmd: list[str], workdir: Path, env: dict[str, str] | None = None) -> float:
    """Run command, return wall-clock seconds. Raises on failure."""
    out_file = workdir / "main.o"
    if out_file.exists():
        out_file.unlink()

    run_env = None
    if env:
        run_env = {**os.environ, **env}

    start = time.perf_counter()
    result = subprocess.run(
        cmd,
        cwd=str(workdir),
        capture_output=True,
        text=True,
        env=run_env,
    )
    elapsed = time.perf_counter() - start

    if result.returncode != 0:
        log(f"  FAILED: {' '.join(cmd[:3])}...")
        log(f"  stdout: {result.stdout[:500]}")
        log(f"  stderr: {result.stderr[:500]}")
        raise RuntimeError(f"Command failed: {result.returncode}")

    return elapsed


def benchmark(label: str, cmd: list[str], workdir: Path,
              iterations: int = ITERATIONS, env: dict[str, str] | None = None) -> list[float]:
    """Run command multiple times, return list of times (after warmup)."""
    times: list[float] = []
    for i in range(WARMUP + iterations):
        t = time_command(cmd, workdir, env=env)
        if i >= WARMUP:
            times.append(t)
        tag = "warmup" if i < WARMUP else f"run {i - WARMUP + 1}"
        log(f"  {label} [{tag}]: {t:.3f}s")
    return times


# ── Cache management ───────────────────────────────────────────────────

def clear_sccache() -> None:
    subprocess.run([SCCACHE, "--stop-server"], capture_output=True)
    # Clear sccache local cache
    cache_dir = Path.home() / "AppData/Local/Mozilla/sccache"
    if cache_dir.exists():
        shutil.rmtree(cache_dir, ignore_errors=True)
    cache_dir2 = Path.home() / "AppData/Local/Mozilla/sccache/cache"
    if cache_dir2.exists():
        shutil.rmtree(cache_dir2, ignore_errors=True)
    subprocess.run([SCCACHE, "--start-server"], capture_output=True)


def clear_zccache() -> None:
    subprocess.run([ZCCACHE, "stop"], capture_output=True)
    # Clear zccache cache directory
    cache_dir = Path.home() / ".zccache"
    if cache_dir.exists():
        shutil.rmtree(cache_dir, ignore_errors=True)


def show_sccache_stats() -> None:
    result = subprocess.run([SCCACHE, "--show-stats"], capture_output=True, text=True)
    for line in result.stdout.splitlines():
        if any(k in line.lower() for k in ["hit", "miss", "request"]):
            log(f"    sccache: {line.strip()}")


def show_zccache_stats() -> None:
    result = subprocess.run([ZCCACHE, "status"], capture_output=True, text=True)
    output = result.stdout + result.stderr
    for line in output.splitlines():
        if any(k in line.lower() for k in ["hit", "miss", "cached", "compil"]):
            log(f"    zccache: {line.strip()}")


# ── Main benchmark ─────────────────────────────────────────────────────

def main() -> None:
    workdir = Path(tempfile.mkdtemp(prefix="zccache_bench_"))
    log(f"Workdir: {workdir}")

    src, rsp = create_test_files(workdir)
    out = workdir / "main.o"
    rsp_arg = f"@{str(rsp).replace(chr(92), '/')}"

    # Build explicit -I flags for sccache (can't use @file)
    explicit_includes = []
    for i in range(100):
        d = workdir / "includes" / f"lib_{i:03d}" / "include"
        explicit_includes.append(f"-I{d}")

    # Base compiler flags
    flags_with_includes = ["-c", "-O2", "-std=c++17"] + explicit_includes + [str(src), "-o", str(out)]
    flags_with_rsp = ["-c", "-O2", "-std=c++17", rsp_arg, str(src), "-o", str(out)]

    results: dict[str, dict[str, float]] = {}

    # ── 1. Bare clang (baseline) ──────────────────────────────────────
    log("\n=== Bare clang (no cache) ===")
    bare_cmd = [CLANG] + flags_with_rsp
    times = benchmark("bare clang", bare_cmd, workdir)
    med = statistics.median(times)
    results["bare clang"] = {"cold": med, "warm": med}  # no cache = same both times
    log(f"  -> median: {med:.3f}s\n")

    # ── 2. sccache — cold (empty cache) ──────────────────────────────
    log("=== sccache (cold) ===")
    clear_sccache()
    sccache_cold_cmd = [SCCACHE, CLANG] + flags_with_includes  # sccache can't do @file
    times_cold = benchmark("sccache cold", sccache_cold_cmd, workdir)
    cold_med = statistics.median(times_cold)
    show_sccache_stats()

    # ── 3. sccache — warm (cache populated) ──────────────────────────
    log("\n=== sccache (warm) ===")
    # Cache is now warm from the cold runs
    times_warm = benchmark("sccache warm", sccache_cold_cmd, workdir)
    warm_med = statistics.median(times_warm)
    show_sccache_stats()
    results["sccache"] = {"cold": cold_med, "warm": warm_med}
    log(f"  -> cold median: {cold_med:.3f}s, warm median: {warm_med:.3f}s\n")

    # ── 4. sccache + response file (must bypass — the fbuild problem) ──
    log("=== sccache + @file (must bypass = bare clang) ===")
    log("  (sccache expands @file, hits 32K limit on Windows)")
    log(f"  -> same as bare clang: {results['bare clang']['cold']:.3f}s\n")
    results["sccache + @file"] = results["bare clang"].copy()

    # ── 5. zccache — cold (empty cache) ──────────────────────────────
    log("=== zccache (cold) ===")
    clear_zccache()
    # Start daemon in background
    subprocess.Popen(
        [ZCCACHE_DAEMON, "--foreground"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(2)  # let daemon start

    # Create a session (zccache requires ZCCACHE_SESSION_ID)
    session_result = subprocess.run(
        [ZCCACHE, "session-start", "--compiler", CLANG, "--pid", str(os.getpid())],
        capture_output=True, text=True,
    )
    if session_result.returncode != 0:
        log(f"  session-start failed: {session_result.stderr}")
        sys.exit(1)
    import json
    session_info = json.loads(session_result.stdout.strip())
    session_id = str(session_info["session_id"])
    log(f"  Session ID: {session_id} (started at {session_info['started_at']})")
    zccache_env = {"ZCCACHE_SESSION_ID": session_id}

    zccache_cmd = [ZCCACHE, CLANG] + flags_with_rsp
    times_cold_z = benchmark("zccache cold", zccache_cmd, workdir, env=zccache_env)
    cold_med_z = statistics.median(times_cold_z)
    show_zccache_stats()

    # ── 6. zccache — warm (cache populated) ──────────────────────────
    log("\n=== zccache (warm — with @file!) ===")
    times_warm_z = benchmark("zccache warm", zccache_cmd, workdir, env=zccache_env)
    warm_med_z = statistics.median(times_warm_z)
    show_zccache_stats()
    results["zccache"] = {"cold": cold_med_z, "warm": warm_med_z}
    results["zccache + @file"] = results["zccache"].copy()  # same — zccache handles it
    log(f"  -> cold median: {cold_med_z:.3f}s, warm median: {warm_med_z:.3f}s\n")

    # End session
    subprocess.run([ZCCACHE, "session-end", session_id], capture_output=True)

    # ── Cleanup ──────────────────────────────────────────────────────
    subprocess.run([ZCCACHE, "stop"], capture_output=True)
    subprocess.run([SCCACHE, "--stop-server"], capture_output=True)

    # ── Results table ────────────────────────────────────────────────
    baseline = results["bare clang"]["cold"]
    print("\n## Benchmark Results\n")
    print(f"**System**: Windows x86_64, clang 21.1.5, {ITERATIONS} iterations (median)")
    print(f"**Source**: Template-heavy C++ with 100 include dirs, STL usage")
    print()
    print("| Tool | Cold Build | Warm Build | Warm Speedup vs Bare |")
    print("|------|-----------|------------|---------------------|")
    for name in ["bare clang", "sccache", "sccache + @file", "zccache", "zccache + @file"]:
        r = results[name]
        cold = r["cold"]
        warm = r["warm"]
        speedup = baseline / warm if warm > 0 else 0
        speedup_str = f"{speedup:.1f}x" if speedup > 1.05 else "1.0x (no cache)"
        note = ""
        if name == "sccache + @file":
            note = " *"
        print(f"| {name}{note} | {cold:.3f}s | {warm:.3f}s | {speedup_str} |")

    print()
    print("\\* sccache + @file: sccache must be **bypassed** when response files are used")
    print("  (it expands @file args, recreating the 32K command-line limit on Windows).")
    print("  So it falls back to bare clang — no caching benefit.")
    print()
    print("zccache handles @file natively — caching works in all cases.")

    # Cleanup workdir
    shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    main()
