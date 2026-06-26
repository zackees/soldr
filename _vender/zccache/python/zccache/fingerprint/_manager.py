from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Callable, Optional

from zccache.fingerprint._result import FingerprintResult

_DEFAULT_SKIP_DIR_NAMES: frozenset[str] = frozenset(
    [".git", "target", "node_modules", "__pycache__", ".mypy_cache", "build"]
)
_CPP_SOURCE_EXTS: frozenset[str] = frozenset([".cpp", ".h", ".hpp", ".c", ".ino"])
_PY_SOURCE_EXTS: frozenset[str] = frozenset([".py"])


def _get_max_source_file_mtime(
    root: Path,
    exts: frozenset[str] | None = None,
    skip_dir_names: frozenset[str] | None = None,
) -> float:
    if exts is None:
        exts = _CPP_SOURCE_EXTS
    if skip_dir_names is None:
        skip_dir_names = _DEFAULT_SKIP_DIR_NAMES

    max_mtime = 0.0
    stack = [str(root)]
    while stack:
        current = stack.pop()
        try:
            with os.scandir(current) as it:
                for entry in it:
                    try:
                        name = entry.name
                        if entry.is_dir(follow_symlinks=False):
                            if name not in skip_dir_names:
                                stack.append(entry.path)
                        elif entry.is_file(follow_symlinks=False):
                            _, ext = os.path.splitext(name)
                            if ext.lower() in exts:
                                mtime = entry.stat(follow_symlinks=False).st_mtime
                                if mtime > max_mtime:
                                    max_mtime = mtime
                    except OSError:
                        pass
        except OSError:
            pass
    return max_mtime


class FingerprintManager:
    def __init__(
        self,
        cache_dir: Path,
        build_mode: str = "quick",
        mode_aware_names: set[str] | None = None,
    ) -> None:
        self.cache_dir = cache_dir
        self.build_mode = build_mode
        self.fingerprint_dir = cache_dir / "fingerprint"
        self.cache_dir.mkdir(exist_ok=True)
        self.fingerprint_dir.mkdir(exist_ok=True)
        self._fingerprints: dict[str, FingerprintResult] = {}
        self._prev_fingerprints: dict[str, Optional[FingerprintResult]] = {}
        self._mode_aware_names: set[str] = mode_aware_names or {"cpp_test", "examples"}

    def _get_fingerprint_file(self, name: str) -> Path:
        if name in self._mode_aware_names:
            return self.fingerprint_dir / f"{name}_{self.build_mode}.json"
        return self.fingerprint_dir / f"{name}.json"

    def read(self, name: str) -> Optional[FingerprintResult]:
        fp_file = self._get_fingerprint_file(name)
        if fp_file.exists():
            try:
                with open(fp_file, "r", encoding="utf-8") as f:
                    data = json.load(f)
                return FingerprintResult(
                    hash=data.get("hash", ""),
                    elapsed_seconds=data.get("elapsed_seconds"),
                    status=data.get("status"),
                    num_tests_run=data.get("num_tests_run"),
                    num_tests_passed=data.get("num_tests_passed"),
                    duration_seconds=data.get("duration_seconds"),
                    test_name=data.get("test_name"),
                )
            except (json.JSONDecodeError, OSError):
                pass
        return None

    def write(self, name: str, fingerprint: FingerprintResult) -> None:
        fp_file = self._get_fingerprint_file(name)
        data: dict[str, object] = {
            "hash": fingerprint.hash,
            "elapsed_seconds": fingerprint.elapsed_seconds,
            "status": fingerprint.status,
        }
        if fingerprint.num_tests_run is not None:
            data["num_tests_run"] = fingerprint.num_tests_run
        if fingerprint.num_tests_passed is not None:
            data["num_tests_passed"] = fingerprint.num_tests_passed
        if fingerprint.duration_seconds is not None:
            data["duration_seconds"] = fingerprint.duration_seconds
        if fingerprint.test_name is not None:
            data["test_name"] = fingerprint.test_name
        with open(fp_file, "w", encoding="utf-8") as f:
            json.dump(data, f, indent=2)

    def check(self, name: str, calculator: Callable[[], FingerprintResult]) -> bool:
        prev = self.read(name)
        self._prev_fingerprints[name] = prev

        current = calculator()
        self._fingerprints[name] = current

        if prev is None:
            return True
        return not prev.should_skip(current)

    def save_all(self, status: str) -> None:
        for name, fingerprint in self._fingerprints.items():
            fingerprint.status = status
            if fingerprint.num_tests_run is None:
                prev = self._prev_fingerprints.get(name)
                if prev is not None:
                    fingerprint.num_tests_run = prev.num_tests_run
                    fingerprint.num_tests_passed = prev.num_tests_passed
                    fingerprint.duration_seconds = prev.duration_seconds
                    fingerprint.test_name = prev.test_name
            self.write(name, fingerprint)

    def update_test_metadata(
        self,
        name: str,
        num_tests_run: int,
        num_tests_passed: int,
        duration_seconds: float,
        test_name: Optional[str] = None,
    ) -> None:
        if name in self._fingerprints:
            fingerprint = self._fingerprints[name]
            fingerprint.num_tests_run = num_tests_run
            fingerprint.num_tests_passed = num_tests_passed
            fingerprint.duration_seconds = duration_seconds
            if test_name:
                fingerprint.test_name = test_name

    def get_prev_fingerprint(self, name: str) -> Optional[FingerprintResult]:
        return self._prev_fingerprints.get(name)

    def _mtime_fast_path(
        self,
        name: str,
        *dirs: Path,
        exts: frozenset[str] | None = None,
    ) -> bool:
        fp_file = self._get_fingerprint_file(name)
        if not fp_file.exists():
            return False
        try:
            fp_mtime = fp_file.stat().st_mtime
            max_file_mtime = max(
                (_get_max_source_file_mtime(d, exts=exts) for d in dirs), default=0.0
            )
            if max_file_mtime > fp_mtime:
                return False
            prev = self.read(name)
            if prev is None or prev.status != "success":
                return False
            self._prev_fingerprints[name] = prev
            self._fingerprints[name] = FingerprintResult(hash=prev.hash)
            return True
        except OSError:
            return False
