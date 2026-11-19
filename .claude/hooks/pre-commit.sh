#!/usr/bin/env bash
# Agent pre-commit gate. When the git hook is active (bin/setup), it already runs
# bin/check on this commit — don't run the suite twice.
set -euo pipefail
cd "$(dirname "$0")/../.."
[ "$(git config core.hooksPath || true)" = ".githooks" ] && exit 0

# bin/check's exit contract: 0 and 79 mean commit, 75 and 1 mean stop.
status=0
bin/check || status=$?
case "$status" in
  0) exit 0 ;;
  79)
    echo "pre-commit: bin/check graded everything it could here (exit 79) — committing; CI grades the rest." >&2
    exit 0
    ;;
  75)
    echo "pre-commit: bin/check could not run here (exit 75) — the environment is at fault, not this change." >&2
    exit 75
    ;;
  *)
    echo "pre-commit: bin/check failed (exit $status) — this change is red." >&2
    exit "$status"
    ;;
esac
