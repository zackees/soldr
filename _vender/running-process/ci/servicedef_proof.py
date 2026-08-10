"""Per-OS servicedef install-path proof (#386).

Proves the `.servicedef` package-install contract on a real runner with
`RUNNING_PROCESS_SERVICE_DEF_DIR` deliberately unset, exercising the
*platform-default* directory end-to-end:

1. `config --effective --json` reports the expected per-OS directory
   with source `platform-default` (Windows additionally asserts the
   literal `AppData\\Roaming\\running-process\\services` suffix so the
   documented `%APPDATA%` contract stays honest).
2. `servicedef install` (postinstall-style CLI) writes a proof
   definition into that directory.
3. `broker doctor --json` reports `servicedef:dir` and the fresh
   `servicedef:<name>` check as PASS.
4. Unix only: the directory is made world-writable and doctor must FAIL
   the `servicedef:dir` check (proves the permission check bites).
5. Cleanup: the proof file/dir is removed so runner reuse cannot leak
   state.

Evidence (install/config/doctor JSON) is written into `logs/` so CI can
upload it as the per-OS acceptance transcript.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
LOGS = ROOT / "logs"

PROOF_SERVICE = "rp-ci-proof"
ENV_OVERRIDE = "RUNNING_PROCESS_SERVICE_DEF_DIR"


def expected_platform_default_dir() -> Path:
    if sys.platform == "win32":
        appdata = os.environ.get("APPDATA")
        if not appdata:
            raise RuntimeError("APPDATA must be set on Windows runners")
        return Path(appdata) / "running-process" / "services"
    if sys.platform == "darwin":
        return (
            Path.home()
            / "Library"
            / "Application Support"
            / "running-process"
            / "services"
        )
    xdg = os.environ.get("XDG_CONFIG_HOME")
    base = Path(xdg) if xdg else Path.home() / ".config"
    return base / "running-process" / "services"


def run_cli(
    binary: Path, *args: str, expect_code: int | None = 0
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        [str(binary), *args],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    print(f"$ {binary.name} {' '.join(args)} -> exit {result.returncode}")
    if expect_code is not None and result.returncode != expect_code:
        sys.stderr.write(result.stdout)
        sys.stderr.write(result.stderr)
        raise RuntimeError(
            f"{binary.name} {' '.join(args)} exited {result.returncode}, "
            f"expected {expect_code}"
        )
    return result


def save_evidence(name: str, content: str) -> None:
    LOGS.mkdir(parents=True, exist_ok=True)
    path = LOGS / name
    path.write_text(content, encoding="utf-8")
    print(f"evidence written: {path}")


def doctor_check_status(report: dict[str, Any], name: str) -> str:
    for check in report["checks"]:
        if check["check"] == name:
            return str(check["status"])
    raise RuntimeError(f"doctor report has no check {name!r}")


def assert_config_reports_platform_default(binary: Path, expected_dir: Path) -> None:
    result = run_cli(binary, "config", "--effective", "--json")
    save_evidence("servicedef-proof-config.json", result.stdout)
    config = json.loads(result.stdout)
    entry = config["values"]["paths"]["service_definition_dir"]
    if entry["source"] != "platform-default":
        raise RuntimeError(
            f"service_definition_dir source is {entry['source']!r}, "
            "expected 'platform-default'"
        )
    reported = Path(entry["value"])
    if reported != expected_dir:
        raise RuntimeError(
            f"service_definition_dir is {reported}, expected {expected_dir}"
        )
    if sys.platform == "win32":
        suffix = r"\AppData\Roaming\running-process\services"
        if not str(reported).endswith(suffix):
            raise RuntimeError(
                f"Windows dir {reported} does not honor the documented "
                f"%APPDATA% contract (expected suffix {suffix})"
            )
    print(f"config reports platform-default dir: {reported}")


def install_proof_definition(binary: Path, expected_dir: Path) -> Path:
    result = run_cli(
        binary,
        "servicedef",
        "install",
        "--service",
        PROOF_SERVICE,
        "--binary-path",
        str(binary.resolve()),
        "--min-version",
        "0.1.0",
        "--json",
    )
    save_evidence("servicedef-proof-install.json", result.stdout)
    payload = json.loads(result.stdout)
    if payload["dir_source"] != "platform-default":
        raise RuntimeError(
            f"install dir_source is {payload['dir_source']!r}, "
            "expected 'platform-default'"
        )
    written = Path(payload["path"])
    if written.parent != expected_dir:
        raise RuntimeError(f"install wrote into {written.parent}, not {expected_dir}")
    if not written.is_file():
        raise RuntimeError(f"install reported {written} but the file does not exist")
    print(f"installed proof servicedef: {written}")
    return written


def assert_doctor_passes(binary: Path) -> None:
    result = run_cli(binary, "doctor", "--json")
    save_evidence("servicedef-proof-doctor.json", result.stdout)
    report = json.loads(result.stdout)
    for check in (
        "servicedef:dir",
        f"servicedef:{PROOF_SERVICE}.servicedef",
    ):
        status = doctor_check_status(report, check)
        if status != "PASS":
            raise RuntimeError(f"doctor check {check!r} is {status}, expected PASS")
    print("doctor: servicedef checks PASS")


def assert_doctor_fails_world_writable(binary: Path, service_dir: Path) -> None:
    if sys.platform == "win32":
        print("skipping world-writable negative probe on Windows")
        return
    service_dir.chmod(0o777)
    try:
        result = run_cli(binary, "doctor", "--json", expect_code=1)
        save_evidence("servicedef-proof-doctor-insecure.json", result.stdout)
        report = json.loads(result.stdout)
        status = doctor_check_status(report, "servicedef:dir")
        if status != "FAIL":
            raise RuntimeError(
                f"doctor servicedef:dir is {status} on a world-writable dir, "
                "expected FAIL"
            )
        print("doctor: world-writable dir correctly FAILs servicedef:dir")
    finally:
        service_dir.chmod(0o700)


def find_default_binary() -> Path:
    """Locate the built broker CLI.

    CI builds land in `target/debug/`; local soldr-routed builds force an
    explicit target triple and land in `target/<triple>/debug/`.
    """
    name = (
        "running-process-broker-v1.exe"
        if sys.platform == "win32"
        else "running-process-broker-v1"
    )
    candidates = [ROOT / "target" / "debug" / name]
    candidates.extend(sorted((ROOT / "target").glob(f"*/debug/{name}")))
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return candidates[0]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--bin",
        type=Path,
        default=None,
        help="Path to the running-process-broker-v1 binary "
        "(default: target/debug/running-process-broker-v1[.exe])",
    )
    args = parser.parse_args()

    binary = args.bin or find_default_binary()
    if not binary.is_file():
        raise RuntimeError(f"broker binary not found at {binary}; build it first")

    if os.environ.get(ENV_OVERRIDE) is not None:
        raise RuntimeError(
            f"{ENV_OVERRIDE} must be unset: this proof exercises the "
            "platform-default directory"
        )

    expected_dir = expected_platform_default_dir()
    if expected_dir.exists():
        raise RuntimeError(
            f"{expected_dir} already exists; refusing to run the proof over "
            "pre-existing service definitions"
        )

    try:
        assert_config_reports_platform_default(binary, expected_dir)
        install_proof_definition(binary, expected_dir)
        assert_doctor_passes(binary)
        assert_doctor_fails_world_writable(binary, expected_dir)
    finally:
        # Leave no state behind for runner reuse.
        shutil.rmtree(expected_dir, ignore_errors=True)

    print("servicedef install-path proof: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
