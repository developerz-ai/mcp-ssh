# Brief: is the project healthy?

Weekly checkup (Tuesdays 07:00 UTC) fired by `schedules: health`: the failures no issue or PR announces.

| Signal | Command | Healthy |
|---|---|---|
| `main` is green | `gh run list --workflow ci.yml --branch main --event push --limit 1 --json conclusion,headSha,url`, then `git rev-parse origin/main` | `success` on the `origin/main` tip; a null conclusion or another sha is no verdict |
| the last tag published | `t=$(git describe --tags --abbrev=0 --match 'v[0-9]*' origin/main); gh run list --workflow ci.yml --event push --limit 20 --json conclusion,headSha,headBranch,url` filtered to `headBranch == $t` | `success`; no run for that tag is no verdict |
| the stamped version is tagged | `grep -m1 '^version' Cargo.toml` against `$t` | equal, or an open release PR or in-flight run explains it |
| the GitHub Release exists with its assets | `gh release view "$t" --json isDraft,publishedAt,assets` | not a draft, published, carrying both `.deb` files and `SHA256SUMS` (`deploy/install.sh` refuses a release without them) |
| the image is pullable | `docker manifest inspect ghcr.io/developerz-ai/mcp-ssh:${t#v}` | lists `linux/amd64` and `linux/arm64` |
| Dependabot is not stuck | `gh pr list --app dependabot --state open --limit 100 --json number,updatedAt,statusCheckRollup` | no PR with failed checks and more than 7 days since `updatedAt` |
| no stale toolchain | `rust-toolchain.toml` channel, `Cargo.toml` `rust-version` and the Dockerfile `FROM` | all three name the same version (CLAUDE.md: bump together) |

## What to file

For each unhealthy signal, one ask, unless an open issue already carries its exact title. Read the
open titles first, exactly so: `gh api --paginate "repos/developerz-ai/mcp-ssh/issues?state=open" | jq -r '.[] | select(.pull_request == null) | .title'`,
then compare byte for byte. A failed read is no verdict, never "no open ask".
Titles are stable: nothing that changes run to run (sha, run URL, version) goes in one; that goes in the body,
with the command and its output. Unreadable evidence is no verdict: re-run it, never file it as a defect.
A red `main` comes first: nothing releases from it.

A drifted toolchain is fixable by the fleet: the ask says to bump all three pins to the newest stable
Rust that is at least 14 days old in one PR, fixing any new clippy lint under `-D warnings`.

This lane reads and files; it never repairs. It never re-runs a publish, re-tags, moves or deletes a tag,
pushes an image by hand, force-pushes `main` or merges with `--admin`.

## Rows

- `main-green` — The latest `ci.yml` push run on the `origin/main` tip is `success`.
- `last-tag-published` — The last `v*` tag has a `success` `ci.yml` run.
- `stamped-version-tagged` — `version` in `Cargo.toml` equals that tag, or a release in flight explains it.
- `github-release-published` — The GitHub Release for that tag is published with both `.deb` files and `SHA256SUMS`.
- `image-pullable` — The GHCR image for that tag lists amd64 and arm64.
- `dependabot-not-stuck` — No open Dependabot PR has failed checks with more than 7 days since its `updatedAt`.
- `toolchain-in-step` — `rust-toolchain.toml`, `Cargo.toml` `rust-version` and the Dockerfile base agree.
