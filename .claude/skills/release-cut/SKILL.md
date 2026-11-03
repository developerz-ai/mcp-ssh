---
description: Cutting a release tag and letting CI build the .deb and image. Use for release, tag, publish, cargo deb, docker push.
applies: [rust]
---

> House skill — established by dz-runner setup mode from this repo's own evidence, then owned by its maintainers. Correct it in place; setup never overwrites a skill that exists.

1. Confirm `main` is green: run `./bin/check`. Fail -> stop, fix the gate first.
2. Read `Cargo.toml` and pick the next semver for the `version` field; the release tag must match it (`vX.Y.Z`).
3. Bump `version` in `Cargo.toml` if it does not already match the tag you intend to push.
4. Commit the version bump on `main` and let CI run the `fmt` + `check` jobs to green.
5. Push the tag: `git tag vX.Y.Z && git push origin vX.Y.Z`. Fail -> stop, do not retry with `--force`.
6. Do NOT build or upload artifacts by hand. CI does it: the `release` job runs `cargo deb` and uploads `target/debian/*.deb` to the GitHub release; the `docker` job builds and pushes `ghcr.io/developerz-ai/mcp-ssh` with semver, major.minor and major tags.
7. Verify the `release` and `docker` jobs both passed on that tag. Fail -> stop; the tag is already pushed, so fix forward with a new patch tag rather than moving it.
8. Report the tag pushed and link the release workflow run.
