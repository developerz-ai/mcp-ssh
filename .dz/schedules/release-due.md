# Brief: is a release needed?

Weekly checkup, Tuesdays 03:00 UTC, fired by `schedules: release-due`. It covers what people install: the `v*` tag, the image on GHCR (`ghcr.io/developerz-ai/mcp-ssh`), and the GitHub Release carrying the `.deb` files that `deploy/install.sh` downloads. The block below gathers facts and you make the call. A merge or a count never cuts a release.

## Judge

Read everything merged since the last release: the window below with its diffs, then `git log "$last"..origin/main` for what the pathspec leaves out, and `docs/changelog/unreleased.md`. Does someone running mcp-ssh get something real from a release now?

- Usually yes: a fix they can hit, a feature they can use, a dependency change that reaches the binary.
- Usually no: a run of dev-dependency or CI bumps.

Nothing real: no release. Say why either way: a release ask opens with your reason, and a no ends your run summary with `no release: <reason>`.

## Decide

```bash
git fetch --tags --unshallow origin 2>/dev/null || git fetch --tags origin
last=$(git describe --tags --abbrev=0 --match 'v[0-9]*' origin/main)
git log --format='%h %cI %s' "$last"..origin/main -- src Cargo.toml Cargo.lock Dockerfile deploy/debian deploy/mcp-ssh.service
grep -m1 '^version' Cargo.toml
```

The pathspec names what ships in the binary, the image and the `.deb`. `deploy/install.sh` is served from `main` directly and needs no release.

| Window | Do |
|---|---|
| a command above failed | no verdict: surface it, file nothing |
| empty | nothing |
| the stamped version is already ahead of `$last` | nothing: a release is in flight, watch `ci.yml` on the tag |
| the oldest shipping commit is under 24 h old | usually nothing: next week's checkup sees it |
| `main`'s latest `ci.yml` push run is not `success` | nothing: `health` owns a red `main` |
| a breaking change in the window | no release ask. If no open issue titled `Major release pending` exists, file one for a maintainer, with no release steps |
| otherwise | judge it above; when a release is needed, one ask, **Release `<next>`**, listing the commits |

`<next>`: a `feat` subject in the window is a minor, anything else a patch.

## The ask

Pre-flight guard: only a lane where `gh auth status` exits 0 files the ask. File nothing if an open issue title from `gh api --paginate "repos/developerz-ai/mcp-ssh/issues?state=open" | jq -r '.[] | select(.pull_request == null) | .title'` is exactly `Release <next>`, or an open PR title from `gh api --paginate "repos/developerz-ai/mcp-ssh/pulls?state=open" | jq -r '.[].title'` matches `^chore\(release\):`. A failed read is no verdict. Body, for the box that takes it:

1. Re-run Decide on fresh `main`; still needed, or stop.
2. Bump `version` in `Cargo.toml`, run `cargo update -p mcp-ssh` so `Cargo.lock` carries it. Move `docs/changelog/unreleased.md`'s entries to a new `docs/changelog/<next>.md`, reset `unreleased.md` to its heading, and add the `## [<next>] - <date>` entry at the top of `CHANGELOG.md` (one paragraph, linking the notes; update the `[Unreleased]` diff link). No source change.
3. `bin/check`, then one PR titled `chore(release): <next>`. It merges on green like any PR, never `--admin`.
4. After it merges: `git fetch origin`, confirm `git show <merge sha>:Cargo.toml` shows `<next>`, then `git tag -a v<next> -m "v<next>" <merge sha> && git push origin v<next>`.
5. Watch `ci.yml` on the tag to the end: `version` (tag = Cargo.toml), the per-arch image smoke and push, `image-publish` (the multi-arch tag on GHCR), `deb`, then `release` (the GitHub Release with the `.deb` files and `SHA256SUMS`). A failure is an issue for a maintainer.

Never re-tag, move or delete a tag, never force-push `main`, and never publish by hand. A failed publish ships its fix as the next patch.

## Rows

- `release-needed` — A release is needed now and none is in flight.
- `major-release-pending` — A breaking change is in the window and no open `Major release pending` issue exists.
- `release-publish-failed` — `ci.yml` on the tag was watched to the end and the publish failed.
