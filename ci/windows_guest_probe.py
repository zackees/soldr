"""Dispatch-only dockur/Windows feasibility probe for soldr#3295.

The Linux cross-build is owned by _ci-cross-build-linux.yml. This helper only
stages its verified archive and native nextest, boots an ephemeral guest, and
turns measurements (including an honest no-go) into a durable summary.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import stat
import subprocess
import tarfile
import time
import urllib.request
import uuid
from datetime import datetime
from pathlib import Path
from typing import Any

# Upstream dockur/windows v6.03, commit f6fcb46958fb9635df49e2af09f7c800b65a89e3.
# Pin the published multi-arch image digest, not the moving tag or stale fork.
IMAGE = "ghcr.io/dockur/windows@sha256:743847e75b776790c059f33ac6654f84727ba36a6d458a61e37cb2b2f043d168"
# Microsoft's documented permalink, pinned to the exact signed package read
# for this one-off probe. A changed permalink payload fails closed for review.
VC_REDIST_URL = "https://aka.ms/vc14/vc_redist.x64.exe"
VC_REDIST_SHA256 = "843068991daaa1f73ad9f6239bce4d0f6a07a51f18c37ea2a867e9beca71295c"
MAX_VC_REDIST_BYTES = 64 * 1024**2
CAPABILITY_NAMES = (
    "powershell",
    "job_objects",
    "conpty",
    "webview2",
    "gpu_rendering",
    "admin",
    "vcruntime140",
    "vcruntime140_after",
)


def make_report(
    *,
    host: dict[str, Any],
    timings: dict[str, Any],
    guest: dict[str, Any] | None,
    disk: dict[str, Any],
) -> dict[str, Any]:
    """Never turn missing evidence into a success or a measured zero."""
    measured = {
        name: timings.get(name)
        for name in (
            "iso_download_seconds",
            "install_seconds",
            "boot_seconds",
            "runtime_prep_seconds",
            "runtime_install_seconds",
            "replay_seconds",
        )
    }
    measured["usable_shell_seconds"] = timings.get("observed_total_seconds")
    phases = (
        measured["iso_download_seconds"],
        measured["install_seconds"],
        measured["boot_seconds"],
    )
    if measured["usable_shell_seconds"] is None and all(
        isinstance(value, (int, float)) for value in phases
    ):
        total = 0.0
        for value in phases:
            if isinstance(value, (int, float)):
                total += value
        measured["usable_shell_seconds"] = total
    nextest = (guest or {}).get("nextest") or {"status": "not-run"}
    capabilities = {
        name: (guest or {}).get("capabilities", {}).get(name, "not-measured")
        for name in CAPABILITY_NAMES
    }
    if host.get("probe_error"):
        reason = f"Probe infrastructure error: {host['probe_error']}"
    elif not host.get("kvm"):
        reason = "KVM is unavailable on this runner"
    elif not guest:
        reason = "Windows did not reach the probe script within the boot budget"
    elif (
        nextest.get("exit_code") != 0
        or nextest.get("run", 0) <= 0
        or nextest.get("failed", 0) != 0
    ):
        reason = "Native nextest replay did not pass a nonempty test set"
    elif measured["usable_shell_seconds"] is None:
        reason = "Install and first-boot timing could not be measured"
    elif measured["usable_shell_seconds"] > 600:
        reason = "Cold boot exceeded the ten-minute nightly budget"
    else:
        reason = "Cold boot and native replay fit the probe budget"
    return {
        "schema_version": 1,
        "decision": (
            "go"
            if reason == "Cold boot and native replay fit the probe budget"
            else "no-go"
        ),
        "reason": reason,
        "host": host,
        "timings": measured,
        "disk": disk,
        "nextest": nextest,
        "capabilities": capabilities,
        "edition": "Windows Server 2025 Core evaluation",
        "image": IMAGE,
    }


def _read_json(path: Path) -> dict[str, Any] | None:
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, ValueError):
        return None
    return data if isinstance(data, dict) else None


def _run(
    *args: str, check: bool = True, capture: bool = False
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, check=check, text=True, capture_output=capture)


def _fetch_vc_redist(shared: Path, *, expected_sha256: str = VC_REDIST_SHA256) -> None:
    """Stage one exact Microsoft runtime installer; never trust a moving URL."""
    destination = shared / "vc_redist.x64.exe"
    digest = hashlib.sha256()
    size = 0
    with urllib.request.urlopen(VC_REDIST_URL, timeout=120) as response:
        with destination.open("wb") as output:
            while chunk := response.read(1024 * 1024):
                size += len(chunk)
                if size > MAX_VC_REDIST_BYTES:
                    raise OSError("Visual C++ Redistributable exceeds 64 MiB limit")
                digest.update(chunk)
                output.write(chunk)
    if digest.hexdigest() != expected_sha256:
        raise OSError(
            f"Visual C++ Redistributable sha256 mismatch: {digest.hexdigest()}"
        )


def _stage_payload(repo: Path, artifact: Path, shared: Path, oem: Path) -> Path:
    archive = artifact / "windows-guest-probe-msvc-tests.tar.zst"
    nextest = artifact / "package" / "tools" / "cargo-nextest.exe"
    if not archive.is_file() or not nextest.is_file():
        raise FileNotFoundError(
            f"cross-build bundle missing archive or native nextest under {artifact}"
        )
    shared.mkdir(parents=True)
    oem.mkdir(parents=True)
    shutil.copy2(archive, shared / "tests.tar.zst")
    shutil.copy2(nextest, shared / "cargo-nextest.exe")
    shutil.copy2(repo / "ci" / "windows_guest_probe.ps1", oem / "probe.ps1")
    shutil.copy2(repo / "ci" / "windows_guest_probe.bat", oem / "install.bat")
    _fetch_vc_redist(shared)
    workspace = shared / "workspace"
    workspace.mkdir()
    # Nextest requires a real workspace manifest at --workspace-remap. Use the
    # same commit as the cross-build, with no target/, .git/, or local leftovers.
    with subprocess.Popen(
        ("git", "-C", str(repo), "archive", "--format=tar", "HEAD"),
        stdout=subprocess.PIPE,
    ) as process:
        assert process.stdout is not None
        with tarfile.open(fileobj=process.stdout, mode="r|") as source:
            source.extractall(workspace, filter="data")
        if process.wait() != 0:
            raise OSError("git archive failed")
    return archive


def _disk_measure(storage: Path) -> dict[str, Any]:
    images = [
        path
        for path in storage.rglob("*")
        if path.is_file() and path.suffix in (".img", ".qcow2", ".raw")
    ]
    if not images:
        return {
            "image_bytes": None,
            "compressed_bytes": None,
            "restore_seconds": None,
            "cache_viable": "not-measured",
        }
    image = max(images, key=lambda path: path.stat().st_size)
    # Measure a compressed stream, without creating a multi-GB cache artifact.
    with subprocess.Popen(
        ("zstd", "-q", "-T0", "-3", "-c", str(image)), stdout=subprocess.PIPE
    ) as zstd:
        assert zstd.stdout is not None
        compressed_count = 0
        while chunk := zstd.stdout.read(1024 * 1024):
            compressed_count += len(chunk)
        compressed = compressed_count if zstd.wait() == 0 else None
    return {
        "image_bytes": image.stat().st_size,
        "allocated_bytes": image.stat().st_blocks * 512,
        "compressed_bytes": compressed,
        "restore_seconds": None,
        "cache_viable": "unproven" if compressed is not None else "not-measured",
    }


def _phase_timings(
    log: str, started: float, shell_ready: float | None
) -> dict[str, float | None]:
    """Use dockur's own phase announcements; report absent boundaries as unknown."""
    events: dict[str, float] = {}
    for line in log.splitlines():
        try:
            timestamp, message = line.split(" ", 1)
            when = datetime.fromisoformat(timestamp.replace("Z", "+00:00")).timestamp()
        except ValueError:
            continue
        lowered = message.lower()
        if "downloading" in lowered and ("windows" in lowered or "server" in lowered):
            events.setdefault("download_start", when)
        if "extracting" in lowered and "image" in lowered:
            events.setdefault("download_end", when)
        if "booting windows" in lowered and "qemu" in lowered:
            events.setdefault("install_start", when)
    download_end = events.get("download_end")
    install_start = events.get("install_start")
    return {
        "iso_download_seconds": (
            max(0, download_end - events["download_start"])
            if download_end and "download_start" in events
            else None
        ),
        # The OEM hook runs on the first usable boot. Dockur exposes no
        # trustworthy boundary between unattended install and that boot, so
        # report the combined interval rather than invent two measurements.
        "install_seconds": (
            max(0, shell_ready - install_start)
            if shell_ready and install_start
            else None
        ),
        "boot_seconds": None,
        "observed_total_seconds": (
            max(0, shell_ready - started) if shell_ready else None
        ),
    }


def _write_summary(report: dict[str, Any], path: Path) -> None:
    lines = [
        "## Windows guest probe (#3295)",
        "",
        f"**{report['decision'].upper()}** — {report['reason']}",
        "",
    ]
    lines.extend(["| Measure | Result |", "|---|---|"])
    for key, value in report["timings"].items():
        lines.append(f"| {key} | {value if value is not None else 'not measured'} |")
    for key, value in report["disk"].items():
        lines.append(
            f"| disk.{key} | {value if value is not None else 'not measured'} |"
        )
    for key, value in report["nextest"].items():
        lines.append(f"| nextest.{key} | {value} |")
    for key, value in report["capabilities"].items():
        lines.append(f"| {key} | {value} |")
    lines.append("")
    lines.append(
        "This is one ephemeral evaluation boot, not a CI gate or a reusable activated image."
    )
    lines.append(
        "Dockur exposes no reliable install-versus-first-boot boundary; "
        "`install_seconds` is their combined interval, and `boot_seconds` is explicitly unmeasured."
    )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def _kvm_usable() -> bool:
    device = Path("/dev/kvm")
    try:
        if not stat.S_ISCHR(device.stat().st_mode):
            return False
    except OSError:
        return False
    if not os.access(device, os.R_OK | os.W_OK):
        # The stock Ubuntu runner's KVM node may be root-only. The macOS
        # Recovery action enables the same device for its QEMU container.
        _run("sudo", "chmod", "0666", str(device), check=False, capture=True)
    return os.access(device, os.R_OK | os.W_OK)


def _host_created_at(path: Path, started: float) -> float | None:
    """Read host-kernel ctime, not a guest clock or delayed poll timestamp."""
    try:
        created = path.stat().st_ctime
    except OSError:
        return None
    return created if started <= created <= time.time() + 1 else None


def _prepare_scratch(scratch: Path, *, privileged_root: Path = Path("/mnt")) -> None:
    """Create the run-owned scratch directory on the runner's large /mnt disk."""
    try:
        scratch.mkdir(parents=True)
    except PermissionError as exc:
        if scratch.parent != privileged_root:
            raise
        # GitHub's Ubuntu runner exposes the large /mnt filesystem but keeps
        # its top level root-owned. Grant only this unique run directory to the
        # runner, so normal cleanup can remove every guest byte afterward.
        _run(
            "sudo",
            "install",
            "-d",
            "-o",
            str(os.getuid()),
            "-g",
            str(os.getgid()),
            str(scratch),
        )
        if not scratch.is_dir() or not os.access(scratch, os.W_OK):
            raise PermissionError(
                f"scratch is not writable after setup: {scratch}"
            ) from exc


def _remove_scratch(
    scratch: Path, run_id: str, *, privileged_root: Path = Path("/mnt")
) -> None:
    """Remove only this run's scratch, even if Docker made children root-owned."""
    try:
        shutil.rmtree(scratch)
    except PermissionError as exc:
        if scratch.parent != privileged_root or scratch.name != (
            f"soldr-windows-guest-{run_id}"
        ):
            raise
        _run("sudo", "rm", "-rf", "--", str(scratch))
        if scratch.exists():
            raise OSError(
                f"scratch still exists after privileged cleanup: {scratch}"
            ) from exc


def run_probe(args: argparse.Namespace) -> int:
    repo, artifact, scratch = (
        Path(args.repo).resolve(),
        Path(args.artifact).resolve(),
        Path(args.scratch).resolve(),
    )
    output = Path(args.output).resolve()
    if scratch.exists():
        raise FileExistsError(
            f"refusing to remove pre-existing scratch path: {scratch}"
        )
    if scratch == output or scratch in output.parents:
        raise ValueError("output must live outside ephemeral guest scratch")
    output.parent.mkdir(parents=True, exist_ok=True)
    host: dict[str, Any] = {"kvm": None, "free_bytes": None}
    timings: dict[str, Any] = {}
    guest = None
    disk: dict[str, Any] = {}
    name = f"soldr-windows-probe-{args.run_id}-{uuid.uuid4().hex[:12]}"
    started = time.time()
    shell_ready = None
    runtime_install_start = None
    runtime_ready = None
    result_seen = None
    attempted_container = False
    try:
        _prepare_scratch(scratch)
        host["kvm"] = _kvm_usable()
        host["free_bytes"] = shutil.disk_usage(scratch).free
        if host["kvm"] and host["free_bytes"] >= 32 * 1024**3:
            shared, oem, storage = (
                scratch / "shared",
                scratch / "oem",
                scratch / "storage",
            )
            storage.mkdir()
            _stage_payload(repo, artifact, shared, oem)
            started = time.time()
            attempted_container = True
            _run(
                "docker",
                "run",
                "-d",
                "--name",
                name,
                "--device=/dev/kvm",
                "--device=/dev/net/tun",
                "--cap-add=NET_ADMIN",
                "--stop-timeout=120",
                "-e",
                "VERSION=2025",
                "-e",
                "EDITION=core",
                "-e",
                "DISK_SIZE=32G",
                "-e",
                "RAM_SIZE=8G",
                "-e",
                "CPU_CORES=4",
                "-e",
                "LOG=Y",
                "-v",
                f"{storage}:/storage",
                "-v",
                f"{shared}:/shared",
                "-v",
                f"{oem}:/oem",
                IMAGE,
            )
            deadline = time.monotonic() + args.timeout_seconds
            while time.monotonic() < deadline:
                if shell_ready is None and (shared / "guest-shell-ready.txt").is_file():
                    shell_ready = _host_created_at(
                        shared / "guest-shell-ready.txt", started
                    )
                if (
                    runtime_install_start is None
                    and (shared / "runtime-install-start.txt").is_file()
                ):
                    runtime_install_start = _host_created_at(
                        shared / "runtime-install-start.txt", started
                    )
                if runtime_ready is None and (shared / "runtime-ready.txt").is_file():
                    runtime_ready = _host_created_at(
                        shared / "runtime-ready.txt", started
                    )
                if (shared / "guest-result.json").is_file():
                    result_seen = _host_created_at(
                        shared / "guest-result.json", started
                    )
                    guest = _read_json(shared / "guest-result.json")
                    break
                state = _run(
                    "docker",
                    "inspect",
                    "--format",
                    "{{.State.Running}}",
                    name,
                    capture=True,
                )
                if state.stdout.strip() != "true":
                    break
                time.sleep(10)
            _run("docker", "stop", "--time", "120", name, check=False, capture=True)
            log = _run(
                "docker", "logs", "--timestamps", name, check=False, capture=True
            )
            (output.parent / "windows-guest-container.log").write_text(
                log.stdout + log.stderr, encoding="utf-8"
            )
            timings = _phase_timings(log.stdout + log.stderr, started, shell_ready)
            timings["runtime_prep_seconds"] = (
                max(0, runtime_install_start - shell_ready)
                if runtime_install_start is not None and shell_ready is not None
                else None
            )
            timings["runtime_install_seconds"] = (
                max(0, runtime_ready - runtime_install_start)
                if runtime_ready is not None and runtime_install_start is not None
                else None
            )
            timings["replay_seconds"] = (
                max(0, result_seen - runtime_ready)
                if result_seen is not None and runtime_ready is not None
                else None
            )
            disk = _disk_measure(storage)
        else:
            host["preflight"] = (
                "insufficient disk (<32 GiB)" if host["kvm"] else "KVM unavailable"
            )
    except (OSError, subprocess.CalledProcessError, tarfile.TarError) as exc:
        host["probe_error"] = str(exc)[:500]
    finally:
        for guest_name, host_name in (
            ("nextest.log", "windows-guest-nextest.log"),
            ("vc-redist-install.log", "windows-guest-vc-redist.log"),
        ):
            guest_log = scratch / "shared" / guest_name
            try:
                if guest_log.is_file():
                    shutil.copy2(guest_log, output.parent / host_name)
            except OSError as exc:
                host["diagnostic_error"] = str(exc)[:500]
        if attempted_container:
            try:
                if not (output.parent / "windows-guest-container.log").is_file():
                    log = _run(
                        "docker",
                        "logs",
                        "--timestamps",
                        name,
                        check=False,
                        capture=True,
                    )
                    (output.parent / "windows-guest-container.log").write_text(
                        log.stdout + log.stderr, encoding="utf-8"
                    )
            except OSError as exc:
                host["diagnostic_error"] = str(exc)[:500]
            try:
                # The random suffix makes this name exclusive to this run,
                # including when `docker run` created a container then failed.
                removal = _run("docker", "rm", "-f", name, check=False, capture=True)
                still_there = _run("docker", "inspect", name, check=False, capture=True)
                if (
                    still_there.returncode == 0
                    or ("No such object" not in still_there.stderr)
                    or (
                        removal.returncode != 0
                        and "No such container" not in removal.stderr
                    )
                ):
                    host["cleanup_error"] = f"container {name} removal unverified"
            except OSError as exc:
                host["cleanup_error"] = str(exc)[:500]
        # All multi-GB guest state is ephemeral and never enters the Actions cache.
        try:
            if scratch.exists():
                _remove_scratch(scratch, str(args.run_id))
        except (OSError, subprocess.CalledProcessError) as exc:
            host["cleanup_error"] = str(exc)[:500]
    report = make_report(host=host, timings=timings, guest=guest, disk=disk)
    if host.get("preflight") == "insufficient disk (<32 GiB)":
        report["decision"] = "no-go"
        report["reason"] = "Runner scratch has less than dockur's 32 GiB minimum"
    if host.get("cleanup_error"):
        report["decision"] = "no-go"
        report["reason"] = f"Guest cleanup failed: {host['cleanup_error']}"
    output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    _write_summary(report, output.with_suffix(".md"))
    if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(summary_path).open("a", encoding="utf-8") as summary:
            summary.write(output.with_suffix(".md").read_text(encoding="utf-8"))
    return 1 if host.get("cleanup_error") else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--artifact", required=True)
    parser.add_argument("--scratch", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--timeout-seconds", type=int, default=3600)
    return run_probe(parser.parse_args())


if __name__ == "__main__":
    raise SystemExit(main())
