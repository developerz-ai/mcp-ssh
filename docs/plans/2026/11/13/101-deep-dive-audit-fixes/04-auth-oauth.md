# 04 — Auth + OAuth hardening

> Part of [`overview.md`](overview.md). Depends on: 03 (migration framework, file modes, store error type).

No bypass of `/mcp` was found: `require_auth` guards it, Basic is rejected there, bad Host with a valid bearer → 403 ([E2E]). The weaknesses are in how a token can be obtained or kept.

**Secrets rule for this slice:** no password, token, authorization code, or PKCE verifier in any log line, span, error body, `Debug` output, id or filename. Every new log event in the auth path gets a test asserting that.

## Findings
| ID | Sev | Finding | Evidence |
|---|---|---|---|
| A1 | HIGH | `/authorize` mints a code on a bare GET with Basic creds — no consent/CSRF step. `/register` is open. Attacker registers `https://attacker/cb`, gets the operator's browser (cached Basic creds / autofill) to load the authorize URL → code → root-capable bearer | [READ] `src/oauth/flow.rs:34-105`, `src/oauth/mod.rs:42`; browser caching not exercised |
| A2 | HIGH | Tokens stored plaintext (`token TEXT PRIMARY KEY`); any DB read/backup = live bearer + 365-day refresh | [READ] `src/db.rs:17-24`, `src/oauth/store.rs:216-223` (file mode: slice 03 D1) |
| A3 | HIGH | No revocation: `set-auth` leaves every token valid; refresh window resets on each use; no family/reuse detection; `sessions` only counts | [READ] `src/oauth/store.rs:26,173-197`, `src/config.rs:188-204`, `src/admin.rs:103` |
| A4 | HIGH | Empty password/user accepted from env and overrides a strong file value (`opt` = `env.get(key).or(file)`, no empty filter) | [E2E] `MCP_SSH_PASS=` → server listens. `src/config.rs:97-98,259-261` |
| A5 | MED | Loopback redirect check bypassed by userinfo with a colon: `http://localhost:1@evil.example/cb` accepted | [E2E] `/register` returned 201 with that URI. `src/oauth/flow.rs:229-240` |
| A6 | MED | `/register`: unauthenticated, no cap on URI count/length (up to axum's 2 MiB), rows never swept (no timestamp column). Fills the state volume; each insert takes the one connection | [E2E] 1.5 MB URI → 201. `src/oauth/flow.rs:162-205`, `src/db.rs:29-33` |
| A7 | MED | OAuth routes have no Host allowlist; with `public_url` unset, issuer/endpoints are built from request `Host` + verbatim `X-Forwarded-Proto` | [E2E] `Host: evil.example` + `X-Forwarded-Proto: javascript` → `"issuer":"javascript://evil.example"`. `src/app.rs:16,28`, `src/oauth/metadata.rs:10-30` |
| A8 | MED | `WWW-Authenticate` uses `resource_metadata_url`; RFC 9728 §5.1 / MCP spec say `resource_metadata`. And with `public_url` unset the 401 carries **no** challenge at all | [E2E] no `WWW-Authenticate` on 401. `src/auth.rs:88-89`; tests at `src/auth.rs:154`, `src/app_tests.rs:169` pin the wrong name |
| A9 | MED | Failed logins: no log event (nothing for fail2ban/alerting), no backoff; shipped proxy examples have no rate limit | [READ] `src/oauth/flow.rs:39-47` (proxy examples: slice 06) |
| A10 | MED | Password plaintext in `config.toml` and in `#[derive(Debug)]` structs (`Config`, `FileConfig`); TOML parse errors may echo the `pass = …` source line into journald/Sentry | [READ] `src/config.rs:11-15,30-34,224`; snippet behaviour [PLAUS] |
| A11 | LOW | Refresh rotation = two commits (delete, then issue) → a failure between them burns the client's only refresh token | [READ] `src/oauth/store.rs:178-196` |
| A12 | LOW | `/token` internal failure returns 400 → clients discard a valid refresh token on a DB blip | [READ] `src/oauth/flow.rs:155` |
| A13 | LOW | PKCE inputs unvalidated (challenge any string, verifier any length/charset; RFC 7636: 43–128 unreserved) | [READ] `src/oauth/flow.rs:82-87`, `src/oauth/store.rs:319-324` |
| A14 | LOW | Codes/tokens not bound to `client_id` (blocks per-client revoke + family tracking) | [READ] |
| A15 | LOW | Fragment allowed in redirect URIs (`https://x/cb#f`) | [READ] `src/oauth/flow.rs:90-100,224-228` |
| A16 | LOW | Long-lived SSE stream outlives token expiry/deletion (validated per request only) | [PLAUS] `src/auth.rs:60-70` |
| A17 | LOW | `bin/mcp-token` discards the refresh token (valid 1 year, unrevocable) and registers a permanent client per run; header comment says tokens reset on restart (false) | [E2E] 2 runs → 2 access + 2 refresh rows |

## Files to change
- `src/oauth/flow.rs` — consent GET/POST, redirect parsing, register caps, PKCE validation, 5xx mapping, login-failure event.
- `src/oauth/store.rs` — hashed tokens, `client_id`/family columns, single-txn refresh, `revoke_all`, client sweep.
- `src/oauth/metadata.rs` — scheme allowlist; prefer `public_url`.
- `src/oauth/mod.rs`, `src/app.rs` — host allowlist layer on the OAuth router.
- `src/auth.rs:88-89` — header parameter name; challenge without `public_url`.
- `src/config.rs` — non-empty credential newtype with redacted `Debug`; `set_auth` revokes sessions; scrub parse errors.
- `src/admin.rs`, `src/cli.rs` — `mcp-ssh sessions --revoke` (a flag on the existing subcommand).
- `src/jobs/reaper.rs:66-67` — add the client sweep to the existing pass.
- `src/db.rs` — migrations (via slice 03 step 2).
- `bin/mcp-token` — follow the new flow; fix comment.

## Steps
1. **Empty creds** (A4) — first, smallest: empty/whitespace `user`/`pass` ⇒ `ConfigError::NoCredentials` (env empty must not shadow the file). Parse into `Credential(String)` with a hand-written redacted `Debug` (A10).
2. **Redirect validation** (A5, A15): parse with `http::Uri` (already in the tree via axum); reject any authority containing `@` or `\`, any fragment; compare `host()` exactly for the loopback rule.
3. **Register caps + sweep** (A6): ≤ 5 URIs, ≤ 2048 bytes each, else 400; migration adds `created_unix`; reaper drops clients with no live token older than 7 days; hard row ceiling ⇒ 429/503.
4. **Hashed tokens + binding** (A2, A14): migration replaces `token` with `token_hash` (SHA-256 hex; 256-bit random input, no salt needed) and adds `client_id`, `family_id`. Existing rows dropped (documented one-time logout). `validate`/`refresh` look up by hash.
5. **Single-txn refresh + reuse detection** (A11, A3): one `BEGIN IMMEDIATE`: delete-returning, expiry check, insert pair. Keep consumed refresh hashes (tombstone until family expiry); presenting one ⇒ delete the family. Absolute family lifetime cap (recommend 90 days — owner decision).
6. **Revocation** (A3): `oauth::Store::revoke_all`; `mcp-ssh set-auth` calls it (prints the count, never values); `mcp-ssh sessions --revoke` exposes it. Both open the DB via `open_existing` (03).
7. **Consent step** (A1): `GET /authorize` validates params and renders a minimal HTML page (client redirect host shown, username/password form, CSRF token bound to the params, `Content-Security-Policy: frame-ancestors 'none'`, `Cache-Control: no-store`). `POST /authorize` checks CSRF + credentials (constant-time path unchanged) and only then 302s with `code`. Drop the Basic-on-GET issuance. `auth::check_basic` shrinks to form-credential checking. HTML lives in `src/oauth/` as a const; no template engine.
8. **Login failures** (A9): `tracing::warn!(event = "login_failed")` with remote addr from `X-Forwarded-For` only if present — no username, no password; a small global failure delay (e.g. 500 ms sleep on failure). Real rate limiting stays in the proxy (examples: slice 06).
9. **Metadata + host** (A7, A8): apply the same allowed-hosts check to the OAuth router (small axum middleware reusing `cfg.allowed_hosts`); accept only `http`/`https` from `X-Forwarded-Proto`; when `public_url` is set never read `Host`. Fix the header to `resource_metadata="…"` and always emit it (derive from the validated Host when `public_url` is unset). Update the two tests that pin the wrong name to assert the full header.
10. **PKCE** (A13): challenge must be 43-char base64url; verifier 43–128 unreserved chars.
11. **`/token` errors** (A12): `ServerError` ⇒ 503 with `Cache-Control: no-store`.
12. **Password at rest** (A10 — owner decision, see overview): if approved, `set-auth` writes an argon2id `pass_hash`; loader accepts `pass_hash` (file) or plaintext `MCP_SSH_PASS` (env); legacy plaintext `pass` in file still loads with a warn. If declined, keep plaintext 0600 and only do the redacted-`Debug` + parse-error scrub.
13. **`bin/mcp-token`** (A17): GET (extract CSRF) + POST; print access token; revoke nothing automatically but fix the comment and mention `sessions --revoke`.
14. **A16:** verify rmcp stream lifetime first; if streams do outlive the token, close sessions on revoke (`LocalSessionManager`) — only then.

## Tests
- Config: `MapEnv` with `MCP_SSH_PASS=""` + file password ⇒ never an empty password; `format!("{cfg:?}")` lacks the password; broken `pass` line ⇒ error text lacks the value.
- Redirects: `http://localhost:1@evil.com/cb`, `http://127.0.0.1:80@evil.com/`, `https://x/cb#f` rejected.
- Store: after `issue()` no row equals the returned token; refresh with `access_tokens` dropped leaves the refresh row; replaying a rotated refresh token kills its successor; `revoke_all` ⇒ `validate` false.
- Flow over the **router** (today all OAuth tests call handlers directly): `/register` → `GET /authorize` (no `code=` in any response) → `POST /authorize` → `/token`; wrong CSRF ⇒ 403; expired code; refresh-as-access and access-as-refresh rejected; `Cache-Control: no-store` on both grants; >N URIs ⇒ 400.
- Host guard: valid bearer + disallowed `Host` ⇒ **403** (fix `mcp_with_bearer_auth_passes_the_guard`, `src/app_tests.rs:175-194`, which only asserts "not 401"); metadata with a disallowed Host ⇒ 403; `X-Forwarded-Proto: javascript` not reflected.
- No-leak: 400/401/403/500 bodies and captured log events contain no password/token/code.
- Manual before release: connect a real Claude client end to end through the consent page. Gate: `bin/check`.

## Done when
- `GET /authorize` can never yield a code; empty creds refuse to start.
- DB contains only token hashes; `set-auth` and `sessions --revoke` invalidate every session.
- OAuth metadata can't be pointed at an arbitrary host/scheme; 401 carries a spec-correct challenge.
- `clients` table is bounded.
