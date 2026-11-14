---
description: Cut a tagged release - CI builds the .deb and docker image from the tag. Use for release, tag, deploy, cargo deb, ghcr, ship
applies: [rust]
---

> House skill — established by the developerz.ai maintainer agent for the maintainers of this repository from this repo's own evidence, then owned by its maintainers. Correct it in place; setup never overwrites a skill that exists.

1. Confirm `main` green: `bin/check`.
2. Pick the next semver tag and bump `version` in Cargo.toml to match (CI refuses a mismatch); rename `docs/changelog/unreleased.md` to `docs/changelog/X.Y.Z.md` (heading `# X.Y.Z — YYYY-MM-DD`, a one-paragraph summary, an **Upgrading** section for a breaking change), start a fresh `unreleased.md`, and add the entry and compare link to `CHANGELOG.md`.
3. `git tag vX.Y.Z && git push origin vX.Y.Z`.
4. CI on the tag gates on fmt, check (clippy + unit + e2e), `cargo audit`, shellcheck and tag == Cargo.toml version; only then it builds + smoke-tests the amd64/arm64 `.deb`s (`cargo deb --locked`) and images, attaches the `.deb`s + `SHA256SUMS` to the GitHub release (the installer refuses a release without `SHA256SUMS`), and publishes the multi-arch `ghcr.io/developerz-ai/mcp-ssh` manifest.
5. Report the tag pushed and the release workflow run link.
fail -> never build or upload artifacts by hand; fix on main, then tag the next patch version.
