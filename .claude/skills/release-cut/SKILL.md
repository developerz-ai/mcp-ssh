---
description: Cut and publish a release from a green main. Use for release, tag, semver, cargo deb, ghcr, docker image, artifacts, version bump.
applies: [rust]
---

> House skill — established by dz-runner setup mode from this repo's own evidence, then owned by its maintainers. Correct it in place; setup never overwrites a skill that exists.

1. Confirm `main` is green: `bin/check` locally; the CI `fmt` and `check` jobs must be green too.
2. Pick the next semver version. Keep `Cargo.toml` `[package] version` and the tag in sync.
3. Tag and push — CI does the building, never build or upload artifacts by hand:
   `git tag vX.Y.Z` then `git push origin vX.Y.Z`.
4. The `release` job (`.github/workflows/ci.yml`, `needs: [fmt, check]`, only on `refs/tags/v*`) runs `cargo deb` and publishes `target/debian/*.deb` via `softprops/action-gh-release`.
5. The `docker` job publishes to `ghcr.io/developerz-ai/mcp-ssh` with semver tags (`{{version}}`, `{{major}}.{{minor}}`, `{{major}}`).
6. Report the tag you pushed and link the release workflow run — do not claim a release you did not see publish.
7. If `cargo deb` fails, run `cargo deb` locally to reproduce; packaging metadata lives in `[package.metadata.deb]` in `Cargo.toml` (assets: binary, `deploy/mcp-ssh.service`, README).

Never force-push `main`. Never build in CI by hand-uploading; the tag is the trigger.
