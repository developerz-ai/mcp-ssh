Execute the agreed plan. Smallest diff that works — match existing style, no drive-by refactors. Write the test that proves each change (`#[cfg(test)]` for logic — in the sibling `tests.rs` when the file has one — and `tests/e2e/` for behaviour over HTTP against the real binary), then make it pass. Follow the conventions in CLAUDE.md: typed errors, no `unwrap`/`expect` outside main+tests, no new MCP tools (parametrize the surface). Finish by running `bin/check`.

$ARGUMENTS
