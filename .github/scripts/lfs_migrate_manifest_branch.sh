#!/usr/bin/env bash
# Thin shell wrapper around lfs_migrate_manifest_branch.py so the runbook
# in docs/MANIFEST_LFS_MIGRATION.md can document a single invocation form.
#
# Run from a clean checkout of the `manifest` branch; review the rewritten
# history with `git log` before pushing. This wrapper DOES NOT push.
#
# Owner-runbook context: docs/MANIFEST_LFS_MIGRATION.md §2.3.
#
# Usage:
#   bash .github/scripts/lfs_migrate_manifest_branch.sh           # real run
#   bash .github/scripts/lfs_migrate_manifest_branch.sh --dry-run # plan only
#
# All flags are forwarded verbatim to the Python script. Use --help for
# the full flag list.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/lfs_migrate_manifest_branch.py"

if [[ ! -f "$SCRIPT" ]]; then
    echo "ERROR: cannot find $SCRIPT" >&2
    exit 2
fi

# Prefer python3, fall back to python (Windows Git Bash often only has `python`).
if command -v python3 >/dev/null 2>&1; then
    PY=python3
elif command -v python >/dev/null 2>&1; then
    PY=python
else
    echo "ERROR: neither python3 nor python is on PATH." >&2
    exit 2
fi

exec "$PY" "$SCRIPT" "$@"
