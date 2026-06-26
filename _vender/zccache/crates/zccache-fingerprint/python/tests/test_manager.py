"""Tests for FingerprintManager."""

import json
import tempfile
from pathlib import Path

from zccache.fingerprint import FingerprintManager, FingerprintResult


def test_check_first_run_returns_true() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        ran = mgr.check("test", lambda: FingerprintResult(hash="abc"))
        assert ran is True


def test_check_unchanged_returns_false() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save_all("success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        ran = mgr2.check("test", lambda: FingerprintResult(hash="abc"))
        assert ran is False


def test_check_changed_returns_true() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save_all("success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        ran = mgr2.check("test", lambda: FingerprintResult(hash="def"))
        assert ran is True


def test_check_previous_failure_returns_true() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save_all("failure")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        ran = mgr2.check("test", lambda: FingerprintResult(hash="abc"))
        assert ran is True


def test_save_all_persists_status() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save_all("success")

        fp_file = Path(tmp) / "fingerprint" / "test.json"
        assert fp_file.exists()
        data = json.loads(fp_file.read_text())
        assert data["hash"] == "abc"
        assert data["status"] == "success"


def test_cache_file_naming_with_build_mode() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp), build_mode="debug")
        mgr.check("cpp_test", lambda: FingerprintResult(hash="x"))
        mgr.save_all("success")

        assert (Path(tmp) / "fingerprint" / "cpp_test_debug.json").exists()

        mgr.check("other", lambda: FingerprintResult(hash="y"))
        mgr.save_all("success")
        assert (Path(tmp) / "fingerprint" / "other.json").exists()


def test_update_test_metadata() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.update_test_metadata("test", 10, 9, 1.5, "unit")
        mgr.save_all("success")

        data = json.loads(
            (Path(tmp) / "fingerprint" / "test.json").read_text()
        )
        assert data["num_tests_run"] == 10
        assert data["num_tests_passed"] == 9
        assert data["duration_seconds"] == 1.5
        assert data["test_name"] == "unit"


def test_save_all_preserves_prev_metadata_on_skip() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        # First run: record test metadata.
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.update_test_metadata("test", 10, 10, 2.0)
        mgr.save_all("success")

        # Second run: cache hit - no new metadata.
        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        mgr2.check("test", lambda: FingerprintResult(hash="abc"))
        mgr2.save_all("success")

        data = json.loads(
            (Path(tmp) / "fingerprint" / "test.json").read_text()
        )
        assert data["num_tests_run"] == 10  # Carried forward from prev


def test_get_prev_fingerprint() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        mgr = FingerprintManager(cache_dir=Path(tmp))
        mgr.check("test", lambda: FingerprintResult(hash="abc"))
        mgr.save_all("success")

        mgr2 = FingerprintManager(cache_dir=Path(tmp))
        mgr2.check("test", lambda: FingerprintResult(hash="abc"))
        prev = mgr2.get_prev_fingerprint("test")
        assert prev is not None
        assert prev.hash == "abc"
        assert prev.status == "success"
