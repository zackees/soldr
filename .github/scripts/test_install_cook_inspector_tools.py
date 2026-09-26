"""Tests for install_cook_inspector_tools.py (soldr#3043 cook-timeout
inspector follow-up). No real apt-get/sudo invocation: every `subprocess.run`
call is monkeypatched.
"""

from __future__ import annotations

import types
from pathlib import Path

import pytest
from _script_loader import load_script_module

SCRIPT = Path(__file__).resolve().parent / "install_cook_inspector_tools.py"


@pytest.fixture()
def mod():
    # Not module-scoped: PERF_CANDIDATES is read from `platform.uname()` at
    # import time and several tests monkeypatch `subprocess.run` per-call, so
    # a fresh module per test keeps them independent.
    return load_script_module(SCRIPT, "install_cook_inspector_tools")


def test_run_never_raises_on_a_missing_binary(mod):
    assert mod.run(["/no/such/binary-xyz", "--version"]) is False


def test_run_true_on_success(mod, monkeypatch):
    fake_completed = types.SimpleNamespace(returncode=0)
    monkeypatch.setattr(mod.subprocess, "run", lambda *a, **k: fake_completed)
    assert mod.run(["true"]) is True


def test_run_false_on_nonzero_exit(mod, monkeypatch):
    fake_completed = types.SimpleNamespace(returncode=1)
    monkeypatch.setattr(mod.subprocess, "run", lambda *a, **k: fake_completed)
    assert mod.run(["false"]) is False


def test_run_never_captures_or_swallows_child_streams(mod, monkeypatch):
    """Owner rule: apt's own output must reach the job log directly -- no
    `capture_output`, no `stdout=DEVNULL`/`stderr=DEVNULL`."""
    seen_kwargs = {}
    fake_completed = types.SimpleNamespace(returncode=0)

    def fake_run(argv, **kwargs):
        seen_kwargs.update(kwargs)
        return fake_completed

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    mod.run(["apt-get", "install", "-y", "gdb"])
    assert "capture_output" not in seen_kwargs
    assert seen_kwargs.get("stdout") is None
    assert seen_kwargs.get("stderr") is None


def test_install_best_effort_tries_every_always_package(mod, monkeypatch):
    calls = []

    def fake_run(argv):
        calls.append(argv)
        return True

    monkeypatch.setattr(mod, "run", fake_run)
    mod.install_best_effort()
    installed_packages = {
        argv[-1]
        for argv in calls
        if argv[:3] == ["sudo", "-n", "apt-get"] and "install" in argv
    }
    assert "gdb" in installed_packages
    assert "elfutils" in installed_packages
    assert mod.BPFCC_PACKAGE in installed_packages


def test_install_best_effort_stops_at_the_first_perf_candidate_that_installs(
    mod, monkeypatch
):
    attempted = []

    def fake_run(argv):
        if argv[-1] in mod.PERF_CANDIDATES:
            attempted.append(argv[-1])
            # Only the second candidate "installs".
            return argv[-1] == mod.PERF_CANDIDATES[1]
        return True

    monkeypatch.setattr(mod, "run", fake_run)
    mod.install_best_effort()
    assert attempted == mod.PERF_CANDIDATES[:2]


def test_install_best_effort_never_raises_when_every_package_fails(mod, monkeypatch):
    monkeypatch.setattr(mod, "run", lambda argv: False)
    mod.install_best_effort()  # must not raise


def test_main_always_returns_zero_even_when_everything_fails(mod, monkeypatch):
    monkeypatch.setattr(mod, "run", lambda argv: False)
    assert mod.main() == 0


def test_main_returns_zero_on_full_success(mod, monkeypatch):
    monkeypatch.setattr(mod, "run", lambda argv: True)
    assert mod.main() == 0
