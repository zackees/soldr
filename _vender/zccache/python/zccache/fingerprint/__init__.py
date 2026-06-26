"""zccache fingerprint APIs."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from zccache._native import NativeClient
from zccache.fingerprint._manager import FingerprintManager
from zccache.fingerprint._result import FingerprintResult


@dataclass(frozen=True)
class FingerprintDecision:
    should_run: bool
    reason: str | None = None
    changed_files: tuple[str, ...] = ()


class FingerprintCache:
    def __init__(
        self,
        cache_file: str | Path,
        *,
        cache_type: str = "two-layer",
        endpoint: str | None = None,
    ) -> None:
        self.cache_file = Path(cache_file)
        self.cache_type = cache_type
        self._native = NativeClient(endpoint)

    def check(
        self,
        *,
        root: str | Path = ".",
        include: list[str] | None = None,
        exclude: list[str] | None = None,
        ext: list[str] | None = None,
    ) -> FingerprintDecision:
        include_list = list(include or [])
        ext_list = list(ext or [])
        if include_list and ext_list:
            raise ValueError("include and ext are mutually exclusive")
        result = self._native.fingerprint_check(
            str(self.cache_file),
            self.cache_type,
            str(Path(root)),
            ext_list,
            include_list,
            list(exclude or []),
        )
        return FingerprintDecision(
            should_run=result.decision == "run",
            reason=result.reason,
            changed_files=tuple(result.changed_files),
        )

    def mark_success(self) -> None:
        self._native.fingerprint_mark_success(str(self.cache_file))

    def mark_failure(self) -> None:
        self._native.fingerprint_mark_failure(str(self.cache_file))

    def invalidate(self) -> None:
        self._native.fingerprint_invalidate(str(self.cache_file))


class Api:
    """Core hashing functions backed by Rust + blake3."""

    @staticmethod
    def hash_files(
        root: str,
        extensions: list[str] | None = None,
        exclude_dirs: list[str] | None = None,
    ) -> str:
        from zccache.fingerprint._native import hash_files

        return hash_files(root, extensions or [], exclude_dirs or [])

    @staticmethod
    def hash_files_glob(
        root: str,
        include: list[str] | None = None,
        exclude: list[str] | None = None,
    ) -> str:
        from zccache.fingerprint._native import hash_files_glob

        return hash_files_glob(root, include or [], exclude or [])

    @staticmethod
    def walk_and_hash(
        root: str,
        extensions: list[str] | None = None,
        exclude_dirs: list[str] | None = None,
    ) -> list[tuple[str, str]]:
        from zccache.fingerprint._native import walk_and_hash

        return walk_and_hash(root, extensions or [], exclude_dirs or [])

    @staticmethod
    def hash_directory(
        start_directory: str | Path,
        glob: str = "**/*.h,**/*.cpp,**/*.hpp",
    ) -> str:
        extensions: list[str] = []
        for pattern in glob.split(","):
            ext = Path(pattern.strip()).suffix
            if ext:
                extensions.append(ext.lstrip("."))
        return Api.hash_files(str(start_directory), extensions)

    @staticmethod
    def fingerprint_code_base(
        start_directory: str | Path | None = None,
        glob: str = "**/*.h,**/*.cpp,**/*.hpp",
    ) -> FingerprintResult:
        import time

        directory = (
            str(start_directory) if start_directory is not None else str(Path.cwd() / "src")
        )
        start_time = time.time()
        try:
            digest = Api.hash_directory(directory, glob)
            elapsed = time.time() - start_time
            return FingerprintResult(hash=digest, elapsed_seconds=f"{elapsed:.2f}")
        except Exception as exc:
            elapsed = time.time() - start_time
            return FingerprintResult(
                hash="",
                elapsed_seconds=f"{elapsed:.2f}",
                status=f"error: {exc}",
            )


__all__ = [
    "Api",
    "FingerprintCache",
    "FingerprintDecision",
    "FingerprintManager",
    "FingerprintResult",
]
