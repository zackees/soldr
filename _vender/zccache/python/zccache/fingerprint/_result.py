from __future__ import annotations

from dataclasses import dataclass
from typing import Optional


@dataclass
class FingerprintResult:
    hash: str
    elapsed_seconds: Optional[str] = None
    status: Optional[str] = None
    num_tests_run: Optional[int] = None
    num_tests_passed: Optional[int] = None
    duration_seconds: Optional[float] = None
    test_name: Optional[str] = None

    def should_skip(self, current: "FingerprintResult") -> bool:
        if self.hash != current.hash:
            return False
        if self.status != "success":
            return False
        return True

    def get_cache_summary(self) -> str:
        if self.num_tests_run is not None and self.num_tests_passed is not None:
            duration_str = (
                f" in {self.duration_seconds:.2f}s" if self.duration_seconds else ""
            )
            return f"{self.num_tests_passed}/{self.num_tests_run} passed{duration_str}"
        return ""
