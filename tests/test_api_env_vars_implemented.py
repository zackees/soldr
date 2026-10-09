"""Every SOLDR_* env var documented in docs/API.md must be implemented (soldr#3608).

SOLDR_LOG, SOLDR_OFFLINE and SOLDR_QUIET_DEFENDER were documented but never
existed in code, so a user setting SOLDR_OFFLINE=1 believed fetches were
disabled when they were not. Docs cannot satisfy themselves: only
implementation files count.
"""

from __future__ import annotations

import re
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
API_DOC = REPO_ROOT / "docs" / "API.md"

# Implementation roots searched, defined once. Directories are walked
# recursively; files are read directly. Docs (*.md) and tests/ are excluded.
IMPLEMENTATION_ROOTS = (
    "crates",
    "src",
    "ci",
    ".github/scripts",
    ".github/actions",
    "scripts",
    "bench",
    "perf",
    "install",
)
SKIP_DIRS = {"target", ".git", ".venv", "node_modules", "__pycache__", "tests"}
MAX_BYTES = 2_000_000
ENV_RE = re.compile(r"SOLDR_[A-Z0-9_]+")


def documented_env_vars(text: str) -> set[str]:
    return set(ENV_RE.findall(text))


def _implementation_files():
    for root in IMPLEMENTATION_ROOTS:
        base = REPO_ROOT / root
        if base.is_file():
            yield base
            continue
        stack = [base]
        while stack:
            d = stack.pop()
            try:
                entries = list(d.iterdir())
            except OSError:
                continue
            for p in entries:
                if p.is_dir():
                    if p.name not in SKIP_DIRS:
                        stack.append(p)
                elif p.suffix != ".md":
                    yield p


def _implementation_corpus() -> str:
    chunks = []
    for p in _implementation_files():
        try:
            if p.stat().st_size > MAX_BYTES:
                continue
            data = p.read_bytes()
        except OSError:
            continue
        if b"\0" in data[:4096]:
            continue
        chunks.append(data.decode("utf-8", errors="ignore"))
    return "\n".join(chunks)


def missing_env_vars(doc_text: str) -> list[str]:
    corpus = _implementation_corpus()
    return sorted(n for n in documented_env_vars(doc_text) if n not in corpus)


def test_documented_env_vars_are_implemented() -> None:
    missing = missing_env_vars(API_DOC.read_text(encoding="utf-8"))
    assert not missing, (
        "docs/API.md documents env vars that no implementation file mentions: "
        + ", ".join(missing)
        + ". Implement them or remove them from docs/API.md (soldr#3608)."
    )
