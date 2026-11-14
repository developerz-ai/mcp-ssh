Run `bin/check` (fmt --check + clippy -D warnings + test) and fix anything it flags. Then review the working diff against CLAUDE.md: SRP and ≤300 non-test LOC per file (tests in a sibling `tests.rs`), the CLAUDE.md module map naming every `src` file (`tests/module_map.rs`), typed errors, no `unwrap`/`expect` on the request path, no secrets in logs/responses/errors, no new MCP tools, tracing span on every tool dispatch. Report findings; fix the clear ones.

$ARGUMENTS
