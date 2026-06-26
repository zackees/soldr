from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from zccache._native import NativeClient, default_endpoint as _default_endpoint
from zccache.downloader import DownloadSource, FetchResult, FetchState


@dataclass(frozen=True)
class DaemonStatus:
    version: str
    artifact_count: int
    cache_size_bytes: int
    metadata_entries: int
    uptime_secs: int
    cache_hits: int
    cache_misses: int
    total_compilations: int
    non_cacheable: int
    compile_errors: int
    time_saved_ms: int
    total_links: int
    link_hits: int
    link_misses: int
    link_non_cacheable: int
    dep_graph_contexts: int
    dep_graph_files: int
    sessions_total: int
    sessions_active: int
    cache_dir: str
    dep_graph_version: int
    dep_graph_disk_size: int


@dataclass(frozen=True)
class SessionStartResult:
    session_id: str
    journal_path: str | None = None


@dataclass(frozen=True)
class SessionStats:
    duration_ms: int
    compilations: int
    hits: int
    misses: int
    non_cacheable: int
    errors: int
    time_saved_ms: int
    unique_sources: int
    bytes_read: int
    bytes_written: int


def _coerce_status(status: object) -> DaemonStatus:
    return DaemonStatus(
        version=status.version,
        artifact_count=status.artifact_count,
        cache_size_bytes=status.cache_size_bytes,
        metadata_entries=status.metadata_entries,
        uptime_secs=status.uptime_secs,
        cache_hits=status.cache_hits,
        cache_misses=status.cache_misses,
        total_compilations=status.total_compilations,
        non_cacheable=status.non_cacheable,
        compile_errors=status.compile_errors,
        time_saved_ms=status.time_saved_ms,
        total_links=status.total_links,
        link_hits=status.link_hits,
        link_misses=status.link_misses,
        link_non_cacheable=status.link_non_cacheable,
        dep_graph_contexts=status.dep_graph_contexts,
        dep_graph_files=status.dep_graph_files,
        sessions_total=status.sessions_total,
        sessions_active=status.sessions_active,
        cache_dir=status.cache_dir,
        dep_graph_version=status.dep_graph_version,
        dep_graph_disk_size=status.dep_graph_disk_size,
    )


def _coerce_session_stats(stats: object | None) -> SessionStats | None:
    if stats is None:
        return None
    return SessionStats(
        duration_ms=stats.duration_ms,
        compilations=stats.compilations,
        hits=stats.hits,
        misses=stats.misses,
        non_cacheable=stats.non_cacheable,
        errors=stats.errors,
        time_saved_ms=stats.time_saved_ms,
        unique_sources=stats.unique_sources,
        bytes_read=stats.bytes_read,
        bytes_written=stats.bytes_written,
    )


class ZcCacheClient:
    def __init__(self, endpoint: str | None = None) -> None:
        self.endpoint = endpoint
        self._native = NativeClient(endpoint)

    @staticmethod
    def default_endpoint() -> str:
        return _default_endpoint()

    def start(self) -> None:
        self._native.start()

    def stop(self) -> bool:
        return bool(self._native.stop())

    def status(self) -> DaemonStatus:
        return _coerce_status(self._native.status())

    def download(
        self,
        *,
        source: DownloadSource,
        destination: str | Path | None = None,
        unarchive: str | Path | None = None,
        expected_sha256: str | None = None,
        max_connections: int | None = None,
        min_segment_size: int | None = None,
        blocking: bool = True,
        dry_run: bool = False,
        force: bool = False,
    ) -> FetchResult:
        result = self._native.download(
            source,
            None if destination is None else str(Path(destination)),
            None if unarchive is None else str(Path(unarchive)),
            expected_sha256,
            max_connections,
            min_segment_size,
            blocking,
            dry_run,
            force,
        )
        return FetchResult(
            status=result.status,
            cache_path=result.cache_path,
            expanded_path=result.expanded_path,
            bytes=result.bytes,
            sha256=result.sha256,
        )

    def download_exists(
        self,
        *,
        source: DownloadSource,
        destination: str | Path | None = None,
        unarchive: str | Path | None = None,
        expected_sha256: str | None = None,
    ) -> FetchState:
        state = self._native.download_exists(
            source,
            None if destination is None else str(Path(destination)),
            None if unarchive is None else str(Path(unarchive)),
            expected_sha256,
        )
        return FetchState(
            kind=state.kind,
            cache_path=state.cache_path,
            expanded_path=state.expanded_path,
            bytes=state.bytes,
            sha256=state.sha256,
            reason=state.reason,
        )

    def session_start(
        self,
        *,
        cwd: str | Path = ".",
        log_file: str | Path | None = None,
        track_stats: bool = False,
        journal_path: str | Path | None = None,
    ) -> SessionStartResult:
        result = self._native.session_start(
            str(Path(cwd)),
            None if log_file is None else str(Path(log_file)),
            track_stats,
            None if journal_path is None else str(Path(journal_path)),
        )
        return SessionStartResult(
            session_id=result.session_id,
            journal_path=result.journal_path,
        )

    def session_end(self, session_id: str) -> SessionStats | None:
        return _coerce_session_stats(self._native.session_end(session_id))

    def session_stats(self, session_id: str) -> SessionStats | None:
        return _coerce_session_stats(self._native.session_stats(session_id))
