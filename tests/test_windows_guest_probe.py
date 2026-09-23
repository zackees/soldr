"""Contract tests for the dispatch-only Windows guest feasibility probe."""

import hashlib
import importlib.util
import json
import urllib.request
from argparse import Namespace
from datetime import datetime, timezone
from io import BytesIO
from pathlib import Path
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "ci" / "windows_guest_probe.py"
spec = importlib.util.spec_from_file_location("windows_guest_probe", SCRIPT)
assert spec is not None and spec.loader is not None
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def test_summary_keeps_unmeasured_capabilities_explicit():
    report = probe.make_report(
        host={"kvm": False, "free_bytes": 20_000_000_000},
        timings={},
        guest=None,
        disk={},
    )
    assert report["decision"] == "no-go"
    assert report["reason"] == "KVM is unavailable on this runner"
    assert report["nextest"]["status"] == "not-run"
    assert report["capabilities"]["powershell"] == "not-measured"
    assert report["timings"]["iso_download_seconds"] is None
    assert report["timings"]["runtime_prep_seconds"] is None
    assert report["timings"]["runtime_install_seconds"] is None
    assert report["timings"]["git_prep_seconds"] is None


def test_vc_redist_download_is_pinned_and_rejects_drift(tmp_path, monkeypatch):
    payload = b"test redistributable payload"
    expected = hashlib.sha256(payload).hexdigest()
    monkeypatch.setattr(
        urllib.request, "urlopen", lambda *_args, **_kwargs: BytesIO(payload)
    )
    probe._fetch_vc_redist(tmp_path, expected_sha256=expected)
    assert (tmp_path / "vc_redist.x64.exe").read_bytes() == payload
    try:
        probe._fetch_vc_redist(tmp_path, expected_sha256="0" * 64)
    except OSError as exc:
        assert "sha256" in str(exc).lower()
    else:
        raise AssertionError("redistributable checksum drift must fail")


def test_mingit_download_is_pinned_and_rejects_drift(tmp_path, monkeypatch):
    payload = b"test MinGit archive"
    expected = hashlib.sha256(payload).hexdigest()
    monkeypatch.setattr(
        urllib.request, "urlopen", lambda *_args, **_kwargs: BytesIO(payload)
    )
    probe._fetch_mingit(tmp_path, expected_sha256=expected)
    assert (tmp_path / "mingit.zip").read_bytes() == payload
    try:
        probe._fetch_mingit(tmp_path, expected_sha256="0" * 64)
    except OSError as exc:
        assert "sha256" in str(exc).lower()
    else:
        raise AssertionError("MinGit checksum drift must fail")


def test_summary_requires_real_replay_counts_for_go():
    guest = {
        "nextest": {"run": 8, "passed": 8, "failed": 0, "exit_code": 0},
        "capabilities": {"powershell": "5.1", "admin": True},
    }
    report = probe.make_report(
        host={"kvm": True, "free_bytes": 60_000_000_000},
        timings={
            "iso_download_seconds": 80,
            "install_seconds": 400,
            "boot_seconds": 65,
        },
        guest=guest,
        disk={"image_bytes": 12_000_000_000, "compressed_bytes": 6_000_000_000},
    )
    assert report["decision"] == "go"
    assert report["nextest"]["passed"] == 8
    assert report["timings"]["usable_shell_seconds"] == 545


def test_failed_replay_is_not_a_go():
    guest = {"nextest": {"run": 1, "passed": 0, "failed": 1, "exit_code": 100}}
    report = probe.make_report(
        host={"kvm": True, "free_bytes": 60_000_000_000},
        timings={
            "iso_download_seconds": 80,
            "install_seconds": 400,
            "boot_seconds": 65,
        },
        guest=guest,
        disk={},
    )
    assert report["decision"] == "no-go"


def test_guest_exception_is_named_in_no_go_report():
    report = probe.make_report(
        host={"kvm": True},
        timings={"observed_total_seconds": 510},
        guest={
            "error": "native executable could not start",
            "nextest": {"status": "not-run", "run": 0},
        },
        disk={},
    )
    assert report["decision"] == "no-go"
    assert report["guest_error"] == "native executable could not start"
    assert "native executable could not start" in report["reason"]


def test_dockur_log_timings_do_not_invent_a_boot_boundary():
    log = "\n".join(
        [
            "2026-09-22T00:00:10Z Downloading Windows Server 2025...",
            "2026-09-22T00:01:10Z Extracting Windows Server 2025 image...",
            "2026-09-22T00:02:00Z Booting Windows using QEMU v10...",
        ]
    )
    shell_ready = datetime(2026, 9, 22, 0, 5, tzinfo=timezone.utc).timestamp()
    timings = probe._phase_timings(log, shell_ready - 300, shell_ready)
    assert timings["iso_download_seconds"] == 60
    assert timings["boot_seconds"] is None
    assert timings["install_seconds"] == 180


def test_preflight_reports_no_go_and_removes_its_own_scratch(tmp_path, monkeypatch):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: False)
    scratch = tmp_path / "unique-scratch"
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path / "missing",
        scratch=scratch,
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    report = json.loads(output.read_text())
    assert report["reason"] == "KVM is unavailable on this runner"
    assert not scratch.exists()


def test_root_owned_scratch_parent_is_prepared_for_runner(tmp_path, monkeypatch):
    scratch = tmp_path / "unique-scratch"
    mkdir = Path.mkdir
    calls = []

    def root_owned_mkdir(path, *args, **kwargs):
        if path == scratch:
            raise PermissionError("root-owned /mnt")
        return mkdir(path, *args, **kwargs)

    def fake_run(*command, **_kwargs):
        calls.append(command)
        mkdir(scratch)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(Path, "mkdir", root_owned_mkdir)
    monkeypatch.setattr(probe, "_run", fake_run)
    probe._prepare_scratch(scratch, privileged_root=tmp_path)
    assert scratch.is_dir()
    assert calls == [
        (
            "sudo",
            "install",
            "-d",
            "-o",
            str(probe.os.getuid()),
            "-g",
            str(probe.os.getgid()),
            str(scratch),
        )
    ]


def test_scratch_setup_error_reports_no_go_and_removes_partial_directory(
    tmp_path, monkeypatch
):
    scratch = tmp_path / "scratch"

    def partial_setup(path):
        path.mkdir()
        raise OSError("simulated sudo install failure")

    monkeypatch.setattr(probe, "_prepare_scratch", partial_setup)
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=scratch,
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    report = json.loads(output.read_text())
    assert report["decision"] == "no-go"
    assert "simulated sudo install failure" in report["reason"]
    assert not scratch.exists()


def test_root_owned_guest_storage_gets_scoped_privileged_cleanup(tmp_path, monkeypatch):
    scratch = tmp_path / "soldr-windows-guest-test"
    scratch.mkdir()
    rmtree = probe.shutil.rmtree
    calls = []

    def denied(_path):
        raise PermissionError("root-owned Docker storage")

    def fake_run(*command, **_kwargs):
        calls.append(command)
        rmtree(scratch)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe.shutil, "rmtree", denied)
    monkeypatch.setattr(probe, "_run", fake_run)
    probe._remove_scratch(scratch, "test", privileged_root=tmp_path)
    assert not scratch.exists()
    assert calls == [("sudo", "rm", "-rf", "--", str(scratch))]


def test_privileged_cleanup_refuses_unowned_path(tmp_path, monkeypatch):
    scratch = tmp_path / "unrelated"
    scratch.mkdir()

    def denied(_path):
        raise PermissionError("root-owned")

    monkeypatch.setattr(probe.shutil, "rmtree", denied)

    def unexpected_run(*_args, **_kwargs):
        raise AssertionError("sudo must not remove an unrelated directory")

    monkeypatch.setattr(probe, "_run", unexpected_run)
    try:
        probe._remove_scratch(scratch, "test", privileged_root=tmp_path)
    except PermissionError:
        pass
    else:
        raise AssertionError("unowned scratch path should be rejected")


def test_privileged_cleanup_failure_is_not_hidden(tmp_path, monkeypatch):
    scratch = tmp_path / "soldr-windows-guest-test"
    scratch.mkdir()
    rmtree = probe.shutil.rmtree

    def denied(_path):
        raise PermissionError("root-owned")

    def sudo_denied(*_args, **_kwargs):
        raise OSError("sudo rm denied")

    monkeypatch.setattr(probe.shutil, "rmtree", denied)
    monkeypatch.setattr(probe, "_run", sudo_denied)
    try:
        probe._remove_scratch(scratch, "test", privileged_root=tmp_path)
    except OSError as exc:
        assert "sudo rm denied" in str(exc)
    else:
        raise AssertionError("failed privileged cleanup must be reported")
    finally:
        rmtree(scratch)


def test_scratch_cleanup_failure_is_a_failing_no_go(tmp_path, monkeypatch):
    scratch = tmp_path / "soldr-windows-guest-test"
    output = tmp_path / "result.json"
    monkeypatch.setattr(probe, "_kvm_usable", lambda: False)

    def denied(_path):
        raise PermissionError("root-owned")

    monkeypatch.setattr(probe.shutil, "rmtree", denied)
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=scratch,
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 1
    report = json.loads(output.read_text())
    assert report["decision"] == "no-go"
    assert "cleanup failed" in report["reason"].lower()


def test_shell_and_replay_have_distinct_timestamps(tmp_path, monkeypatch):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe.time, "sleep", lambda _: None)
    clock = iter([100.0, 105.0, 107.0, 110.0])
    monkeypatch.setattr(probe.time, "time", lambda: next(clock))
    marker_times = iter([107.0, 107.5, 108.0, 108.2, 108.7, 110.0])
    monkeypatch.setattr(probe, "_host_created_at", lambda *_: next(marker_times))
    shared = tmp_path / "scratch" / "shared"

    def stage(_repo, _artifact, shared_dir, oem_dir):
        shared_dir.mkdir()
        oem_dir.mkdir()

    monkeypatch.setattr(probe, "_stage_payload", stage)
    monkeypatch.setattr(probe, "_disk_measure", lambda _: {})
    inspections = 0

    def fake_run(*command, **_kwargs):
        nonlocal inspections
        if command[:2] == ("docker", "inspect") and len(command) == 3:
            return SimpleNamespace(stdout="", stderr="No such object", returncode=1)
        if command[:2] == ("docker", "inspect"):
            inspections += 1
            if inspections == 1:
                (shared / "guest-shell-ready.txt").write_text("ready")
            elif inspections == 2:
                (shared / "runtime-install-start.txt").write_text("ready")
            elif inspections == 3:
                (shared / "runtime-ready.txt").write_text("ready")
            elif inspections == 4:
                (shared / "git-install-start.txt").write_text("ready")
            elif inspections == 5:
                (shared / "tools-ready.txt").write_text("ready")
            elif inspections == 6:
                (shared / "guest-result.json").write_text(
                    json.dumps(
                        {
                            "nextest": {
                                "run": 8,
                                "passed": 8,
                                "failed": 0,
                                "exit_code": 0,
                            }
                        }
                    )
                )
            return SimpleNamespace(stdout="true", stderr="", returncode=0)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=tmp_path / "scratch",
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    report = json.loads(output.read_text())
    assert report["timings"]["usable_shell_seconds"] == 2
    assert report["timings"]["runtime_prep_seconds"] == 0.5
    assert report["timings"]["runtime_install_seconds"] == 0.5
    assert report["timings"]["git_prep_seconds"] == 0.5
    assert round(report["timings"]["replay_seconds"], 1) == 1.3
    assert report["decision"] == "go"
    assert (
        json.loads((tmp_path / "windows-guest-raw-result.json").read_text())["nextest"][
            "passed"
        ]
        == 8
    )
    assert not shared.exists()


def test_operational_failure_reports_no_go_and_cleans_unique_container(
    tmp_path, monkeypatch
):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe, "_stage_payload", lambda *_: None)
    calls = []

    def fake_run(*command, **_kwargs):
        calls.append(command)
        if command[:2] == ("docker", "run"):
            raise OSError("simulated Docker launch failure")
        if command[:2] == ("docker", "inspect"):
            return SimpleNamespace(stdout="", stderr="No such object", returncode=1)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=tmp_path / "scratch",
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    report = json.loads(output.read_text())
    assert report["decision"] == "no-go"
    assert "simulated Docker launch failure" in report["reason"]
    assert any(command[:3] == ("docker", "rm", "-f") for command in calls)
    assert not (tmp_path / "scratch").exists()


def test_host_marker_time_uses_file_change_time(tmp_path):
    marker = tmp_path / "ready"
    started = probe.time.time() - 1
    marker.write_text("ready")
    created = probe._host_created_at(marker, started)
    assert created is not None
    assert started <= created <= probe.time.time()


def test_cleanup_failure_is_reported_and_fails_workflow(tmp_path, monkeypatch):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe, "_stage_payload", lambda *_: None)

    def fake_run(*command, **_kwargs):
        if command[:2] == ("docker", "run"):
            raise OSError("launch failed")
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    output = tmp_path / "result.json"
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=tmp_path / "scratch",
        output=output,
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 1
    report = json.loads(output.read_text())
    assert report["decision"] == "no-go"
    assert "cleanup failed" in report["reason"].lower()


def test_log_failure_still_attempts_container_removal(tmp_path, monkeypatch):
    monkeypatch.setattr(probe, "_kvm_usable", lambda: True)
    monkeypatch.setattr(
        probe.shutil, "disk_usage", lambda _: SimpleNamespace(free=60 * 1024**3)
    )
    monkeypatch.setattr(probe, "_stage_payload", lambda *_: None)
    calls = []

    def fake_run(*command, **_kwargs):
        calls.append(command)
        if command[:2] == ("docker", "run"):
            raise OSError("launch failed")
        if command[:2] == ("docker", "logs"):
            raise OSError("log unavailable")
        if command[:2] == ("docker", "inspect"):
            return SimpleNamespace(stdout="", stderr="No such object", returncode=1)
        return SimpleNamespace(stdout="", stderr="", returncode=0)

    monkeypatch.setattr(probe, "_run", fake_run)
    args = Namespace(
        repo=ROOT,
        artifact=tmp_path,
        scratch=tmp_path / "scratch",
        output=tmp_path / "result.json",
        run_id="test",
        timeout_seconds=1,
    )
    assert probe.run_probe(args) == 0
    assert any(command[:3] == ("docker", "rm", "-f") for command in calls)
    assert not (tmp_path / "scratch").exists()


def test_workflow_is_dispatch_only_and_not_a_required_gate():
    workflow = (ROOT / ".github/workflows/windows-guest-probe.yml").read_text()
    assert "workflow_dispatch:" in workflow
    assert "pull_request:" not in workflow
    assert "push:" not in workflow
    assert "schedule:" not in workflow
    assert "_ci-cross-build-linux.yml" in workflow
    assert "windows_guest_probe.py" in workflow
    assert "windows-guest-nextest.log" in workflow
    assert "windows-guest-vc-redist.log" in workflow
    assert "windows-guest-raw-result.json" in workflow


def test_guest_installs_signed_runtime_before_native_replay():
    guest = (ROOT / "ci" / "windows_guest_probe.ps1").read_text()
    assert "Get-AuthenticodeSignature" in guest
    assert "vc_redist.x64.exe" in guest
    assert "runtime-install-start.txt" in guest
    assert "runtime-ready.txt" in guest
    assert guest.index("runtime-install-start.txt") < guest.index("Start-Process")
    assert guest.index("runtime-ready.txt") < guest.index("& $nextest nextest run")


def test_guest_native_stderr_cannot_abort_nextest_replay():
    guest = (ROOT / "ci" / "windows_guest_probe.ps1").read_text()
    assert "# Windows PowerShell 5.1 promotes a native program's stderr" in guest
    assert "'Continue'" in guest
    assert guest.index("$ErrorActionPreference = 'Continue'") < guest.index(
        "& $nextest nextest run"
    )
    assert guest.index("$exitCode = $LASTEXITCODE") < guest.index(
        "$ErrorActionPreference = $savedErrorActionPreference"
    )


def test_guest_stages_mingit_before_full_selected_replay():
    guest = (ROOT / "ci" / "windows_guest_probe.ps1").read_text()
    assert "mingit.zip" in guest
    assert "Expand-Archive" in guest
    assert "cmd\\git.exe" in guest
    assert "tools-ready.txt" in guest
    assert guest.index("Expand-Archive") < guest.index("& $nextest nextest run")
    assert "package(soldr-core)" in guest
    assert "not test(" not in guest
