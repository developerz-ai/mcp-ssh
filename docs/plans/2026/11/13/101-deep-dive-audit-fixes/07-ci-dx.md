# 07 — CI/CD, developer experience, efficiency

> Part of [`overview.md`](overview.md). Depends on: 06 for the image/`.deb` smoke tests to pass; the harness itself (steps 1–2) can land first and is useful while doing 01–05.

Baseline measured: `bin/check` cold ≈ 2.5 min, 191 tests in 2.3 s; CI 40–190 s per run; latest release `v1.4.0` (2026-07-21) ships one asset, `mcp-ssh_1.4.0-1_amd64.deb`.

## Findings
| ID | Sev | Finding | Evidence |
|---|---|---|---|
| C1 | HIGH | No end-to-end test: nothing boots the binary and speaks MCP over HTTP. `tests/` doesn't exist (CLAUDE.md says it does); `src/app_tests.rs` uses the router in-process and covers `job list`, one `bash echo`, one file write/read. The 8 top findings in the overview were all invisible to the 191 green tests | [E2E] |
| C2 | HIGH | Release artifacts are never exercised: the `.deb` and the Docker image are built on tag and published untested (see 06 R2–R4 — both documented install paths are broken) | [READ] `.github/workflows/ci.yml` `release`/`docker` jobs |
| C3 | MED | CI doesn't use `--locked` (clippy/test/deb resolve fresh ⇒ a release can differ from the tested lockfile; only the Dockerfile uses `--locked`) | [READ] `ci.yml` |
| C4 | MED | Toolchain is not pinned: `rust-toolchain.toml` says `channel = "stable"`, CI uses `dtolnay/rust-toolchain@stable`, Dockerfile pins `rust-1.98`, `Cargo.toml` says `rust-version = "1.85"` (never checked). CLAUDE.md claims "pinned". A new stable's clippy lints can break `main` with no code change | [READ] |
| C5 | MED | No supply-chain check: no `cargo audit`/`cargo deny` (advisories, licenses, duplicate crates — 16 duplicated crates today, 264 total). Workflow actions are tag-pinned (`@v7`), not SHA-pinned, in a repo whose release job has `contents: write` + `packages: write` | [READ]; a "pin workflow actions to commit shas" PR run exists but `main` is unpinned |
| C6 | MED | Release: no check that the tag matches `Cargo.toml` `version`; no `SHA256SUMS`; no arm64 `.deb` although `install.sh` selects by arch; Docker image single-arch; `release` and `docker` jobs each rebuild from scratch | [E2E] assets list |
| C7 | LOW | `concurrency: cancel-in-progress: true` keyed on `github.ref` also cancels an in-flight `main` push run (and a tag's release) when a newer one starts | [READ] |
| C8 | MED | Dev bootstrap is broken by default: `.env.example` sets `MCP_SSH_JOB_DIR=/var/lib/mcp-ssh/logs/jobs` and no `MCP_SSH_DB` ⇒ `bin/dev` after `cp .env.example .env` tries to create `/var/lib/mcp-ssh` (unwritable). Missing from the example: `MCP_SSH_DB`, `MCP_SSH_PUBLIC_URL`, `MCP_SSH_PORT`. `.gitignore` already reserves `.local/` for exactly this | [READ] `.env.example`, `src/config.rs:107-121` |
| C9 | LOW | Three overlapping pre-commit mechanisms: `.githooks/pre-commit` (full `bin/check`, not activated — `core.hooksPath` unset, `bin/setup` doesn't set it), `.claude/hooks/pre-commit.sh`, `.claude/hooks/pre-push.sh` (plain `cargo test` with `set -e`) | [E2E] `git config core.hooksPath` empty |
| C10 | LOW | `bin/stop` uses `pkill -f '[m]cp-ssh serve'` — kills any matching process on the machine, including a production-like local instance | [READ] |
| C11 | LOW | No `shellcheck` on `bin/*` + `deploy/install.sh` (the installer runs as root via `curl \| sudo bash`) | [READ] |
| C12 | LOW | Dependabot rand bump (#46) has failed since 2026-11-09 — `rand 0.8` is two majors behind; `Dockerfile` base uses a `latest-` tag prefix | [E2E] open issue #46 |
| C13 | INFO | Efficiency is fine: tests 2.3 s, CI < 3.5 min, `lto = "thin"` + `codegen-units = 1`. Only real waste: the tag pipeline builds the release binary twice (deb job + Docker) | measured |

## Files to change
- New `bin/e2e` + `tests/e2e.rs` (or keep it shell-only — see step 1).
- `.github/workflows/ci.yml` — locked builds, audit, e2e job, artifact smoke tests, arm64, checksums, version check, concurrency.
- `rust-toolchain.toml`, `Cargo.toml` (`rust-version`), `Dockerfile` (base tag) — one toolchain version.
- New `deny.toml` (if `cargo deny` chosen).
- `.env.example`, `bin/dev`, `bin/setup`, `bin/stop`, `.githooks/pre-commit`, `.claude/hooks/*`.

## Steps
1. **E2E harness** (C1). Rust integration test in `tests/e2e.rs` using the built binary (`env!("CARGO_BIN_EXE_mcp-ssh")`): spawn `serve` on an ephemeral port with `MCP_SSH_DB`/`MCP_SSH_JOB_DIR`/`MCP_SSH_CONFIG` in a tempdir, run register → authorize → token (PKCE), `initialize`, then tool calls over real HTTP; kill the child in `Drop`. Needs an HTTP client dev-dependency — `reqwest` is already in the tree via `sentry` (add as dev-dep with `rustls`, no default features). This makes CLAUDE.md's "Integration: … in `tests/`" true. Minimum scenario set (one per top finding, each asserting the *fixed* behaviour):
   - auth matrix: no auth / Basic / bad bearer ⇒ 401 with `WWW-Authenticate`; bad Host + good bearer ⇒ 403.
   - `bash` inline, auto-background, `bg`, `timeout`, bad `cwd`.
   - `job` poll paging (limit/cursor), list paging, kill + kill-again wording, kill of a TERM-trapping job.
   - bounded memory: start a long-running quiet job, replace its `<id>.log` content with a sparse 2 GiB file (`set_len`) in the test job dir, poll, assert response time and server RSS (`/proc/<pid>/status`) stay bounded. Never generate real output for this.
   - `file`: write/read/append, `/dev/zero` read errors fast, `delete link/` keeps the target, move edge cases.
   - restart: stop + start on the same DB ⇒ token still valid, history listed, survivors reconciled.
   - shutdown: SIGTERM with an open SSE stream exits within the drain deadline.
   - file modes of DB/logs.
2. `bin/e2e` = `cargo test --test e2e -- --test-threads=1`; `bin/check` keeps running everything (`cargo test` picks up `tests/`). If the e2e suite pushes `bin/check` past ~30 s of test time, keep it in `bin/check` anyway for CI and let the pre-commit hook run `cargo test --bins` only.
3. **CI correctness** (C3, C4, C7): `--locked` on clippy/test/deb; pin `rust-toolchain.toml` to an exact version (match the Dockerfile's) and make CI read it (drop `@stable` input, or pass the same version); set `rust-version` to that or add an MSRV job — recommend the former, one number. Dependabot can't bump `rust-toolchain.toml`; note the manual bump in the release skill. Concurrency: `cancel-in-progress: ${{ github.event_name == 'pull_request' }}`.
4. **Supply chain** (C5): add a `cargo audit` step (binary available via `taiki-e/install-action`); SHA-pin all actions (finish the existing "pin workflow actions" work; Dependabot's `github-actions` ecosystem keeps SHAs fresh); scope `permissions:` to `contents: read` at workflow level, elevate per job.
5. **Artifact smoke tests** (C2), gating publish:
   - Docker: build, `docker run -d -p 127.0.0.1:1337:1337 -e MCP_SSH_USER -e MCP_SSH_PASS -e MCP_SSH_ALLOWED_HOSTS`, `curl /healthz` on the published port, mint a token, run `bash` + `job kill` through it (proves 02 K1 in the image). Run on PRs too (build without push) so a broken Dockerfile is caught before a tag.
   - `.deb`: `cargo deb --locked`, install in a `debian:bookworm` container, assert user exists, `mcp-ssh --version`, `mcp-ssh serve` starts as `mcp-ssh` with env creds and answers `/healthz` (systemd itself isn't needed for this check).
6. **Release** (C6): job step asserting `v${version}` == tag; build arm64 `.deb` (`ubuntu-24.04-arm` runner or `cross`) and a multi-arch image (`platforms: linux/amd64,linux/arm64` — native arm runner preferred over QEMU for build time); publish `SHA256SUMS`; reuse the release binary between the deb job and the image where practical (C13) — optional, only if simple.
7. **Dev bootstrap** (C8): `.env.example` → `MCP_SSH_JOB_DIR=.local/jobs`, `MCP_SSH_DB=.local/mcp-ssh.db`, add commented `MCP_SSH_PUBLIC_URL`/`MCP_SSH_PORT`; `bin/dev` creates `.local/`. Production defaults in `src/config.rs` unchanged.
8. **Hooks** (C9): one gate. `bin/setup` runs `git config core.hooksPath .githooks`; `.claude/hooks/pre-commit.sh` and `pre-push.sh` call `bin/check` (or are removed in favour of the git hook, which also fires for agent commits). Don't run the suite twice per commit.
9. **Small** (C10, C11, C12): `bin/stop` kills only the pid `bin/dev` recorded in `.local/dev.pid`; add a `shellcheck` CI step; do the `rand` 0.10 migration (issue #46 — token generation in `src/oauth/`: keep an OS-seeded CSPRNG, re-verify the entropy test); pin the cargo-chef base to an exact Rust version tag.

## Tests
- The e2e suite is the deliverable; it must fail on `main` today for the unfixed findings (write each scenario against the target behaviour, land it with — or right after — the slice that fixes it; mark not-yet-fixed scenarios `#[ignore = "slice NN"]` rather than weakening them).
- CI: open a PR with a deliberately broken Dockerfile bind ⇒ smoke test red. Tag/version mismatch ⇒ release job red.
- Gate: `bin/check`.

## Done when
- `bin/e2e` boots the real binary and covers every row of the overview's top-findings table.
- A tag can't publish an image or `.deb` that fails to start, and can't publish a version that disagrees with `Cargo.toml`.
- Builds are `--locked` on one pinned toolchain; advisories are checked on every PR.
- `cp .env.example .env && bin/dev` works on a fresh clone with no sudo.
- arm64 users get a `.deb` and an image.
