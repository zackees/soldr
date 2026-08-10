from __future__ import annotations

import sys

from ci import linux_docker


def main(argv: list[str] | None = None) -> int:
    args = list(argv or [])
    return linux_docker.main(["pytest", *args])


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
