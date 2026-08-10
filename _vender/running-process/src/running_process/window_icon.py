"""Set the host console/terminal window icon (#577).

Why this reports a capability instead of just doing it
------------------------------------------------------

Most terminals do not let a running program change their window icon, and they
do not say so — the underlying call succeeds and nothing happens. Windows
Terminal is the case that matters: it hosts the session in a pseudo-console
whose window handle is real but hidden, so setting an icon on it succeeds
against a window nobody can see.

A function that returned quietly there would be worse than one that failed: you
would ship a feature that silently does nothing on the default terminal of
every recent Windows install, with no signal anything was wrong.

So :func:`icon_support` reports whether it will work and why not, and
:func:`set_host_icon` raises rather than pretending.

Supported today: the classic Windows console (``conhost.exe``). Everything
else — Windows Terminal, macOS Terminal.app and iTerm2, Wayland compositors,
Alacritty, Ghostty — deliberately reserves the window decoration to the
terminal, and no in-process API changes that.
"""

from __future__ import annotations

from enum import Enum


class StockIcon(str, Enum):
    """An icon the operating system already provides.

    An enum rather than a bare string so the valid set is discoverable — a
    caller can see the options without consulting docs, and a typo is caught
    at the call site instead of at runtime.

    Subclasses ``str`` so an existing caller passing ``"warning"`` keeps
    working; the enum is the discoverable form, not a new requirement.
    """

    APPLICATION = "application"
    WARNING = "warning"
    ERROR = "error"
    INFORMATION = "information"
    SHIELD = "shield"


class IconUnsupportedError(RuntimeError):
    """The host terminal will never accept an icon from this process.

    Distinct from a bad icon file: retrying, or supplying different data, will
    not help. Callers should stop asking rather than treat it as transient.
    """


def _native_module():
    """Return the native extension, or ``None`` if it lacks icon support."""
    try:
        from running_process import _native
    except ImportError:
        return None
    if not hasattr(_native, "native_window_icon_support"):
        return None
    return _native


def icon_support(pid: int | None = None) -> str | None:
    """Why the window cannot accept an icon, or ``None`` when it can.

    With no ``pid`` this asks about the calling process's own console window;
    with one, about that child's. A child that inherited this console has none
    *of its own* and is reported unsupported — targeting it would change this
    process's icon too, which is not what the caller asked for.

    Returning the reason rather than a bare bool is deliberate: a caller that
    only learns "no" has nothing to log, and cannot tell "this terminal never
    allows it" from "this process has no console attached right now".
    """
    native = _native_module()
    if native is None:
        return "this build of running_process._native has no window-icon support"
    return native.native_window_icon_support(pid)


def is_supported(pid: int | None = None) -> bool:
    """Whether the window will accept an icon."""
    return icon_support(pid) is None


def set_host_icon(path: str, pid: int | None = None) -> None:
    """Set this process's host console window icon from a ``.ico`` file.

    Raises :class:`IconUnsupportedError` when the terminal cannot accept one,
    and ``OSError`` when the file itself cannot be loaded — different problems
    with different remedies.
    """
    native = _native_module()
    if native is None:
        raise IconUnsupportedError(
            "this build of running_process._native has no window-icon support"
        )
    try:
        native.native_set_window_icon_from_path(str(path), pid)
    except RuntimeError as exc:
        # The native layer raises RuntimeError only for an unsupported host;
        # a bad file arrives as OSError and is left to propagate unchanged.
        raise IconUnsupportedError(str(exc)) from exc


def set_host_icon_from_bytes(data: bytes, pid: int | None = None) -> None:
    """Set the host console window icon from ``.ico`` bytes.

    Takes the data rather than a path so a packaged application can embed its
    icon and never depend on a file existing at runtime, which is the case an
    installed wheel actually has.

    Raises :class:`IconUnsupportedError` when the terminal cannot accept an
    icon, and ``ValueError`` when the bytes are not a usable icon — the
    caller's data being wrong is a different problem from the terminal
    refusing, and only one of them is worth retrying with different input.
    """
    native = _native_module()
    if native is None or not hasattr(native, "native_set_window_icon_from_bytes"):
        raise IconUnsupportedError(
            "this build of running_process._native has no window-icon support"
        )
    try:
        native.native_set_window_icon_from_bytes(bytes(data), pid)
    except RuntimeError as exc:
        # ValueError is also a subclass of Exception but not of RuntimeError,
        # so malformed data propagates untouched; only the unsupported-host
        # RuntimeError is retyped.
        raise IconUnsupportedError(str(exc)) from exc


def set_host_icon_stock(icon: StockIcon | str, pid: int | None = None) -> None:
    """Set the host console window icon to one the OS provides.

    Accepts a :class:`StockIcon` or its string value. Raises
    :class:`IconUnsupportedError` when the terminal cannot accept an icon, and
    ``ValueError`` — naming the valid options — for an unknown icon.
    """
    native = _native_module()
    if native is None or not hasattr(native, "native_set_window_icon_stock"):
        raise IconUnsupportedError(
            "this build of running_process._native has no window-icon support"
        )
    name = icon.value if isinstance(icon, StockIcon) else str(icon)
    try:
        native.native_set_window_icon_stock(name, pid)
    except RuntimeError as exc:
        # Only the unsupported-host RuntimeError is retyped; ValueError for an
        # unknown name propagates untouched, since it is a different problem.
        raise IconUnsupportedError(str(exc)) from exc
