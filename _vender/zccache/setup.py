"""Dynamic version: reads from workspace Cargo.toml so version is defined once."""

import re
from pathlib import Path

from setuptools import setup

_cargo = (Path(__file__).parent / "Cargo.toml").read_text()
_match = re.search(r'\[workspace\.package\]\s*\nversion\s*=\s*"([^"]+)"', _cargo)
assert _match, "Could not find [workspace.package] version in Cargo.toml"

setup(version=_match.group(1))
