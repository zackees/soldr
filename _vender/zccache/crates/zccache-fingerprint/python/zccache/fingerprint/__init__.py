"""zccache-fingerprint: fast file fingerprinting backed by blake3."""

from __future__ import annotations

import os
import time
from pathlib import Path
from zccache.fingerprint._manager import FingerprintManager
from zccache.fingerprint._result import FingerprintResult


class Api:
    """Core hashing functions backed by Rust + blake3."""

    @staticmethod
    def hash_files(
        root: str,
        extensions: list[str] | None = None,
        exclude_dirs: list[str] | None = None,
    ) -> str:
        """Aggregate blake3 hash of files filtered by extension.

        Args:
            root: Directory to scan.
            extensions: File extensions without dot (e.g. ``["rs", "toml"]``).
                        Empty or None means all files.
            exclude_dirs: Directory names to skip (e.g. ``[".git", "target"]``).

        Returns:
            64-char hex-encoded blake3 hash.
        """
        from zccache.fingerprint._native import hash_files

        return hash_files(root, extensions or [], exclude_dirs or [])

    @staticmethod
    def hash_files_glob(
        root: str,
        include: list[str] | None = None,
        exclude: list[str] | None = None,
    ) -> str:
        """Aggregate blake3 hash of files filtered by glob patterns.

        Args:
            root: Directory to scan.
            include: Glob include patterns (e.g. ``["src/**/*.rs"]``).
                     Empty or None means all files.
            exclude: Glob exclude patterns (e.g. ``[".git/**", "target/**"]``).

        Returns:
            64-char hex-encoded blake3 hash.
        """
        from zccache.fingerprint._native import hash_files_glob

        return hash_files_glob(root, include or [], exclude or [])

    @staticmethod
    def walk_and_hash(
        root: str,
        extensions: list[str] | None = None,
        exclude_dirs: list[str] | None = None,
    ) -> list[tuple[str, str]]:
        """Per-file blake3 hashes.

        Args:
            root: Directory to scan.
            extensions: File extensions without dot. Empty or None means all.
            exclude_dirs: Directory names to skip.

        Returns:
            List of ``(relative_path, blake3_hex)`` tuples sorted by path.
        """
        from zccache.fingerprint._native import walk_and_hash

        return walk_and_hash(root, extensions or [], exclude_dirs or [])

    @staticmethod
    def hash_directory(
        start_directory: str | Path,
        glob: str = "**/*.h,**/*.cpp,**/*.hpp",
    ) -> str:
        """Compute blake3 hash of directory contents filtered by glob string.

        This is a convenience wrapper that parses a comma-separated glob
        string (e.g. ``"**/*.h,**/*.cpp"``) into extensions and delegates
        to :meth:`hash_files`.

        Args:
            start_directory: Root directory to scan.
            glob: Comma-separated glob patterns.

        Returns:
            64-char hex-encoded blake3 hash.
        """
        from zccache.fingerprint._native import hash_files

        extensions: list[str] = []
        for pattern in glob.split(","):
            _, ext = os.path.splitext(pattern.strip())
            if ext:
                extensions.append(ext.lstrip("."))
        return hash_files(str(start_directory), extensions)

    @staticmethod
    def fingerprint_code_base(
        start_directory: str | Path | None = None,
        glob: str = "**/*.h,**/*.cpp,**/*.hpp",
    ) -> FingerprintResult:
        """Hash a code base and return a :class:`FingerprintResult` with timing.

        Args:
            start_directory: Root directory to scan. Defaults to ``cwd()/src``.
            glob: Comma-separated glob patterns.
        """
        directory = str(start_directory) if start_directory is not None else str(Path.cwd() / "src")
        start_time = time.time()
        try:
            h = Api.hash_directory(directory, glob)
            elapsed = time.time() - start_time
            return FingerprintResult(hash=h, elapsed_seconds=f"{elapsed:.2f}")
        except Exception as e:
            elapsed = time.time() - start_time
            return FingerprintResult(
                hash="",
                elapsed_seconds=f"{elapsed:.2f}",
                status=f"error: {e}",
            )


__all__ = [
    "Api",
    "FingerprintManager",
    "FingerprintResult",
]
