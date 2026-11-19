#!/usr/bin/env bash
# Agent pre-push gate. Every commit already passed bin/check via the git
# pre-commit hook when it is active (bin/setup); otherwise gate the push here.
set -euo pipefail
cd "$(dirname "$0")/../.."
[ "$(git config core.hooksPath || true)" = ".githooks" ] && exit 0

# bin/check's exit contract: 0 and 79 permit the push, 75 and 1 stop it.
status=0
bin/check || status=$?
case "$status" in
  0) exit 0 ;;
  79)
    echo "pre-push: bin/check graded everything it could here (exit 79) — pushing; CI grades the rest." >&2
    exit 0
    ;;
  75)
    echo "pre-push: bin/check could not run here (exit 75) — the environment is at fault, not this change." >&2
    exit 75
    ;;
  *)
    echo "pre-push: bin/check failed (exit $status) — this change is red." >&2
    exit "$status"
    ;;
esac
