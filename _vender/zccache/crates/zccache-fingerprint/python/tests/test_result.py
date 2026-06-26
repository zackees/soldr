"""Tests for FingerprintResult."""

from zccache.fingerprint import FingerprintResult


def test_should_skip_on_match_and_success() -> None:
    prev = FingerprintResult(hash="abc123", status="success")
    current = FingerprintResult(hash="abc123")
    assert prev.should_skip(current) is True


def test_should_run_on_hash_mismatch() -> None:
    prev = FingerprintResult(hash="abc123", status="success")
    current = FingerprintResult(hash="def456")
    assert prev.should_skip(current) is False


def test_should_run_on_previous_failure() -> None:
    prev = FingerprintResult(hash="abc123", status="failure")
    current = FingerprintResult(hash="abc123")
    assert prev.should_skip(current) is False


def test_should_run_on_no_status() -> None:
    prev = FingerprintResult(hash="abc123")
    current = FingerprintResult(hash="abc123")
    assert prev.should_skip(current) is False


def test_get_cache_summary_with_metadata() -> None:
    r = FingerprintResult(
        hash="x",
        num_tests_run=10,
        num_tests_passed=9,
        duration_seconds=1.5,
    )
    assert r.get_cache_summary() == "9/10 passed in 1.50s"


def test_get_cache_summary_without_metadata() -> None:
    r = FingerprintResult(hash="x")
    assert r.get_cache_summary() == ""


def test_get_cache_summary_without_duration() -> None:
    r = FingerprintResult(hash="x", num_tests_run=5, num_tests_passed=5)
    assert r.get_cache_summary() == "5/5 passed"
