"""Standalone KeyboardInterrupt handling checker for Python files.

Imported from the FastLED workflow and scoped here to lint the watcher Python API.
"""

from __future__ import annotations

import argparse
import ast
import re
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

_BROAD_EXCEPT_RE = re.compile(
    r"^\s*except\s*"
    r"(?:" r":|" r"\(?.*\b(?:Exception|BaseException|KeyboardInterrupt)\b" r")",
    re.MULTILINE,
)


def has_broad_except(source: str) -> bool:
    return _BROAD_EXCEPT_RE.search(source) is not None


@dataclass(frozen=True)
class Violation:
    line: int
    col: int
    code: str
    message: str

    def __str__(self) -> str:
        return f"{self.code} {self.message}"


_NOQA_RE = re.compile(r"#\s*noqa\b(?::\s*([\w,\s]+))?")


def _is_suppressed(source_lines: list[str], lineno: int, code: str) -> bool:
    if lineno < 1 or lineno > len(source_lines):
        return False
    match = _NOQA_RE.search(source_lines[lineno - 1])
    if match is None:
        return False
    codes = match.group(1)
    if codes is None:
        return True
    return code in {c.strip() for c in codes.split(",")}


class TryExceptVisitor(ast.NodeVisitor):
    def __init__(self, source_lines: list[str] | None = None) -> None:
        self.violations: list[Violation] = []
        self._source_lines = source_lines or []

    def visit_Try(self, node: ast.Try) -> None:  # noqa: N802
        catches_broad_exception = False
        has_keyboard_interrupt_handler = False
        keyboard_interrupt_handlers: list[ast.ExceptHandler] = []

        for handler in node.handlers:
            if handler.type is None:
                catches_broad_exception = True
            elif isinstance(handler.type, ast.Name):
                if handler.type.id in ("Exception", "BaseException"):
                    catches_broad_exception = True
                elif handler.type.id == "KeyboardInterrupt":
                    has_keyboard_interrupt_handler = True
                    keyboard_interrupt_handlers.append(handler)
            elif isinstance(handler.type, ast.Tuple):
                for exc_type in handler.type.elts:
                    if isinstance(exc_type, ast.Name):
                        if exc_type.id in ("Exception", "BaseException"):
                            catches_broad_exception = True
                        elif exc_type.id == "KeyboardInterrupt":
                            has_keyboard_interrupt_handler = True
                            keyboard_interrupt_handlers.append(handler)

        if catches_broad_exception and not has_keyboard_interrupt_handler:
            if not _is_suppressed(self._source_lines, node.lineno, "KBI001"):
                self.violations.append(
                    Violation(
                        line=node.lineno,
                        col=node.col_offset,
                        code="KBI001",
                        message=(
                            "Try-except catches Exception/BaseException without KeyboardInterrupt handler. "
                            "Add: except KeyboardInterrupt as ki: _thread.interrupt_main(); raise"
                        ),
                    )
                )

        for handler in keyboard_interrupt_handlers:
            if not _handler_calls_interrupt_main(handler):
                if not _is_suppressed(self._source_lines, handler.lineno, "KBI002"):
                    self.violations.append(
                        Violation(
                            line=handler.lineno,
                            col=handler.col_offset,
                            code="KBI002",
                            message=(
                                "KeyboardInterrupt handler must call _thread.interrupt_main() "
                                "or handle_keyboard_interrupt(ki) or notify_main_thread()"
                            ),
                        )
                    )

        for call_node in _find_interrupt_handler_calls(node.body):
            if not _is_suppressed(self._source_lines, call_node.lineno, "KBI003"):
                self.violations.append(
                    Violation(
                        line=call_node.lineno,
                        col=call_node.col_offset,
                        code="KBI003",
                        message="interrupt handler helper called outside KeyboardInterrupt handler",
                    )
                )

        for handler in node.handlers:
            if handler in keyboard_interrupt_handlers:
                continue
            for call_node in _find_interrupt_handler_calls(handler.body):
                if not _is_suppressed(self._source_lines, call_node.lineno, "KBI003"):
                    self.violations.append(
                        Violation(
                            line=call_node.lineno,
                            col=call_node.col_offset,
                            code="KBI003",
                            message="interrupt handler helper called outside KeyboardInterrupt handler",
                        )
                    )

        self.generic_visit(node)


_INTERRUPT_HANDLER_NAMES = frozenset({"handle_keyboard_interrupt", "notify_main_thread"})


def _find_interrupt_handler_calls(stmts: list[ast.stmt]) -> list[ast.Call]:
    calls: list[ast.Call] = []
    _collect_calls(stmts, calls)
    return calls


def _collect_calls(nodes: Sequence[ast.AST], out: list[ast.Call]) -> None:
    for node in nodes:
        if isinstance(node, ast.Try):
            return
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            if node.func.id in _INTERRUPT_HANDLER_NAMES:
                out.append(node)
        _collect_calls(list(ast.iter_child_nodes(node)), out)


def _handler_calls_interrupt_main(handler: ast.ExceptHandler) -> bool:
    for node in ast.walk(handler):
        if isinstance(node, ast.Call):
            if isinstance(node.func, ast.Attribute):
                if (
                    isinstance(node.func.value, ast.Name)
                    and node.func.value.id == "_thread"
                    and node.func.attr == "interrupt_main"
                ):
                    return True
            if isinstance(node.func, ast.Name):
                if node.func.id in _INTERRUPT_HANDLER_NAMES:
                    return True
    return False


def check_file(path: str, source: str) -> list[Violation]:
    try:
        tree = ast.parse(source, filename=path)
    except SyntaxError:
        return []
    visitor = TryExceptVisitor(source_lines=source.splitlines())
    visitor.visit(tree)
    return visitor.violations


def collect_python_files(paths: list[str], excludes: list[str]) -> list[Path]:
    result: list[Path] = []
    exclude_parts = [e.replace("\\", "/").strip("/") for e in excludes]
    for path_str in paths:
        path = Path(path_str)
        if path.is_file() and path.suffix == ".py":
            if not _is_excluded(path, exclude_parts):
                result.append(path)
        elif path.is_dir():
            for py_file in path.rglob("*.py"):
                if not _is_excluded(py_file, exclude_parts):
                    result.append(py_file)
    return result


def _is_excluded(path: Path, excludes: list[str]) -> bool:
    path_str = path.as_posix()
    return any(exc in path_str for exc in excludes)


def find_candidates(files: list[Path]) -> list[tuple[Path, str]]:
    candidates: list[tuple[Path, str]] = []
    for path in files:
        try:
            source = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if has_broad_except(source):
            candidates.append((path, source))
    return candidates


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Check Python files for proper KeyboardInterrupt handling.",
    )
    parser.add_argument("paths", nargs="+", help="Files or directories to check")
    parser.add_argument("--exclude", nargs="*", default=[], help="Path substrings to exclude")
    args = parser.parse_args(argv)

    files = collect_python_files(args.paths, args.exclude)
    candidates = find_candidates(files)
    violations = 0
    for path, source in candidates:
        for violation in check_file(str(path), source):
            print(f"{path}:{violation.line}:{violation.col}: {violation}")
            violations += 1
    return 1 if violations else 0


if __name__ == "__main__":
    sys.exit(main())
