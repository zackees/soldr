#!/usr/bin/env python3
"""Enforce the exact allowlist for platform cfg inside non-private Rust functions."""

from __future__ import annotations

import argparse
import re
from pathlib import Path

PLATFORM_SELECTORS = (
    "windows",
    "unix",
    "target_os",
    "target_family",
    "target_arch",
    "target_abi",
    "target_env",
    "target_vendor",
    "target_endian",
    "target_pointer_width",
)
CFG_STARTS = ("#[cfg(", "#[cfg_attr(", "cfg!(")
ATTRIBUTE_CFG_STARTS = ("#[cfg(", "#[cfg_attr(")
PUBLIC_FUNCTION = re.compile(
    r"(?m)^[ \t]*pub(?:\s*\((?P<scope>[^)]*)\))?\s+"
    r"(?:(?:async|unsafe|const)\s+|extern(?:\s+\"[^\"]*\")?\s+)*"
    r"fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
    r"[^;{]*\{"
)
INLINE_MODULE = re.compile(r"\bmod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\{")


def outer_attributes_start(masked_source: str, function_start: int) -> int:
    """Return the start of the contiguous outer attributes on a function."""
    start = function_start
    while True:
        cursor = start
        while cursor and masked_source[cursor - 1].isspace():
            cursor -= 1
        if cursor == 0 or masked_source[cursor - 1] != "]":
            return start
        depth = 1
        opening = cursor - 2
        while opening >= 0 and depth:
            if masked_source[opening] == "]":
                depth += 1
            elif masked_source[opening] == "[":
                depth -= 1
            opening -= 1
        hash_offset = opening
        if depth or hash_offset < 0 or masked_source[hash_offset] != "#":
            return start
        start = hash_offset


def test_only(attributes: str) -> bool:
    compact = "".join(attributes.split())
    return "#[cfg(test)]" in compact


def mask_comments_and_strings(source: str) -> str:
    """Replace comments and string contents with spaces while preserving offsets."""
    chars = list(source)
    output = list(source)
    index = 0
    while index < len(chars):
        raw_match = re.match(r"(?:br|r)(?P<hashes>#{0,255})\"", source[index:])
        if raw_match:
            start = index
            terminator = '"' + raw_match.group("hashes")
            index += raw_match.end()
            end = source.find(terminator, index)
            index = len(chars) if end < 0 else end + len(terminator)
            for offset in range(start, index):
                if output[offset] != "\n":
                    output[offset] = " "
        elif source.startswith("//", index):
            end = source.find("\n", index)
            end = len(chars) if end < 0 else end
            output[index:end] = " " * (end - index)
            index = end
        elif source.startswith("/*", index):
            start = index
            depth = 1
            index += 2
            while index < len(chars) and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
            for offset in range(start, index):
                if output[offset] != "\n":
                    output[offset] = " "
        elif (
            chars[index] == "'"
            and index + 2 < len(chars)
            and (chars[index + 2] == "'" or chars[index + 1] == "\\")
        ) or source.startswith("b'", index):
            start = index
            index += 2 if source.startswith("b'", index) else 1
            escaped = False
            while index < len(chars):
                character = chars[index]
                index += 1
                if character == "'" and not escaped:
                    break
                escaped = character == "\\" and not escaped
                if character != "\\":
                    escaped = False
            for offset in range(start, min(index, len(chars))):
                if output[offset] != "\n":
                    output[offset] = " "
        elif chars[index] == '"' or source.startswith('b"', index):
            start = index
            index += 2 if source.startswith('b"', index) else 1
            while index < len(chars):
                if chars[index] == "\\":
                    index += 2
                    continue
                index += 1
                if chars[index - 1] == '"':
                    break
            for offset in range(start, min(index, len(chars))):
                if output[offset] != "\n":
                    output[offset] = " "
        else:
            index += 1
    return "".join(output)


def balanced_invocation(source: str, offset: int) -> str | None:
    """Return one complete cfg-style invocation, including nested parentheses."""
    depth = 0
    saw_open = False
    for end in range(offset, len(source)):
        if source[end] == "(":
            saw_open = True
            depth += 1
        elif source[end] == ")" and saw_open:
            depth -= 1
            if depth == 0:
                return source[offset : end + 1]
    return None


def platform_cfg_invocations(
    masked_source: str, starts: tuple[str, ...] = CFG_STARTS
) -> list[str]:
    compact = "".join(masked_source.split())
    invocations: list[str] = []
    for start in starts:
        search_from = 0
        while (offset := compact.find(start, search_from)) >= 0:
            clause = balanced_invocation(compact, offset)
            if clause and any(selector in clause for selector in PLATFORM_SELECTORS):
                invocations.append(clause.removeprefix("#["))
            search_from = offset + len(start)
    return invocations


def platform_cfg_qualifiers(attributes: str) -> list[str]:
    compact = "".join(attributes.split())
    qualifiers: list[str] = []
    for start in ATTRIBUTE_CFG_STARTS:
        search_from = 0
        while (offset := compact.find(start, search_from)) >= 0:
            clause = balanced_invocation(compact, offset)
            if clause and any(selector in clause for selector in PLATFORM_SELECTORS):
                qualifiers.append(clause.removeprefix("#["))
            search_from = offset + len(start)
    return qualifiers


def contains_platform_cfg(masked_source: str) -> bool:
    return bool(platform_cfg_invocations(masked_source))


def matching_brace(masked_source: str, opening: int) -> int:
    depth = 0
    for offset in range(opening, len(masked_source)):
        if masked_source[offset] == "{":
            depth += 1
        elif masked_source[offset] == "}":
            depth -= 1
            if depth == 0:
                return offset
    raise ValueError(f"unclosed function body at byte {opening}")


def inline_module_ranges(masked_source: str) -> list[tuple[int, int, str]]:
    ranges = []
    for match in INLINE_MODULE.finditer(masked_source):
        opening = match.end() - 1
        ranges.append(
            (opening, matching_brace(masked_source, opening), match.group("name"))
        )
    return ranges


def qualified_key(
    relative: str,
    name: str,
    modules: list[str],
    attributes: str,
) -> str:
    path = "::".join((relative, *modules, name))
    qualifiers = platform_cfg_qualifiers(attributes)
    return f"{path}@{'+'.join(qualifiers)}" if qualifiers else path


def violations(source_root: Path) -> set[str]:
    candidates: list[str] = []
    for path in sorted(source_root.rglob("*.rs")):
        if (
            path.name == "tests.rs"
            or path.name.endswith("_tests.rs")
            or "tests" in path.parts
        ):
            continue
        source = path.read_text(encoding="utf-8")
        masked = mask_comments_and_strings(source)
        module_ranges = inline_module_ranges(masked)
        for match in PUBLIC_FUNCTION.finditer(masked):
            scope = "".join((match.group("scope") or "").split())
            if scope in {"self", "inself"}:
                continue
            modules = [
                name
                for opening, closing, name in module_ranges
                if opening < match.start() < closing
            ]
            if "tests" in modules:
                continue
            attributes_start = outer_attributes_start(masked, match.start())
            attributes = masked[attributes_start : match.start()]
            if test_only(attributes):
                continue
            opening = match.end() - 1
            closing = matching_brace(masked, opening)
            if contains_platform_cfg(masked[attributes_start : closing + 1]):
                relative = path.relative_to(source_root.parents[2]).as_posix()
                candidates.append(
                    qualified_key(
                        relative,
                        match.group("name"),
                        modules,
                        source[attributes_start : match.start()],
                    )
                )

    if len(candidates) != len(set(candidates)):
        duplicates = sorted(key for key in set(candidates) if candidates.count(key) > 1)
        raise ValueError(f"ambiguous platform cfg allowlist keys: {duplicates}")
    return set(candidates)


def read_allowlist(path: Path) -> set[str]:
    return {
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }


def verify(source_root: Path, allowlist_path: Path) -> list[str]:
    actual = violations(source_root)
    allowed = read_allowlist(allowlist_path)
    messages = [f"new violation: {key}" for key in sorted(actual - allowed)]
    messages.extend(f"stale allowlist entry: {key}" for key in sorted(allowed - actual))
    return messages


def main() -> int:
    repo = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source-root", type=Path, default=repo / "crates/soldr-cli/src"
    )
    parser.add_argument(
        "--allowlist",
        type=Path,
        default=repo / "dylints/ban_platform_cfg_in_public_fn/src/allowlist.txt",
    )
    args = parser.parse_args()
    messages = verify(args.source_root, args.allowlist)
    if messages:
        print("platform-neutral public function ratchet failed:")
        for message in messages:
            print(f"  {message}")
        return 1
    print(
        f"platform-neutral public function ratchet: {len(read_allowlist(args.allowlist))} allowed"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
