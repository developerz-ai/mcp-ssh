# 04 — Auth hardening (defense-in-depth)

> Part of [`overview.md`](overview.md). Depends on: none. Lowest priority — no live exploit, all gated behind valid Basic creds. Drop if scope tightens.

Security note: this slice touches `src/oauth/` and the auth/bind surface. Never weaken the auth middleware. Never log/return token or password. Any new persisted client data must not include secrets in logs or ids.

## Files to change
- `src/oauth/flow.rs:140-162` (`register`) + `:31-84` (`authorize`) + `src/oauth/store.rs` — LOW/MEDIUM. DCR persists nothing and `authorize` accepts any `redirect_uri` passing `is_allowed_redirect` (any https host). A victim with valid Basic creds can be lured to an `/authorize` link with an attacker `redirect_uri` + attacker PKCE challenge → code lands at the attacker. Bind `client_id → redirect_uri(s)` at registration (persist in a small table via the store) and require an exact match in `authorize` + token redeem. **Confirm this is wanted** — it changes the currently-stateless `register`; it's defense-in-depth against phishing, not a live bug.
- `src/oauth/flow.rs:181-199` (`is_allowed_redirect`) — LOW. The https branch accepts any non-empty remainder, so `https://?...` (empty host) passes. Also reject empty/whitespace host. Small, do this regardless of the binding decision above.
- `src/config.rs:128-135` (non-loopback bind) — LOW. A non-loopback bind with no explicit `MCP_SSH_ALLOWED_HOSTS` only *warns*, then serves with the default `["localhost","127.0.0.1"]` — which a remote attacker satisfies with `Host: 127.0.0.1`. Consider fail-fast (refuse to serve) instead of warn, so `MCP_SSH_ALLOWED_HOSTS` is mandatory off-loopback (matches CLAUDE.md NEVER "ship without MCP_SSH_ALLOWED_HOSTS set"). **Confirm** — this is a behavior change that could break an existing loose deploy; gate on the bind being genuinely non-loopback.

## Steps
1. `is_allowed_redirect`: reject empty/whitespace host in the https branch (`flow.rs:181-199`). Add a unit test. (Safe, do first.)
2. Decide redirect_uri binding (ask if unsure). If yes: add a `clients` table + store methods, persist redirect_uri(s) at `register`, exact-match at `authorize`/redeem, reject mismatch with `invalid_request`. Keep the flow's existing single-use-code + PKCE guards intact.
3. Decide non-loopback fail-fast vs warn (`config.rs:128-135`). If fail-fast: return a `ConfigError` when bind is non-loopback and `allowed_hosts` is unset/defaulted; add a test for both branches. Keep loopback binds working with the safe default.

## Tests (colocated `#[cfg(test)]`)
- `flow.rs`: `is_allowed_redirect` rejects `https://` empty-host forms; still accepts a valid https URL.
- If binding added: `authorize` with a `redirect_uri` not matching the registered client is rejected; matching one succeeds.
- If fail-fast added: `config` errors on non-loopback bind without explicit allowed-hosts; loopback still defaults cleanly (extend the existing allowed-hosts tests at `config.rs:115-137`).
- Gate: `bin/check`.

## Done when
- Empty-host https redirect URIs are rejected.
- (If approved) redirect_uri is bound to the registered client and enforced end-to-end; non-loopback bind without explicit allowed-hosts fails fast.
- Auth middleware unchanged in strength. `bin/check` green.
