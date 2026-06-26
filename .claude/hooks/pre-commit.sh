#!/usr/bin/env bash
set -e

cargo fmt
cargo clippy --all-targets -- -D warnings

# Stage any files cargo fmt rewrote so the commit includes them.
if ! git diff --quiet; then
  git add -u
fi
