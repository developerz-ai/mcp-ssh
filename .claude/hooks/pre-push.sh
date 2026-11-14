#!/usr/bin/env bash
# Agent pre-push gate. Every commit already passed bin/check via the git
# pre-commit hook when it is active (bin/setup); otherwise gate the push here.
set -euo pipefail
cd "$(dirname "$0")/../.."
[ "$(git config core.hooksPath || true)" = ".githooks" ] && exit 0
exec bin/check
