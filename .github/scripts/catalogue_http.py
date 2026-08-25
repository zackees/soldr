#!/usr/bin/env python3
"""Shared fail-closed HTTP policy for soldr-toolchain catalogue consumers."""

from __future__ import annotations

import urllib.request
from typing import Any
from urllib.parse import urlsplit, urlunsplit

MAX_URL_BYTES = 8192


def validate_https_url(value: object, *, label: str) -> str:
    """Require a bounded, credential-free absolute HTTPS URL."""
    if not isinstance(value, str) or not value or len(value) > MAX_URL_BYTES:
        raise SystemExit(f"{label} has invalid URL length")
    if any(ord(char) < 32 or ord(char) == 127 for char in value):
        raise SystemExit(f"{label} contains control characters")
    try:
        parsed = urlsplit(value)
        hostname = parsed.hostname
        username = parsed.username
        password = parsed.password
        _ = parsed.port
    except ValueError as error:
        raise SystemExit(f"{label} is not a valid absolute URL: {error}") from error
    if (
        parsed.scheme != "https"
        or hostname is None
        or username is not None
        or password is not None
    ):
        raise SystemExit(f"{label} must be credential-free absolute HTTPS")
    return value


def display_url(value: object) -> str:
    """Render a URL for logs without userinfo, query credentials, or fragments."""
    if not isinstance(value, str):
        return "<invalid-url>"
    try:
        parsed = urlsplit(value)
        hostname = parsed.hostname
        port = parsed.port
    except ValueError:
        return "<invalid-url>"
    if not parsed.scheme or hostname is None:
        return "<invalid-url>"
    display_host = f"[{hostname}]" if ":" in hostname else hostname
    netloc = f"{display_host}:{port}" if port is not None else display_host
    return urlunsplit((parsed.scheme, netloc, parsed.path, "", ""))


class SafeRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Retain the credential-free HTTPS policy across every redirect hop."""

    max_redirections = 5

    # urllib defines this six-argument callback; the override must match it.
    # pylint: disable-next=too-many-positional-arguments
    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: Any,
        code: int,
        msg: str,
        headers: Any,
        newurl: str,
    ) -> urllib.request.Request | None:
        validate_https_url(newurl, label=f"redirect from {display_url(req.full_url)}")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


SAFE_URL_OPENER = urllib.request.build_opener(SafeRedirectHandler())


def open_url(request: urllib.request.Request, *, timeout: int) -> Any:
    """Open a request through the shared redirect-revalidating opener."""
    validate_https_url(request.full_url, label="request URL")
    return SAFE_URL_OPENER.open(request, timeout=timeout)
