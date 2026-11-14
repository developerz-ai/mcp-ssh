#!/usr/bin/env bash
# Agent pre-commit gate. When the git hook is active (bin/setup), it already runs
# bin/check on this commit — don't run the suite twice.
set -euo pipefail
cd "$(dirname "$0")/../.."
[ "$(git config core.hooksPath || true)" = ".githooks" ] && exit 0
exec bin/check
