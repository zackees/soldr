from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

from ci.env import build_env, repo_root

ROOT = repo_root()
DIST = ROOT / "dist"
DEV_BUILD_STATE = DIST / ".running-process-dev-build.json"
SOURCE_PATTERNS = (
    "pyproject.toml",
    "Cargo.toml",
    "Cargo.lock",
    "build.py",
    "ci/*.py",
    "src/**/*.py",
    "crates/**/*.rs",
    "crates/**/*.toml",
)


def os_name() -> str:
    return os.name


def repo_python(root: Path = ROOT) -> Path:
    windows_python = root / ".venv" / "Scripts" / "python.exe"
    posix_python = root / ".venv" / "bin" / "python"
    if os_name() == "nt":
        if windows_python.is_file():
            return windows_python
        if posix_python.is_file():
            return posix_python
        return Path(sys.executable)
    if posix_python.is_file():
        return posix_python
    return Path(sys.executable)


def _fingerprint_files(root: Path = ROOT) -> list[Path]:
    files: dict[str, Path] = {}
    for pattern in SOURCE_PATTERNS:
        for path in root.glob(pattern):
            if path.is_file():
                files[str(path.relative_to(root)).replace("\\", "/")] = path
    return [files[key] for key in sorted(files)]


def source_fingerprint(root: Path = ROOT) -> str:
    digest = hashlib.sha256()
    for path in _fingerprint_files(root):
        relative = str(path.relative_to(root)).replace("\\", "/")
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def _load_state(path: Path = DEV_BUILD_STATE) -> dict[str, str] | None:
    if not path.is_file():
        return None
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        return None
    fingerprint = data.get("fingerprint")
    wheel = data.get("wheel")
    if not isinstance(fingerprint, str) or not isinstance(wheel, str):
        return None
    return {"fingerprint": fingerprint, "wheel": wheel}


def _write_state(fingerprint: str, wheel: Path, path: Path = DEV_BUILD_STATE) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps({"fingerprint": fingerprint, "wheel": wheel.name}, indent=2, sort_keys=True),
        encoding="utf-8",
    )


def _latest_wheel(dist: Path = DIST) -> Path:
    wheels = sorted(dist.glob("running_process-*.whl"), key=lambda path: path.stat().st_mtime)
    if not wheels:
        raise RuntimeError(f"no built wheel found in {dist}")
    return wheels[-1]


def _reinstall_wheel(wheel: Path, *, python: Path, root: Path = ROOT) -> int:
    result = subprocess.run(
        [
            "uv",
            "pip",
            "install",
            "--python",
            str(python),
            "--reinstall",
            "--no-deps",
            str(wheel),
        ],
        cwd=root,
        check=False,
        env=build_env(),
    )
    return int(result.returncode)


def ensure_dev_wheel(python: Path | None = None, *, root: Path = ROOT) -> str:
    target_python = python or repo_python(root)
    fingerprint = source_fingerprint(root)
    state = _load_state(root / "dist" / DEV_BUILD_STATE.name)
    if state is not None and state["fingerprint"] == fingerprint:
        wheel = root / "dist" / state["wheel"]
        if wheel.is_file():
            if _reinstall_wheel(wheel, python=target_python, root=root) != 0:
                raise RuntimeError(f"failed to reinstall cached wheel {wheel}")
            return "reused"

    result = subprocess.run(
        [str(target_python), "build.py", "--dev"],
        cwd=root,
        check=False,
        env=build_env(),
    )
    if result.returncode != 0:
        raise RuntimeError("failed to build dev wheel")
    wheel = _latest_wheel(root / "dist")
    _write_state(fingerprint, wheel, root / "dist" / DEV_BUILD_STATE.name)
    return "built"
