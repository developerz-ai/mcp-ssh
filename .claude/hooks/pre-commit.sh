#!/usr/bin/env bash
# Agent pre-commit gate. When the git hook is active (bin/setup), it already runs
# bin/check on this commit, so do not run the suite twice.
set -euo pipefail
cd "$(dirname "$0")/../.."
[ "$(git config core.hooksPath || true)" = ".githooks" ] && exit 0
status=0
bin/check || status=$?
if [[ $status == 79 ]]; then
  echo "pre-commit: partial grade accepted; CI grades deferred coverage" >&2
  exit 0
fi
exit "$status"
