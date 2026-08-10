from __future__ import annotations

import json
import os
import platform
import shlex
import shutil
import signal
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path

from ci.dev_build import ensure_dev_wheel
from ci.soldr import cargo_command

ROOT = Path(__file__).resolve().parent.parent
IN_RUNNING_PROCESS_ENV = "IN_RUNNING_PROCESS"
IN_RUNNING_PROCESS_VALUE = "running-process-cli"
GITHUB_ACTIONS_ENV = "GITHUB_ACTIONS"
SKIP_LINUX_DOCKER_ENV = "RUNNING_PROCESS_SKIP_LINUX_DOCKER"
DEFAULT_TEST_TIMEOUT_SECONDS = "40"
DEFAULT_COMMAND_TIMEOUT_SECONDS = 10.0
DEFAULT_RUST_TEST_TIMEOUT_SECONDS = 60.0
# Windows runs cargo nextest with --test-threads=1 for extra isolation around
# filesystem and named-pipe races. Serialized ConPTY teardown — Job Object close + child wait
# + reader-thread join — can stay quiet for 10s+ at a time, so the
# supervisor's idle window needs more headroom than the parallel POSIX path.
WINDOWS_RUST_TEST_TIMEOUT_SECONDS = 180.0
# The containerized run includes a silent maturin release build of the
# project wheel ("Building running-process @ file:///work"), which can stay
# quiet for ~3 minutes — give the idle watchdog the same headroom as the
# release-build phase.
DEFAULT_LINUX_TEST_TIMEOUT_SECONDS = 600.0
DEFAULT_RELEASE_BUILD_TIMEOUT_SECONDS = 600.0
DEFAULT_PYTEST_TIMEOUT_SECONDS = 40.0
COMMAND_TIMEOUT_ENV = "RUNNING_PROCESS_TEST_COMMAND_TIMEOUT_SECONDS"

# pytest-cov args for the first pytest run (creates fresh .coverage)
_COV_PYTEST_FIRST = [
    "--cov=running_process",
    "--cov-report=term",
]
# pytest-cov args for subsequent runs (appends, then writes final XML)
_COV_PYTEST_APPEND = [
    "--cov=running_process",
    "--cov-report=term",
    "--cov-report=xml:coverage-python.xml",
    "--cov-append",
]


def command_timeout_seconds() -> float | None:
    configured = os.environ.get(COMMAND_TIMEOUT_ENV)
    if configured is None:
        return DEFAULT_COMMAND_TIMEOUT_SECONDS
    configured = configured.strip()
    if not configured:
        return None
    timeout = float(configured)
    if timeout <= 0:
        return None
    return timeout


def supervised_command(
    python: Path,
    *command: str,
    timeout: float | None = None,
) -> list[str]:
    effective_timeout = command_timeout_seconds() if timeout is None else timeout
    if effective_timeout is None:
        return list(command)
    return [
        str(python),
        "-m",
        "running_process.cli",
        "--timeout",
        str(effective_timeout),
        "--",
        *command,
    ]


def _supervised_pytest_command(
    python: Path,
    *pytest_args: str,
) -> list[str]:
    # PTY-heavy Python suites can legitimately stay quiet for longer than the
    # default 10-second command timeout on loaded CI runners, especially under
    # coverage. Use the same wider window as the Linux docker path.
    return supervised_command(
        python,
        str(python),
        "-m",
        "pytest",
        "-vv",
        *pytest_args,
        timeout=DEFAULT_PYTEST_TIMEOUT_SECONDS,
    )


def _linux_unit_test_command(
    python: Path,
    *pytest_args: str,
) -> list[str]:
    command = [
        str(python),
        "-m",
        "ci.linux_docker",
        "all",
        "--output-dir",
        str(ROOT / "linux"),
    ]
    if pytest_args:
        command.extend(["--pytest-args", shlex.join(pytest_args)])
    return supervised_command(
        python,
        *command,
        timeout=DEFAULT_LINUX_TEST_TIMEOUT_SECONDS,
    )


def _release_build_command(python: Path) -> list[str]:
    return supervised_command(
        python,
        str(python),
        "build.py",
        "--release",
        timeout=DEFAULT_RELEASE_BUILD_TIMEOUT_SECONDS,
    )


def running_on_github_actions() -> bool:
    return os.environ.get(GITHUB_ACTIONS_ENV, "").lower() == "true"


def skip_linux_docker_preflight() -> bool:
    return os.environ.get(SKIP_LINUX_DOCKER_ENV, "").lower() in {
        "1",
        "true",
        "yes",
        "on",
    }


def run(cmd: list[str], extra_env: dict[str, str] | None = None) -> int:
    _, clean_env = load_env_helpers()
    env = clean_env()
    if extra_env:
        env.update(extra_env)
    return subprocess.run(cmd, cwd=ROOT, env=env).returncode


def _find_llvm_profdata() -> Path | None:
    """Locate the rustup toolchain's llvm-profdata (llvm-tools component)."""
    try:
        sysroot = subprocess.run(
            ["rustc", "--print", "sysroot"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None
    exe = "llvm-profdata.exe" if sys.platform == "win32" else "llvm-profdata"
    for candidate in Path(sysroot).glob(f"lib/rustlib/*/bin/{exe}"):
        return candidate
    return None


def describe_abnormal_exit(returncode: int) -> str | None:
    """Describe a process killed by a fault, or ``None`` for a normal exit.

    A crash and a refusal both surface as "nonzero", but they mean opposite
    things: a nonzero exit is the tool reporting a problem with its input, a
    fault is the tool itself being unable to run. Coverage failures have been
    misread as the former when they were the latter (#626).
    """
    # POSIX: Python reports a signal death as the negated signal number.
    if returncode < 0:
        signum = -returncode
        try:
            name = signal.Signals(signum).name
        except ValueError:
            name = "unknown signal"
        return f"terminated by signal {signum} ({name})"

    # Windows: an unhandled exception surfaces as the NTSTATUS code, which is
    # what Python hands back as the return code.
    windows_status = {
        0xC000001D: "STATUS_ILLEGAL_INSTRUCTION",
        0xC0000005: "STATUS_ACCESS_VIOLATION",
        0xC000008C: "STATUS_ARRAY_BOUNDS_EXCEEDED",
        0xC0000094: "STATUS_INTEGER_DIVIDE_BY_ZERO",
        0xC00000FD: "STATUS_STACK_OVERFLOW",
    }
    # Return codes come back signed on Windows; normalize before lookup.
    unsigned = returncode & 0xFFFFFFFF
    if unsigned in windows_status:
        return f"terminated by {windows_status[unsigned]} (0x{unsigned:08X})"
    return None


def _cpu_description() -> str:
    """Best-effort CPU identification, for toolchain-mismatch diagnosis.

    An illegal instruction usually means the binary was built for a newer
    microarchitecture than the runner provides, so the CPU is the first thing
    an investigator needs and the hardest to recover after the fact.
    """
    parts = [platform.machine() or "unknown-arch"]
    if platform.processor():
        parts.append(platform.processor())
    try:
        if sys.platform.startswith("linux"):
            text = Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
            for line in text.splitlines():
                if line.startswith("flags") or line.startswith("Features"):
                    parts.append(line.split(":", 1)[1].strip())
                    break
    except OSError:
        pass
    return " | ".join(parts)


def llvm_profdata_preflight(
    profdata_command: Sequence[str] | None = None,
) -> str | None:
    """Check that ``llvm-profdata`` can run at all; return a diagnostic if not.

    #626: on some runners the pinned toolchain's ``llvm-profdata`` dies with
    SIGILL while merging profiles. That happens *after* the whole suite has
    passed, so a 20-minute run ends with a bare signal number and no
    indication that the toolchain, not the code, is at fault.

    Probing the binary first turns that into a fast, named failure. Only a
    *fault* is treated as disqualifying — a nonzero exit means the tool ran
    and objected to its arguments, which is exactly what a merge with no
    inputs should do.

    Returns ``None`` when the binary is usable, or when it cannot be located
    (coverage will then fail on its own terms rather than on a guess).
    """
    if profdata_command is None:
        profdata = _find_llvm_profdata()
        if profdata is None:
            return None
        profdata_command = [str(profdata)]
    command_prefix = list(profdata_command)

    version_text = ""
    for probe_args in (["--version"], ["merge", "-sparse", "-o", os.devnull]):
        command = [*command_prefix, *probe_args]
        try:
            probe = subprocess.run(command, capture_output=True, text=True)
        except OSError as exc:
            return f"coverage: cannot execute {shlex.join(command)}: {exc}"

        if probe_args == ["--version"]:
            version_text = (probe.stdout or probe.stderr).strip()

        fault = describe_abnormal_exit(probe.returncode)
        if fault is not None:
            return "\n".join(
                [
                    "coverage: llvm-profdata is unusable on this machine "
                    "— refusing to run the suite before failing on it (#626).",
                    f"  command:  {shlex.join(command)}",
                    f"  outcome:  {fault}",
                    f"  binary:   {command_prefix[0]}",
                    f"  version:  {version_text or '<unavailable>'}",
                    f"  cpu:      {_cpu_description()}",
                    f"  os:       {os.environ.get('RUNNER_OS') or platform.platform()}",
                    "  This is a toolchain/CPU incompatibility, not a test or "
                    "coverage failure. Compare against a system llvm-profdata "
                    "or pin a toolchain whose llvm-tools match the runner.",
                ]
            )
    return None


def _prune_invalid_profraw(
    profile_dir: Path,
    *,
    bad_dir: Path | None = None,
    profdata_command: Sequence[str] | None = None,
) -> int:
    """Preserve, document, then remove profiles rejected by llvm-profdata.

    A nonzero probe proves only that the input is unusable by this LLVM
    build. It does not establish truncation or identify the process that
    produced the file. Rejected inputs are copied with a manifest before
    removal so a green coverage run still leaves an upstream reproducer.
    """
    if profdata_command is None:
        profdata = _find_llvm_profdata()
        if profdata is None:
            return 0
        profdata_command = [str(profdata)]
    if not profile_dir.is_dir():
        return 0

    command_prefix = list(profdata_command)
    version_probe = subprocess.run(
        [*command_prefix, "--version"],
        capture_output=True,
        text=True,
    )
    llvm_version = (version_probe.stdout or version_probe.stderr).strip()
    rejected: list[tuple[Path, subprocess.CompletedProcess[str]]] = []
    for profraw in sorted(profile_dir.rglob("*.profraw")):
        probe_command = [*command_prefix, "show", str(profraw)]
        probe = subprocess.run(
            probe_command,
            capture_output=True,
            text=True,
        )
        if probe.returncode != 0:
            rejected.append((profraw, probe))

    if not rejected:
        print("coverage: pruned 0 invalid .profraw file(s)", flush=True)
        return 0

    evidence_dir = bad_dir or ROOT / "logs" / "bad-profraw"
    evidence_dir.mkdir(parents=True, exist_ok=True)
    entries: list[dict[str, object]] = []
    for profraw, probe in rejected:
        relative = profraw.relative_to(profile_dir)
        preserved = evidence_dir / relative
        preserved.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(profraw, preserved)
        command = [*command_prefix, "show", str(profraw)]
        entries.append(
            {
                "filename": profraw.name,
                "original_path": str(profraw),
                "preserved_path": str(preserved.relative_to(evidence_dir)),
                "size_bytes": profraw.stat().st_size,
                "probe_command": command,
                "probe_returncode": probe.returncode,
                "probe_signal": -probe.returncode if probe.returncode < 0 else None,
                "probe_stderr": probe.stderr.strip(),
            }
        )

    manifest = {
        "llvm_version": llvm_version,
        "llvm_version_command": [*command_prefix, "--version"],
        "runner_os": os.environ.get("RUNNER_OS") or platform.platform(),
        "github_run_id": os.environ.get("GITHUB_RUN_ID"),
        "github_commit": os.environ.get("GITHUB_SHA"),
        "rejected_profiles": entries,
    }
    manifest_path = evidence_dir / "manifest.json"
    manifest_path.write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )

    # The copies and their manifest now exist; only at this point is it safe
    # to keep the rejected inputs out of cargo-llvm-cov's merge set.
    for profraw, probe in rejected:
        print(
            f"coverage: preserving and pruning invalid profraw ({probe.returncode}): {profraw}",
            flush=True,
        )
        profraw.unlink()
    print(
        f"coverage: pruned {len(rejected)} invalid .profraw file(s); evidence: {evidence_dir}",
        flush=True,
    )
    return len(rejected)


def run_live(cmd: list[str]) -> int:
    _, clean_env = load_env_helpers()
    env = clean_env()
    env["RUNNING_PROCESS_LIVE_TESTS"] = "1"
    return subprocess.run(cmd, cwd=ROOT, env=env).returncode


def live_tests_enabled() -> bool:
    return os.environ.get("RUNNING_PROCESS_LIVE_TESTS") == "1"


def load_env_helpers():
    from ci.env import activate, clean_env

    return activate, clean_env


def _looks_like_pytest_target(arg: str) -> bool:
    return arg.endswith(".py") or "::" in arg or "/" in arg or "\\" in arg


def _normalize_pytest_args(args: list[str]) -> list[str]:
    if not args:
        return []
    if any(arg.startswith("-") for arg in args):
        return list(args)
    targets: list[str] = []
    selectors: list[str] = []
    collecting_targets = True
    for arg in args:
        if collecting_targets and _looks_like_pytest_target(arg):
            targets.append(arg)
            continue
        collecting_targets = False
        selectors.append(arg)
    normalized = list(targets or args[:1])
    if selectors:
        normalized.extend(["-k", " and ".join(selectors)])
    return normalized


def _pytest_exit_is_acceptable(returncode: int, pytest_args: list[str]) -> bool:
    if returncode == 0:
        return True
    return returncode == 5 and bool(pytest_args)


def _ensure_nextest_installed() -> bool:
    """Ensure `cargo nextest` is on PATH; install it on demand if not.

    Per-test timeouts and process isolation come from cargo-nextest
    plus `.config/nextest.toml`. CI workflows pre-install via
    `taiki-e/install-action`; this fallback covers local `./test` runs
    where the developer hasn't done so yet.
    """
    probe = subprocess.run(
        ["cargo", "nextest", "--version"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if probe.returncode == 0:
        return True
    print(
        "cargo-nextest not found — installing (`cargo install cargo-nextest --locked`)…",
        flush=True,
    )
    install = subprocess.run(
        cargo_command("install", "cargo-nextest", "--locked"),
        cwd=ROOT,
    )
    if install.returncode != 0:
        print(
            "Failed to install cargo-nextest. Install it manually with:\n"
            "  cargo install cargo-nextest --locked\n"
            "or via the taiki-e/install-action GitHub Action.",
            file=sys.stderr,
            flush=True,
        )
        return False
    return True


def parse_args(argv: list[str] | None = None) -> tuple[list[str], bool, bool, bool]:
    argv = list(sys.argv[1:] if argv is None else argv)
    raw_pytest_args: list[str] = []
    require_symbols = False
    coverage = False
    live_only = False
    while argv:
        current = argv.pop(0)
        if current == "--no-skip":
            require_symbols = True
            continue
        if current == "--coverage":
            coverage = True
            continue
        if current == "--live-only":
            live_only = True
            continue
        raw_pytest_args.append(current)
    return _normalize_pytest_args(raw_pytest_args), require_symbols, coverage, live_only


def main(argv: list[str] | None = None) -> int:
    pytest_args, require_symbols, coverage, live_only = parse_args(argv)
    activate, _ = load_env_helpers()
    activate()
    if require_symbols:
        os.environ["RUNNING_PROCESS_REQUIRE_NATIVE_DEBUGGER_SYMBOLS"] = "1"
    if live_only:
        os.environ["RUNNING_PROCESS_LIVE_TESTS"] = "1"
    os.environ.setdefault(
        "RUNNING_PROCESS_TEST_TIMEOUT_SECONDS", DEFAULT_TEST_TIMEOUT_SECONDS
    )
    python = Path(sys.executable)
    if os.environ.get(IN_RUNNING_PROCESS_ENV) != IN_RUNNING_PROCESS_VALUE:
        try:
            ensure_dev_wheel(python, root=ROOT)
        except RuntimeError as exc:
            print(str(exc), file=sys.stderr, flush=True)
            return 1

    # -- Rust tests (with optional coverage via cargo-llvm-cov) --
    #
    # We run via `cargo nextest run` rather than `cargo test`. Two wins:
    #
    # 1. Per-test PROCESS isolation — the pyo3 GIL + PTY deadlock that
    #    forces `--test-threads=1` under cargo test on Windows doesn't
    #    apply because each #[test] runs in its own process.
    # 2. Per-test WALL-CLOCK timeout from `.config/nextest.toml`
    #    (`slow-timeout.terminate-after`). Any test that hangs longer
    #    than the deadline is killed and its captured stdout/stderr
    #    appears in the nextest failure summary — enough for a CI agent
    #    to identify what hung and start fixing it.
    #
    # Build test binaries first WITHOUT the idle-timeout supervisor.
    # Compilation can have long gaps (>10s) with no stdout/stderr when
    # linking large crates (tokio, interprocess, clap, etc.) and the
    # 10-second idle-timeout would kill the process mid-compile.
    if not live_only:
        if not _ensure_nextest_installed():
            return 1

        if coverage:
            # Fail fast on a toolchain that cannot merge profiles at all.
            # Without this the suite runs to completion and only then dies in
            # the merge, reporting a bare signal number (#626).
            unusable = llvm_profdata_preflight()
            if unusable is not None:
                print(unusable, file=sys.stderr, flush=True)
                return 1

            # Fixtures first, for the same reason as the non-coverage path
            # below: tests look the binaries up rather than building them.
            #
            # `--target-dir` is not optional here. cargo-llvm-cov redirects
            # the build into `target/llvm-cov-target`, so the test binary
            # resolves fixtures relative to *that* tree; a plain build would
            # put them in `target/debug` where nothing looks.
            if (
                run(
                    cargo_command(
                        "build",
                        "-p",
                        "testbins",
                        "--target-dir",
                        "target/llvm-cov-target",
                    )
                )
                != 0
            ):
                return 1

            # Split run/report so individually unreadable profiles can be
            # preserved and excluded before the final report. Historical
            # LLVM 21.1.8 inputs have crashed llvm-profdata with SIGILL;
            # pruning is retained as defense in depth while graceful daemon
            # profile flushing soaks on main.
            cargo_cmd = supervised_command(
                python,
                *cargo_command("llvm-cov", "nextest", "--workspace", "--no-report"),
                timeout=DEFAULT_RUST_TEST_TIMEOUT_SECONDS,
            )
            if run(cargo_cmd) != 0:
                return 1

            _prune_invalid_profraw(ROOT / "target" / "llvm-cov-target")

            report_cmd = cargo_command(
                "llvm-cov",
                "report",
                "--lcov",
                "--output-path",
                "coverage-rust.lcov",
            )
            report_code = run(report_cmd)
            if report_code != 0:
                # The preflight clears the binary in isolation; a fault here
                # means real profile data triggered it. Say so, rather than
                # letting a signal number read as a coverage regression.
                fault = describe_abnormal_exit(report_code)
                if fault is not None:
                    print(
                        "\n".join(
                            [
                                f"coverage: the report step {fault} while merging "
                                "real profiles (#626).",
                                f"  command: {shlex.join(report_cmd)}",
                                f"  cpu:     {_cpu_description()}",
                                "  The tests themselves passed. Rejected inputs, if "
                                "any, were preserved under logs/bad-profraw.",
                            ]
                        ),
                        file=sys.stderr,
                        flush=True,
                    )
                return 1
        else:
            # Step 0: build the test fixtures.
            #
            # `testbin_path` in the Rust tests used to run `cargo build -p
            # testbins` itself, once per call. That takes cargo's
            # build-directory lock, and nextest gives each test its own
            # process, so a full-suite run had dozens of cargo invocations
            # queueing on one lock — surfacing as an unexplained 30s+ hang
            # (#747). Building the fixtures once here means the tests only
            # have to look them up.
            if run(cargo_command("build", "-p", "testbins")) != 0:
                return 1

            # The tokio console fixture is a SEPARATE workspace, built with
            # `--cfg tokio_unstable` (#788).
            #
            # `console-subscriber` only functions when the profiled application
            # carries that cfg, and cargo has no per-crate RUSTFLAGS: setting it
            # here for the main workspace would apply it to the published crate
            # and invalidate every build cache. So the fixture is excluded from
            # the workspace and built on its own with the flag set for just this
            # invocation.
            #
            # A failure here is not fatal. The cfg makes tokio recompile from
            # scratch, which is slow, and the tests that use the fixture skip
            # with an explanatory message when it is absent — so a missing
            # fixture costs coverage, not a red suite.
            if (
                run(
                    cargo_command(
                        "build", "--manifest-path", "testbins-tokio/Cargo.toml"
                    ),
                    extra_env={"RUSTFLAGS": "--cfg tokio_unstable"},
                )
                != 0
            ):
                print(
                    "warning: could not build testbins-tokio; the tokio "
                    "console-api tests will skip.",
                    file=sys.stderr,
                )

            # Step 1: compile all test binaries (no supervisor, no timeout)
            build_args = cargo_command("nextest", "run", "--workspace", "--no-run")
            if run(build_args) != 0:
                return 1

            # Step 2: run the pre-built tests under the idle-timeout supervisor.
            # nextest's per-test wall clock comes from .config/nextest.toml.
            cargo_test_args = cargo_command("nextest", "run", "--workspace")
            if sys.platform == "win32":
                # Belt-and-braces: even with process-per-test isolation,
                # filesystem and named-pipe races in the daemon test suite
                # are more reliable under serial execution on Windows.
                cargo_test_args += ["--test-threads", "1"]
            if os.environ.get("RUNNING_PROCESS_TEST_NOCAPTURE"):
                # CI-only: surface println!/eprintln! from Rust tests so
                # hangs and panics leave a usable trail in the GH log.
                cargo_test_args.append("--no-capture")
            rust_test_timeout = (
                WINDOWS_RUST_TEST_TIMEOUT_SECONDS
                if sys.platform == "win32"
                else DEFAULT_RUST_TEST_TIMEOUT_SECONDS
            )
            cargo_cmd = supervised_command(
                python,
                *cargo_test_args,
                timeout=rust_test_timeout,
            )
            if run(cargo_cmd) != 0:
                return 1

            # #433 R4: the RUNNING_PROCESS_FAKE_BACKEND seam is compiled out of
            # the default build (it must never ship in production). Exercise its
            # tests in a dedicated pass with the opt-in `test-seams` feature so
            # the backdoor stays covered without leaking into shipped binaries.
            seam_test_args = cargo_command(
                "nextest",
                "run",
                "-p",
                "running-process",
                "--features",
                "test-seams",
                "--test",
                "broker",
                "-E",
                "test(fake_backend)",
            )
            if sys.platform == "win32":
                seam_test_args += ["--test-threads", "1"]
            seam_cmd = supervised_command(
                python,
                *seam_test_args,
                timeout=rust_test_timeout,
            )
            if run(seam_cmd) != 0:
                return 1

        # -- Python non-live tests --
        cov_first = list(_COV_PYTEST_FIRST) if coverage else []
        if not _pytest_exit_is_acceptable(
            run(
                _supervised_pytest_command(
                    python, "-m", "not live", *cov_first, *pytest_args
                )
            ),
            pytest_args,
        ):
            return 1
        if not running_on_github_actions() and not skip_linux_docker_preflight():
            if run(_linux_unit_test_command(python, *pytest_args)) != 0:
                return 1
        if require_symbols and sys.platform == "win32":
            if run(_release_build_command(python)) != 0:
                return 1

    # -- Python live tests --
    if live_tests_enabled():
        cov_append = list(_COV_PYTEST_APPEND) if coverage else []
        if not _pytest_exit_is_acceptable(
            run_live(
                _supervised_pytest_command(
                    python, "-m", "live", *cov_append, *pytest_args
                )
            ),
            pytest_args,
        ):
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
