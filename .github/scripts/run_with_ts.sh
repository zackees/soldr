#!/usr/bin/env bash
# Wrapper installed as the `defaults.run.shell` for the cross-compile
# workflow so every `run:` step's output gets prefixed with elapsed
# seconds-since-step-start (preserving ANSI color codes).
#
# GitHub Actions invokes a step's shell as `<shell-cmd> {0}` where
# `{0}` is the path to the temp file holding the `run:` body. We pass
# that path to bash with the same flags GHA's default bash uses
# (`--noprofile --norc -eo pipefail`), then pipe the merged stdout+stderr
# through `ts_step.py`. The pipeline's first non-zero exit code wins so
# the step's true failure reason surfaces instead of the timestamper's.
#
# Usage (from YAML, not interactively):
#   defaults:
#     run:
#       shell: bash .github/scripts/run_with_ts.sh {0}

set -o pipefail

script_path="$1"
ts_helper="$(dirname "$0")/ts_step.py"

bash --noprofile --norc -eo pipefail "$script_path" 2>&1 \
    | python3 -u "$ts_helper"

codes=("${PIPESTATUS[@]}")
for code in "${codes[@]}"; do
    if [ "$code" -ne 0 ]; then
        exit "$code"
    fi
done
exit 0
