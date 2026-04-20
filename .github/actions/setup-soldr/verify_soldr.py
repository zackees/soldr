#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import subprocess
import sys


def main() -> None:
    binary = os.environ["SETUP_SOLDR_PATH"]
    output_path = os.environ["GITHUB_OUTPUT"]

    version_json = subprocess.check_output([binary, "version", "--json"], text=True)
    payload = json.loads(version_json)

    with open(output_path, "a", encoding="utf-8") as fh:
        fh.write(f"soldr_version={payload['soldr_version']}\n")

    subprocess.run(["cargo", "--version"], check=True)
    subprocess.run(["rustc", "--version"], check=True)
    subprocess.run(["soldr", "status", "--json"], check=True, stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as exc:
        sys.exit(exc.returncode)
