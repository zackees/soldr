"""Tests for run_stable_cook.py (soldr#3043 Phase 2).

No subprocess: `main()` takes an optional `runner=` seam (a callable
`(command, cwd) -> subprocess.CompletedProcess[str]`) so these tests exercise
the real argv construction, classification, and exit-code logic without ever
invoking a real `soldr` binary.
"""

from __future__ import annotations

import io
import subprocess
import sys
import time
from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "run_stable_cook.py"


@pytest.fixture(scope="module")
def mod():
    return load_script_module(SCRIPT, "run_stable_cook")


def _runner(stdout: str, stderr: str, returncode: int = 0):
    """A fake `Runner` that returns a fixed result and never shells out."""

    def run(command, cwd):
        return subprocess.CompletedProcess(
            args=command, returncode=returncode, stdout=stdout, stderr=stderr
        )

    return run


# --- classify ---------------------------------------------------------------


def test_classify_hydrated(mod):
    stderr = "soldr cook: auto-hydrate activated\nsome other line\n"
    assert mod.classify(stderr) == ("hydrated", "")


def test_classify_warm_skip(mod):
    stderr = (
        "soldr cook: warm-cook detected (recipe + rustc match the prior cook "
        "marker at target/.soldr-cook-marker.json) — skipping Phase 2 "
        "(cargo chef cook). See soldr#621.\n"
    )
    assert mod.classify(stderr) == ("warm-skip", "")


def test_classify_restore_declined_echoes_the_reason_field(mod):
    stderr = (
        "soldr cook: decision=skip  size_bytes=123  estimated_transport_ms=45  "
        "compile_elapsed_ms=6789  reason=estimated transport is not cheaper "
        "than the avoided compile\n"
    )
    outcome, detail = mod.classify(stderr)
    assert outcome == "restore-declined"
    assert detail == "estimated transport is not cheaper than the avoided compile"


def test_classify_built_is_the_fallback(mod):
    assert mod.classify("Compiling soldr-core v0.1.0\n") == ("built", "")


def test_classify_hydrated_wins_even_with_other_noise_in_stderr(mod):
    stderr = "warning: unused import\nsoldr cook: auto-hydrate activated\n"
    assert mod.classify(stderr) == ("hydrated", "")


# --- argv builder -------------------------------------------------------------


def test_build_argv_appends_chef_args_after_double_dash(mod):
    argv = mod.build_argv("/opt/soldr", "x86_64-unknown-linux-gnu", ["--all-targets"])
    assert argv == [
        "/opt/soldr",
        "cook",
        "--workspace",
        "--target",
        "x86_64-unknown-linux-gnu",
        "--",
        "--all-targets",
    ]


def test_build_argv_forwards_every_chef_arg_in_order(mod):
    argv = mod.build_argv("/opt/soldr", "T", ["--all-targets", "--profile", "dev"])
    assert argv[argv.index("--") + 1 :] == ["--all-targets", "--profile", "dev"]


def test_build_argv_with_no_chef_args_still_has_a_trailing_double_dash(mod):
    argv = mod.build_argv("/opt/soldr", "T", [])
    assert argv == ["/opt/soldr", "cook", "--workspace", "--target", "T", "--"]


# --- cook_archive_bytes -------------------------------------------------------


def test_cook_archive_bytes_sums_tar_zst_only(mod, tmp_path):
    cook_dir = tmp_path / "cache" / "cook"
    cook_dir.mkdir(parents=True)
    (cook_dir / "a.tar.zst").write_bytes(b"0" * 100)
    (cook_dir / "b.tar.zst").write_bytes(b"0" * 50)
    (cook_dir / "notes.txt").write_bytes(b"0" * 999)
    assert mod.cook_archive_bytes(tmp_path) == 150


def test_cook_archive_bytes_skips_the_tmp_staging_dir(mod, tmp_path):
    cook_dir = tmp_path / "cache" / "cook"
    tmp_dir = cook_dir / ".tmp"
    tmp_dir.mkdir(parents=True)
    (cook_dir / "a.tar.zst").write_bytes(b"0" * 100)
    (tmp_dir / "staging.tar.zst").write_bytes(b"0" * 5000)
    assert mod.cook_archive_bytes(tmp_path) == 100


def test_cook_archive_bytes_with_a_missing_cook_dir_is_zero(mod, tmp_path):
    assert mod.cook_archive_bytes(tmp_path / "nowhere") == 0


def test_cook_archive_bytes_recurses_into_sha_subdirectories(mod, tmp_path):
    # Not a real shape today, but the sum must not assume a flat layout.
    nested = tmp_path / "cache" / "cook" / "ab"
    nested.mkdir(parents=True)
    (nested / "c.tar.zst").write_bytes(b"0" * 30)
    assert mod.cook_archive_bytes(tmp_path) == 30


# --- main: exit-code triage ---------------------------------------------------


def test_main_returns_zero_on_a_clean_run(mod):
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T"],
        runner=_runner("", "soldr cook: auto-hydrate activated\n"),
    )
    assert status == 0


def test_main_propagates_the_uncookable_workspace_exit_code(mod):
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T"],
        runner=_runner(
            "", "soldr cook: skipped - workspace depends on...\n", returncode=3
        ),
    )
    assert status == 3


def test_main_propagates_other_nonzero_exit_codes_verbatim(mod):
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T"],
        runner=_runner("", "boom\n", returncode=7),
    )
    assert status == 7


# --- main: --require-warm ------------------------------------------------------


def test_main_default_does_not_fail_on_a_built_outcome(mod):
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T"],
        runner=_runner("", "Compiling foo v0.1.0\n"),
    )
    assert status == 0


def test_main_require_warm_fails_a_built_outcome(mod):
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T", "--require-warm"],
        runner=_runner("", "Compiling foo v0.1.0\n"),
    )
    assert status == 4


def test_main_require_warm_passes_a_hydrated_outcome(mod):
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T", "--require-warm"],
        runner=_runner("", "soldr cook: auto-hydrate activated\n"),
    )
    assert status == 0


def test_main_require_warm_passes_a_warm_skip_outcome(mod):
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T", "--require-warm"],
        runner=_runner("", "soldr cook: warm-cook detected (...)\n"),
    )
    assert status == 0


def test_main_require_warm_fails_a_restore_declined_outcome(mod):
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T", "--require-warm"],
        runner=_runner("", "soldr cook: decision=skip  reason=no prior data\n"),
    )
    assert status == 4


# --- main: default chef args and forwarding ------------------------------------


def test_main_forwards_the_default_chef_args_when_none_given(mod):
    seen = {}

    def run(command, cwd):
        seen["command"] = command
        return subprocess.CompletedProcess(
            args=command,
            returncode=0,
            stdout="",
            stderr="soldr cook: auto-hydrate activated\n",
        )

    mod.main(["--soldr", "/opt/soldr", "--target", "T"], runner=run)
    tail = seen["command"][seen["command"].index("--") + 1 :]
    assert tail == ["--all-targets"]


def test_main_forwards_repeated_chef_args_in_order(mod):
    seen = {}

    def run(command, cwd):
        seen["command"] = command
        return subprocess.CompletedProcess(
            args=command,
            returncode=0,
            stdout="",
            stderr="soldr cook: auto-hydrate activated\n",
        )

    mod.main(
        [
            "--soldr",
            "/opt/soldr",
            "--target",
            "T",
            # argparse cannot consume a flag-shaped value as a separate
            # token after `--chef-arg` (it looks like another option), so
            # a chef arg starting with `-` must use the `--chef-arg=...`
            # form.
            "--chef-arg=--all-targets",
            "--chef-arg=--locked",
        ],
        runner=run,
    )
    tail = seen["command"][seen["command"].index("--") + 1 :]
    assert tail == ["--all-targets", "--locked"]


def test_main_runs_from_the_repository_root(mod):
    seen = {}

    def run(command, cwd):
        seen["cwd"] = cwd
        return subprocess.CompletedProcess(
            args=command,
            returncode=0,
            stdout="",
            stderr="soldr cook: auto-hydrate activated\n",
        )

    mod.main(["--soldr", "/opt/soldr", "--target", "T"], runner=run)
    repo_root = SCRIPT.resolve().parents[2]
    assert seen["cwd"] == repo_root
    assert (repo_root / "Cargo.toml").is_file()


# --- main: reporting -----------------------------------------------------------


def test_main_reports_the_cook_archive_size_in_mib(mod, tmp_path, capsys):
    cook_dir = tmp_path / "cache" / "cook"
    cook_dir.mkdir(parents=True)
    (cook_dir / "a.tar.zst").write_bytes(b"0" * (2 * 1024 * 1024))
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T", "--cache-dir", str(tmp_path)],
        runner=_runner("", "soldr cook: auto-hydrate activated\n"),
    )
    assert status == 0
    captured = capsys.readouterr()
    assert "2.0 MiB" in captured.out
    assert "soldr#3047" in captured.out


def test_main_without_cache_dir_reports_no_archive_size(mod, capsys):
    mod.main(
        ["--soldr", "/opt/soldr", "--target", "T"],
        runner=_runner("", "soldr cook: auto-hydrate activated\n"),
    )
    captured = capsys.readouterr()
    assert "MiB" not in captured.out


def test_main_writes_to_the_github_step_summary_when_set(mod, tmp_path, monkeypatch):
    summary = tmp_path / "summary.md"
    monkeypatch.setenv("GITHUB_STEP_SUMMARY", str(summary))
    mod.main(
        ["--soldr", "/opt/soldr", "--target", "T"],
        runner=_runner("", "soldr cook: auto-hydrate activated\n"),
    )
    text = summary.read_text(encoding="utf-8")
    assert "cook[T]: outcome=hydrated" in text


def test_main_echoes_cook_stdout_and_stderr(mod, capsys):
    mod.main(
        ["--soldr", "/opt/soldr", "--target", "T"],
        runner=_runner(
            "cargo-chef stdout line\n", "soldr cook: auto-hydrate activated\n"
        ),
    )
    captured = capsys.readouterr()
    assert "cargo-chef stdout line" in captured.out
    assert "soldr cook: auto-hydrate activated" in captured.err


# --- soldr#3117: built-but-unindexed archives -----------------------------------


def test_classify_names_an_unindexed_archive(mod):
    stderr = (
        "soldr: cache 565 HIT, 0 MISS\n"
        "soldr cook: warning: CookRecord to daemon failed: NotRunning. "
        "Artifact written at /x/cache/cook/abc.tar.zst but not indexed.\n"
        "soldr cook: deps built; recipe was ephemeral\n"
    )
    assert mod.classify(stderr) == ("built-unindexed", "")


def test_classify_indexed_build_is_still_built(mod):
    stderr = "soldr cook: indexed  sha256=abc size=1 MiB\nsoldr cook: deps built\n"
    assert mod.classify(stderr) == ("built", "")


def test_main_fails_an_unindexed_archive_without_require_warm(mod, capsys):
    stderr = (
        "soldr cook: warning: CookRecord to daemon failed: NotRunning. "
        "Artifact written at /a but not indexed.\n"
    )
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T"],
        runner=_runner("", stderr),
    )
    assert status == mod.COOK_ARTIFACT_NOT_INDEXED == 5
    out = capsys.readouterr().out
    assert "::error title=soldr cook::COOK_ARTIFACT_NOT_INDEXED" in out
    assert "outcome=built-unindexed" in out


def test_main_unindexed_exit_code_is_distinct_from_require_warm(mod):
    stderr = "soldr cook: warning: CookRecord to daemon failed: NotRunning.\n"
    status = mod.main(
        ["--soldr", "/opt/soldr", "--target", "T", "--require-warm"],
        runner=_runner("", stderr),
    )
    assert status == mod.COOK_ARTIFACT_NOT_INDEXED
    assert mod.COOK_ARTIFACT_NOT_INDEXED != mod.REQUIRE_WARM_FAILURE


# --- streaming + timeout (soldr#3043 CI-silence follow-up) --------------------


def test_stream_and_capture_writes_output_before_process_exit(mod, tmp_path):
    """Output must be visible as it arrives, not only after exit."""
    script = tmp_path / "slow_child.py"
    script.write_text(
        "import sys, time\n"
        "print('first line', flush=True)\n"
        "sys.stdout.flush()\n"
        "time.sleep(60)\n"
        "print('second line', flush=True)\n",
        encoding="utf-8",
    )

    out_sink = io.StringIO()
    err_sink = io.StringIO()
    seen_before_exit = {}

    def watcher():
        # Poll the sink for up to a few seconds; the child sleeps 60s, so if
        # the first line is visible here the process has clearly not exited.
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if "first line" in out_sink.getvalue():
                seen_before_exit["value"] = out_sink.getvalue()
                return
            time.sleep(0.05)

    import threading

    watcher_thread = threading.Thread(target=watcher)
    watcher_thread.start()

    # The child would sleep well past this timeout; use a small timeout so
    # the test itself stays fast while stream_and_capture's own machinery
    # (streaming + timeout enforcement) both get exercised.
    result = mod.stream_and_capture(
        [sys.executable, str(script)],
        tmp_path,
        timeout_secs=1.0,
        stdout_sink=out_sink,
        stderr_sink=err_sink,
    )
    watcher_thread.join(timeout=5)

    assert "value" in seen_before_exit, "first line never became visible before exit"
    assert "first line" in out_sink.getvalue()
    assert result.returncode == mod.TIMEOUT_EXIT_CODE


def test_stream_and_capture_returns_captured_text_for_classify(mod, tmp_path):
    script = tmp_path / "quick_child.py"
    script.write_text(
        "print('stdout line')\n"
        "import sys; print('soldr cook: auto-hydrate activated', file=sys.stderr)\n",
        encoding="utf-8",
    )
    out_sink = io.StringIO()
    err_sink = io.StringIO()
    result = mod.stream_and_capture(
        [sys.executable, str(script)],
        tmp_path,
        timeout_secs=30,
        stdout_sink=out_sink,
        stderr_sink=err_sink,
    )
    assert result.returncode == 0
    assert "stdout line" in result.stdout
    assert mod.classify(result.stderr) == ("hydrated", "")


def test_stream_and_capture_kills_a_hung_process_on_timeout(mod, tmp_path):
    script = tmp_path / "hang.py"
    script.write_text("import time\ntime.sleep(600)\n", encoding="utf-8")
    started = time.monotonic()
    result = mod.stream_and_capture(
        [sys.executable, str(script)],
        tmp_path,
        timeout_secs=1.0,
        stdout_sink=io.StringIO(),
        stderr_sink=io.StringIO(),
    )
    elapsed = time.monotonic() - started
    assert result.returncode == mod.TIMEOUT_EXIT_CODE
    # Bounded by the timeout plus grace periods, nowhere near the 600s sleep.
    assert elapsed < 90


def test_main_default_runner_is_stream_and_capture(mod, tmp_path, monkeypatch):
    """`main()` with no injected `runner=` must go through the streaming
    default path (production behavior), not the old capture-only default."""
    calls = {}

    def fake_stream_and_capture(command, cwd, timeout_secs, **kwargs):
        calls["command"] = command
        calls["timeout_secs"] = timeout_secs
        return subprocess.CompletedProcess(
            args=command,
            returncode=0,
            stdout="",
            stderr="soldr cook: auto-hydrate activated\n",
        )

    monkeypatch.setattr(mod, "stream_and_capture", fake_stream_and_capture)
    status = mod.main(["--soldr", "/opt/soldr", "--target", "T"])
    assert status == 0
    assert calls["command"][0] == "/opt/soldr"
    assert calls["timeout_secs"] == mod.DEFAULT_TIMEOUT_SECS


def test_main_forwards_custom_timeout_secs(mod, monkeypatch):
    calls = {}

    def fake_stream_and_capture(command, cwd, timeout_secs, **kwargs):
        calls["timeout_secs"] = timeout_secs
        return subprocess.CompletedProcess(
            args=command,
            returncode=0,
            stdout="",
            stderr="soldr cook: auto-hydrate activated\n",
        )

    monkeypatch.setattr(mod, "stream_and_capture", fake_stream_and_capture)
    mod.main(
        ["--soldr", "/opt/soldr", "--target", "T", "--timeout-secs", "120"]
    )
    assert calls["timeout_secs"] == 120.0


def test_main_does_not_double_echo_when_using_the_default_streaming_runner(
    mod, monkeypatch, capsys
):
    def fake_stream_and_capture(command, cwd, timeout_secs, **kwargs):
        return subprocess.CompletedProcess(
            args=command,
            returncode=0,
            stdout="already printed live\n",
            stderr="soldr cook: auto-hydrate activated\n",
        )

    monkeypatch.setattr(mod, "stream_and_capture", fake_stream_and_capture)
    mod.main(["--soldr", "/opt/soldr", "--target", "T"])
    captured = capsys.readouterr()
    assert "already printed live" not in captured.out


def test_workflow_restores_a_post_fix_cook_cache_generation():
    """v1 entries were saved with an archive but no index row (soldr#3117)."""
    workflow = (SCRIPT.parents[1] / "workflows" / "_build-and-test.yml").read_text(
        encoding="utf-8"
    )
    assert "key: stable-cook-v2-${{ inputs.target }}-" in workflow
    assert "stable-cook-v1-" not in workflow
