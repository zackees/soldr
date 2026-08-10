"""Standalone KeyboardInterrupt handling checker for Python files.

Replaces flake8 + custom plugin with a two-phase approach:
  Phase 1 (regex): Pre-scan for broad exception patterns, skip files with no matches.
  Phase 2 (AST):  Run TryExceptVisitor on candidates only.

Error Codes:
    KBI001: Try-except catches Exception/BaseException without KeyboardInterrupt handler
    KBI002: KeyboardInterrupt handler must call _thread.interrupt_main() or
            handle_keyboard_interrupt_properly() or notify_main_thread()
"""

from __future__ import annotations

import argparse
import ast
import re
import sys
from dataclasses import dataclass
from pathlib import Path

# ---------------------------------------------------------------------------
# Phase 1 - regex pre-filter
# ---------------------------------------------------------------------------

# Matches lines that contain a broad except clause:
#   except:               (bare except)
#   except Exception      (with or without parens / tuple)
#   except BaseException
#   except KeyboardInterrupt
_BROAD_EXCEPT_RE = re.compile(
    r"^\s*except\s*"
    r"(?:"
    r":|"  # bare except:
    r"\(?.*\b(?:Exception|BaseException|KeyboardInterrupt)\b"  # named broad
    r")",
    re.MULTILINE,
)


def has_broad_except(source: str) -> bool:
    """Return True if *source* contains a line that looks like a broad except."""
    return _BROAD_EXCEPT_RE.search(source) is not None


# ---------------------------------------------------------------------------
# Phase 2 - AST visitor  (logic identical to the former flake8 plugin)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Violation:
    line: int
    col: int
    code: str
    message: str

    def __str__(self) -> str:
        return f"{self.code} {self.message}"


class TryExceptVisitor(ast.NodeVisitor):
    """AST visitor to check try-except blocks for KeyboardInterrupt handling."""

    def __init__(self) -> None:
        self.violations: list[Violation] = []

    def visit_Try(self, node: ast.Try) -> None:
        catches_broad_exception = False
        has_keyboard_interrupt_handler = False
        keyboard_interrupt_handlers: list[ast.ExceptHandler] = []

        for handler in node.handlers:
            if handler.type is None:
                # bare except: catches everything
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
            # A broad handler that dispatches on isinstance() internally is
            # already correct; flagging it would demand a rewrite of working
            # code into a shape that is no safer.
            handled_inline = any(
                _dispatches_on_keyboard_interrupt(h)
                for h in node.handlers
                if h.type is None
                or (isinstance(h.type, ast.Name) and h.type.id in ("Exception", "BaseException"))
            )
            if handled_inline:
                self.generic_visit(node)
                return
            self.violations.append(
                Violation(
                    line=node.lineno,
                    col=node.col_offset,
                    code="KBI001",
                    message=(
                        "Try-except catches Exception/BaseException without a "
                        "KeyboardInterrupt handler. Add: except KeyboardInterrupt "
                        "as ke: handle_keyboard_interrupt(ke)"
                    ),
                )
            )

        for kbi_handler in keyboard_interrupt_handlers:
            if not _handler_calls_interrupt_main(kbi_handler):
                self.violations.append(
                    Violation(
                        line=kbi_handler.lineno,
                        col=kbi_handler.col_offset,
                        code="KBI002",
                        message=(
                            "KeyboardInterrupt handler must call _thread.interrupt_main() "
                            "or use handle_keyboard_interrupt_properly(). "
                            "Add: import _thread; _thread.interrupt_main()"
                        ),
                    )
                )

        self.generic_visit(node)


def _handler_calls_interrupt_main(handler: ast.ExceptHandler) -> bool:
    """Whether a KBI handler propagates the interrupt rather than swallowing it.

    The rule exists so a Ctrl+C on a worker thread still reaches the main
    thread. There is more than one correct way to do that, and treating only
    the helper call as correct produces false positives on code that is
    already right:

    * ``raise`` — re-raising propagates the KeyboardInterrupt to the caller,
      which is precisely what ``handle_keyboard_interrupt`` does on the main
      thread. A handler that re-raises has not swallowed anything.
    * ``handle_keyboard_interrupt(exc)`` and friends — the helper.
    * ``_thread.interrupt_main()`` — the underlying primitive.
    """
    for node in ast.walk(handler):
        # A bare `raise` re-raises the active exception; `raise exc` re-raises
        # a named one. Either way the interrupt keeps travelling.
        if isinstance(node, ast.Raise):
            return True
        if isinstance(node, ast.Call):
            # _thread.interrupt_main()
            if isinstance(node.func, ast.Attribute):
                if (
                    isinstance(node.func.value, ast.Name)
                    and node.func.value.id == "_thread"
                    and node.func.attr == "interrupt_main"
                ):
                    return True
            # handle_keyboard_interrupt() and its documented equivalents.
            if isinstance(node.func, ast.Name):
                if node.func.id in (
                    "handle_keyboard_interrupt",
                    "handle_keyboard_interrupt_properly",
                    "notify_main_thread",
                ):
                    return True
    return False


def _dispatches_on_keyboard_interrupt(handler: ast.ExceptHandler) -> bool:
    """Whether a broad handler singles out KeyboardInterrupt inside its body.

    ``except BaseException as exc:`` followed by ``if isinstance(exc,
    KeyboardInterrupt): _thread.interrupt_main()`` satisfies the rule as
    surely as a separate ``except KeyboardInterrupt`` clause does, and is the
    natural shape when the handler must also record the non-interrupt case.
    Requiring the separate clause would mean rewriting correct code.
    """
    for node in ast.walk(handler):
        if not isinstance(node, ast.Call):
            continue
        if not (isinstance(node.func, ast.Name) and node.func.id == "isinstance"):
            continue
        if len(node.args) != 2:
            continue
        target = node.args[1]
        names = (
            [target]
            if isinstance(target, ast.Name)
            else (target.elts if isinstance(target, ast.Tuple) else [])
        )
        if any(isinstance(n, ast.Name) and n.id == "KeyboardInterrupt" for n in names):
            # The dispatch exists; require that it also propagates.
            return _handler_calls_interrupt_main(handler)
    return False


# ---------------------------------------------------------------------------
# File collection helpers
# ---------------------------------------------------------------------------


_NOQA_RE = re.compile(r"#\s*noqa\s*:\s*(?P<codes>KBI\d{3}(?:\s*,\s*KBI\d{3})*)", re.IGNORECASE)


def _suppressed_codes(line: str) -> set[str]:
    """Codes silenced by a ``# noqa: KBI00x`` comment on *line*."""
    match = _NOQA_RE.search(line)
    if not match:
        return set()
    return {code.strip().upper() for code in match.group("codes").split(",")}


def check_file(path: str, source: str) -> list[Violation]:
    """Parse *source* and return all KBI violations found.

    A ``# noqa: KBI00x`` comment on the reported line suppresses that code.
    The rule has legitimate exceptions — a CLI's top-level handler exists to
    turn Ctrl+C into a clean exit, and making it re-raise would replace an
    orderly shutdown with a traceback. Without a way to say so, the only ways
    to keep the gate green are to damage that code or to leave the whole check
    disabled, and the second is how this checker fell out of the build before.
    A blanket bare ``# noqa`` is deliberately NOT honored: silencing this rule
    should name it.
    """
    try:
        tree = ast.parse(source, filename=path)
    except SyntaxError:
        return []
    visitor = TryExceptVisitor()
    visitor.visit(tree)

    lines = source.splitlines()
    kept: list[Violation] = []
    for violation in visitor.violations:
        line = lines[violation.line - 1] if 0 < violation.line <= len(lines) else ""
        if violation.code in _suppressed_codes(line):
            continue
        kept.append(violation)
    return kept


def collect_python_files(
    paths: list[str],
    excludes: list[str],
) -> list[Path]:
    """Walk *paths* and return all .py files, filtering out *excludes*."""
    result: list[Path] = []
    exclude_parts = [e.replace("\\", "/").strip("/") for e in excludes]

    for p_str in paths:
        p = Path(p_str)
        if p.is_file() and p.suffix == ".py":
            if not _is_excluded(p, exclude_parts):
                result.append(p)
        elif p.is_dir():
            for py_file in p.rglob("*.py"):
                if not _is_excluded(py_file, exclude_parts):
                    result.append(py_file)
    return result


def _is_excluded(path: Path, exclude_parts: list[str]) -> bool:
    """Return True if any component of *path* matches an exclude pattern."""
    path_str = path.as_posix()
    for exc in exclude_parts:
        if exc in path_str:
            return True
    return False


def find_candidates(files: list[Path]) -> list[tuple[Path, str]]:
    """Read files and return those whose source matches the regex pre-filter."""
    candidates: list[tuple[Path, str]] = []
    for f in files:
        try:
            source = f.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if has_broad_except(source):
            candidates.append((f, source))
    return candidates


# ---------------------------------------------------------------------------
# CLI entry-point
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Check Python files for proper KeyboardInterrupt handling.",
    )
    parser.add_argument(
        "paths",
        nargs="+",
        help="Files or directories to check",
    )
    parser.add_argument(
        "--exclude",
        nargs="*",
        default=[],
        help="Substrings to exclude from file paths",
    )
    args = parser.parse_args(argv)

    files = collect_python_files(args.paths, args.exclude)
    candidates = find_candidates(files)

    total_violations = 0
    for path, source in candidates:
        violations = check_file(str(path), source)
        for v in violations:
            print(f"{path}:{v.line}:{v.col}: {v}")
            total_violations += 1

    return 1 if total_violations > 0 else 0


if __name__ == "__main__":
    sys.exit(main())
