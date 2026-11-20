# Unreleased

- **The development gate tells a refusal from a red change.** `bin/check` now exits
  75, running no grading command, when a prerequisite is unmet: cargo, rustc, `cargo
  fmt` or `cargo clippy` cannot run (a rustup proxy on PATH is not proof its selected
  toolchain or component works), or `PUBLIC_TREE_PRIVATE_WORDS_REQUIRED=1` while that
  input is unset, blank or delimiter-only. It exits 79 when the private-word coverage
  is optional and absent but `cargo fmt --check`, `cargo clippy --locked --all-targets
  -- -D warnings` and `cargo test --locked` all pass: a partial grade, deferred to CI.
  Any executed fmt, clippy or test failure is 1, including a child that itself exits 75
  or 79. The git hooks accept 0 and 79 and propagate 75 and every other non-zero.
