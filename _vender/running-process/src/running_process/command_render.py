from __future__ import annotations


def list2cmdline(command: list[str]) -> str:
    parts: list[str] = []
    for argument in command:
        if not argument:
            parts.append('""')
            continue

        need_quotes = any(character in " \t" for character in argument)
        if not need_quotes and '"' not in argument and "\\" not in argument:
            parts.append(argument)
            continue

        rendered: list[str] = ['"'] if need_quotes or '"' in argument or "\\" in argument else []
        backslashes = 0
        for character in argument:
            if character == "\\":
                backslashes += 1
                continue
            if character == '"':
                rendered.append("\\" * (backslashes * 2 + 1))
                rendered.append('"')
                backslashes = 0
                continue
            if backslashes:
                rendered.append("\\" * backslashes)
                backslashes = 0
            rendered.append(character)
        if backslashes:
            rendered.append("\\" * (backslashes * 2 if rendered else backslashes))
        if rendered and rendered[0] == '"':
            rendered.append('"')
        parts.append("".join(rendered) if rendered else argument)
    return " ".join(parts)
