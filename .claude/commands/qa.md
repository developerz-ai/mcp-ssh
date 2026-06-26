Run `bin/check` (fmt --check + clippy -D warnings + test) and fix anything it flags. Then review the working diff against CLAUDE.md: SRP and ≤300 LOC per file, typed errors, no `unwrap`/`expect` on the request path, no secrets in logs/responses/errors, no new MCP tools, tracing span on every tool dispatch. Report findings; fix the clear ones.

$ARGUMENTS
