from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Callable, Optional

from zccache.fingerprint._result import FingerprintResult

# Default directory names to skip during mtime scanning.
_DEFAULT_SKIP_DIR_NAMES: frozenset[str] = frozenset(
    [".git", "target", "node_modules", "__pycache__", ".mypy_cache", "build"]
)

# Default source extensions for C++ projects.
_CPP_SOURCE_EXTS: frozenset[str] = frozenset([".cpp", ".h", ".hpp", ".c", ".ino"])
_PY_SOURCE_EXTS: frozenset[str] = frozenset([".py"])


def _get_max_source_file_mtime(
    root: Path,
    exts: frozenset[str] | None = None,
    skip_dir_names: frozenset[str] | None = None,
) -> float:
    """Return max mtime of any source file under *root*, skipping build dirs.

    Returns 0.0 on missing root or any OS error.
    """
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
    """Cache manager for fingerprint-based skip/run decisions.

    Parameters:
        cache_dir: Directory for cache files.
        build_mode: Build mode string included in cache file names for
                    mode-aware caching (e.g. ``"quick"``, ``"debug"``).
        mode_aware_names: Names that include *build_mode* in the cache file
                         name.  Defaults to ``{"cpp_test", "examples"}``.
    """

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
        self._mode_aware_names: set[str] = mode_aware_names or {
            "cpp_test",
            "examples",
        }

    # ------------------------------------------------------------------
    # Cache file naming
    # ------------------------------------------------------------------

    def _get_fingerprint_file(self, name: str) -> Path:
        if name in self._mode_aware_names:
            return self.fingerprint_dir / f"{name}_{self.build_mode}.json"
        return self.fingerprint_dir / f"{name}.json"

    # ------------------------------------------------------------------
    # Read / write
    # ------------------------------------------------------------------

    def read(self, name: str) -> Optional[FingerprintResult]:
        """Read a cached fingerprint from disk."""
        fp_file = self._get_fingerprint_file(name)
        if fp_file.exists():
            try:
                with open(fp_file, "r") as f:
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
        """Write a fingerprint to disk."""
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
        with open(fp_file, "w") as f:
            json.dump(data, f, indent=2)

    # ------------------------------------------------------------------
    # Check / save
    # ------------------------------------------------------------------

    def check(self, name: str, calculator: Callable[[], FingerprintResult]) -> bool:
        """Return True if the operation should run (cache miss or failure)."""
        prev = self.read(name)
        self._prev_fingerprints[name] = prev

        current = calculator()
        self._fingerprints[name] = current

        if prev is None:
            return True
        return not prev.should_skip(current)

    def save_all(self, status: str) -> None:
        """Persist all fingerprints with the given status."""
        for name, fp in self._fingerprints.items():
            fp.status = status
            # Carry forward test metadata from previous run on cache hits.
            if fp.num_tests_run is None:
                prev = self._prev_fingerprints.get(name)
                if prev is not None:
                    fp.num_tests_run = prev.num_tests_run
                    fp.num_tests_passed = prev.num_tests_passed
                    fp.duration_seconds = prev.duration_seconds
                    fp.test_name = prev.test_name
            self.write(name, fp)

    # ------------------------------------------------------------------
    # Test metadata
    # ------------------------------------------------------------------

    def update_test_metadata(
        self,
        name: str,
        num_tests_run: int,
        num_tests_passed: int,
        duration_seconds: float,
        test_name: Optional[str] = None,
    ) -> None:
        """Attach test metadata to a fingerprint before saving."""
        if name in self._fingerprints:
            fp = self._fingerprints[name]
            fp.num_tests_run = num_tests_run
            fp.num_tests_passed = num_tests_passed
            fp.duration_seconds = duration_seconds
            if test_name:
                fp.test_name = test_name

    def get_prev_fingerprint(self, name: str) -> Optional[FingerprintResult]:
        """Previous fingerprint (from last persisted run)."""
        return self._prev_fingerprints.get(name)

    # ------------------------------------------------------------------
    # Mtime fast-path
    # ------------------------------------------------------------------

    def _mtime_fast_path(
        self,
        name: str,
        *dirs: Path,
        exts: frozenset[str] | None = None,
    ) -> bool:
        """Return True if the fingerprint is up-to-date via mtime check.

        If the cache file is newer than all source files in *dirs* and the
        previous status was ``"success"``, skip the expensive hash
        computation.

        Side-effects on True: populates ``_prev_fingerprints[name]`` and
        ``_fingerprints[name]`` so that :meth:`save_all` works correctly.
        """
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
