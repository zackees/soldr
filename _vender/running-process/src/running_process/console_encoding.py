"""Console encoding detection and safe-text sanitization.

Windows legacy consoles default to ``cp1252`` while child processes — especially
Python children — overwhelmingly emit UTF-8. Handing the caller a ``str`` that
contains characters the parent console cannot encode causes ``print(...)`` to
raise ``UnicodeEncodeError``.

The helpers here let the Python wrapper layer detect the parent's effective
console encoding once and sanitize text it returns to callers so that a naive
``print()`` is safe on every platform.
"""

from __future__ import annotations

import locale
import os
import sys

_FALLBACK = "utf-8"


def detect_console_encoding(explicit: str | None = None) -> str:
    """Return the best-guess encoding the caller's console can render.

    Priority:

    1. ``explicit`` argument (caller-supplied ``encoding=...``).
    2. ``PYTHONIOENCODING`` env var (Python honors this for stdio).
    3. ``sys.stdout.encoding`` — the live console's code page.
    4. ``locale.getpreferredencoding(False)``.
    5. ``"utf-8"`` as a final fallback.
    """
    if explicit:
        return explicit

    pyio = os.environ.get("PYTHONIOENCODING")
    if pyio:
        return pyio.split(":", 1)[0].strip() or _FALLBACK

    stdout_enc = getattr(sys.stdout, "encoding", None)
    if stdout_enc:
        return stdout_enc

    try:
        loc = locale.getpreferredencoding(False)
    except (locale.Error, ValueError):
        loc = None
    if loc:
        return loc

    return _FALLBACK


def sanitize_for_encoding(text: str, encoding: str) -> str:
    """Round-trip ``text`` through ``encoding`` with ``errors='replace'``.

    Any code point the encoding cannot represent becomes ``?`` (or the codec's
    canonical replacement). The result is guaranteed safe to write to a stream
    using that encoding without raising ``UnicodeEncodeError``.

    UTF-8 (and other Unicode-complete encodings) round-trip losslessly, so this
    is a no-op cost on modern terminals.
    """
    if not text:
        return text
    try:
        return text.encode(encoding, errors="replace").decode(encoding, errors="replace")
    except (LookupError, UnicodeError):
        return text.encode(_FALLBACK, errors="replace").decode(_FALLBACK, errors="replace")
