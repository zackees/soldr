from __future__ import annotations

import sys

from ci import linux_docker


def main(argv: list[str] | None = None) -> int:
    return linux_docker.main(["debug", *(argv or [])])


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
