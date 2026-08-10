from __future__ import annotations

import json
import os
import re
import sys
import uuid
from pathlib import Path

TAIL_LINE_LIMIT = 40
_PYTEST_FAILURE_LINE_LIMIT = 80
_PYTEST_FAILURE_HEADER = re.compile(r"=+\s+FAILURES\s+=+")
_PYTEST_SUMMARY_HEADER = re.compile(r"=+\s+short test summary info\s+=+")
_PYTEST_ERROR_LINE = re.compile(r"^E\s{2,}")
_PYTEST_FAILED_LINE = re.compile(r"^(FAILED\s+tests[\\/]|tests[\\/].+\s+FAILED\s+\[)")


def _escape_annotation(text: str) -> str:
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def _append_summary(lines: list[str]) -> None:
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_path:
        return
    with Path(summary_path).open("a", encoding="utf-8") as handle:
        for line in lines:
            handle.write(line)
            handle.write("\n")


def _load_json(path: Path) -> dict[str, object] | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None


def _print_group(title: str, lines: list[str]) -> None:
    token = f"RUNNING_PROCESS_STOP_{uuid.uuid4().hex}"
    print(f"::group::{title}")
    print(f"::stop-commands::{token}")
    for line in lines:
        print(line)
    print(f"::{token}::")
    print("::endgroup::")


def _tail_text_file(path: Path) -> list[str]:
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return []
    return lines[-TAIL_LINE_LIMIT:]


def _extract_pytest_failure_excerpt(lines: list[str]) -> list[str]:
    if not lines:
        return []
    for index, line in enumerate(lines):
        if _PYTEST_FAILURE_HEADER.search(line):
            return lines[index : index + _PYTEST_FAILURE_LINE_LIMIT]
    for index, line in enumerate(lines):
        if _PYTEST_SUMMARY_HEADER.search(line):
            start = max(0, index - 20)
            return lines[start : index + _PYTEST_FAILURE_LINE_LIMIT]
    hit_indexes = [
        index
        for index, line in enumerate(lines)
        if _PYTEST_ERROR_LINE.search(line) or _PYTEST_FAILED_LINE.search(line)
    ]
    if not hit_indexes:
        return []
    start = max(0, hit_indexes[0] - 8)
    end = min(len(lines), hit_indexes[-1] + 16)
    return lines[start:end]


def _analytics_failure_excerpt(data: dict[str, object]) -> list[str]:
    explicit_excerpt = [str(line) for line in data.get("pytest_failure_excerpt", [])]
    if explicit_excerpt:
        return explicit_excerpt[-_PYTEST_FAILURE_LINE_LIMIT:]
    tail_lines = [str(line) for line in data.get("tail_lines", [])][-TAIL_LINE_LIMIT:]
    return _extract_pytest_failure_excerpt(tail_lines)


def _render_analytics(path: Path) -> list[str]:
    data = _load_json(path)
    if data is None:
        return []
    log_path = str(data.get("log_path", path.name.removesuffix(".analytics.json")))
    returncode = data.get("returncode")
    if returncode == 0:
        return []
    last_test_nodeid = data.get("last_test_nodeid")
    last_nonempty_line = data.get("last_nonempty_line")
    tail_lines = [str(line) for line in data.get("tail_lines", [])][-TAIL_LINE_LIMIT:]
    fault_lines = [str(line) for line in data.get("fault_lines", [])][-TAIL_LINE_LIMIT:]

    title = f"{Path(log_path).name} failure analytics"
    annotation_parts = [f"log={Path(log_path).name}", f"exit={returncode}"]
    if last_test_nodeid:
        annotation_parts.append(f"last_test={last_test_nodeid}")
    if last_nonempty_line:
        annotation_parts.append(f"last_line={last_nonempty_line}")
    print(f"::error title={title}::{_escape_annotation(' | '.join(annotation_parts))}")

    rendered = [
        f"log_path: {log_path}",
        f"returncode: {returncode}",
    ]
    if last_test_nodeid:
        rendered.append(f"last_test_nodeid: {last_test_nodeid}")
    if last_nonempty_line:
        rendered.append(f"last_nonempty_line: {last_nonempty_line}")
    if fault_lines:
        rendered.append("")
        rendered.append("fault_lines:")
        rendered.extend(fault_lines)
    failure_excerpt = _analytics_failure_excerpt(data)
    if failure_excerpt:
        rendered.append("")
        rendered.append("pytest_failure_excerpt:")
        rendered.extend(failure_excerpt)
    if tail_lines:
        rendered.append("")
        rendered.append("tail_lines:")
        rendered.extend(tail_lines)
    _print_group(title, rendered)

    summary = [
        f"### {Path(log_path).name}",
        f"- Exit code: `{returncode}`",
    ]
    if last_test_nodeid:
        summary.append(f"- Last pytest nodeid: `{last_test_nodeid}`")
    if last_nonempty_line:
        summary.append(f"- Last output line: `{last_nonempty_line}`")
    if failure_excerpt:
        summary.append("")
        summary.append("```text")
        summary.extend(failure_excerpt)
        summary.append("```")
    summary.append("")
    summary.append("```text")
    summary.extend(tail_lines or ["(no tail lines captured)"])
    summary.append("```")
    summary.append("")
    return summary


def _render_running_process_dump(path: Path) -> list[str]:
    data = _load_json(path)
    if data is None:
        return []

    reason = data.get("reason", "unknown")
    pid = data.get("pid")
    returncode = data.get("returncode")
    command = data.get("command", [])
    title = f"running-process {reason} pid={pid}"
    print(
        f"::notice title={title}::"
        f"{_escape_annotation(f'returncode={returncode} command={command!r}')}"
    )

    rendered = [
        f"path: {path}",
        f"reason: {reason}",
        f"pid: {pid}",
        f"returncode: {returncode}",
        f"command: {command!r}",
    ]
    child_output = data.get("child_output")
    if isinstance(child_output, dict):
        for stream_name in ("stdout", "stderr"):
            stream = child_output.get(stream_name)
            if not isinstance(stream, dict):
                continue
            tail_text = str(stream.get("tail", stream.get("tail_text", "")))
            tail = tail_text.splitlines()[-TAIL_LINE_LIMIT:]
            failure_excerpt = _extract_pytest_failure_excerpt(tail_text.splitlines())
            rendered.append("")
            rendered.append(
                f"child_output.{stream_name}: bytes_seen="
                f"{stream.get('bytes_seen', stream.get('total_bytes'))} "
                f"truncated={stream.get('truncated', stream.get('tail_truncated'))}"
            )
            if failure_excerpt:
                rendered.append("pytest_failure_excerpt:")
                rendered.extend(failure_excerpt)
            rendered.extend(tail or ["(no captured output)"])

    for suffix in (".py-spy.log", ".native-debugger.log"):
        companion = path.with_suffix(suffix)
        tail = _tail_text_file(companion)
        if tail:
            rendered.append("")
            rendered.append(f"{companion.name}:")
            rendered.extend(tail)

    _print_group(title, rendered)

    summary = [
        f"### {title}",
        f"- Return code: `{returncode}`",
        f"- Command: `{command!r}`",
        "",
    ]
    if isinstance(child_output, dict):
        for stream_name in ("stdout", "stderr"):
            stream = child_output.get(stream_name)
            if not isinstance(stream, dict):
                continue
            tail_text = str(stream.get("tail", stream.get("tail_text", "")))
            tail = tail_text.splitlines()[-TAIL_LINE_LIMIT:]
            failure_excerpt = _extract_pytest_failure_excerpt(tail_text.splitlines())
            if failure_excerpt:
                summary.append(f"#### Child {stream_name} failure excerpt")
                summary.append("```text")
                summary.extend(failure_excerpt)
                summary.append("```")
                summary.append("")
            summary.append(f"#### Child {stream_name} tail")
            summary.append("```text")
            summary.extend(tail or ["(no captured output)"])
            summary.append("```")
            summary.append("")
    return summary


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    logs_dir = Path(args[0]) if args else Path("logs")
    if not logs_dir.exists():
        print(f"[render_failure_diagnostics] {logs_dir} does not exist")
        return 0

    summary_lines = ["## Failure Diagnostics", ""]

    analytics_files = sorted(logs_dir.glob("*.analytics.json"))
    for path in analytics_files:
        summary_lines.extend(_render_analytics(path))

    running_process_dir = logs_dir / "running-process"
    if running_process_dir.is_dir():
        for path in sorted(running_process_dir.glob("*.json")):
            summary_lines.extend(_render_running_process_dump(path))

    if len(summary_lines) > 2:
        _append_summary(summary_lines)
    else:
        print("[render_failure_diagnostics] no analytics or running-process dumps found")
    return 0


if __name__ == "__main__":
    sys.exit(main())
