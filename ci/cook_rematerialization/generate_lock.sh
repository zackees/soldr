#!/usr/bin/env bash
set -euo pipefail

cd /repo/tests/fixtures/cook-rematerialization
ZCCACHE_DISABLE=1 /tools/soldr cargo generate-lockfile
