---
description: Cut a tagged release - CI builds the .deb and docker image from the tag. Use for release, tag, deploy, cargo deb, ghcr, ship
applies: [rust]
---

> House skill — established by the developerz.ai maintainer agent for the maintainers of this repository from this repo's own evidence, then owned by its maintainers. Correct it in place; setup never overwrites a skill that exists.

1. Confirm `main` green: `bin/check`.
2. Pick the next semver tag, matching `version` in Cargo.toml.
3. `git tag vX.Y.Z && git push origin vX.Y.Z`.
4. CI on the tag runs fmt + check first; only then the release job builds the `.deb` (`cargo deb`) and attaches it to the GitHub release, and the docker job pushes `ghcr.io/developerz-ai/mcp-ssh`.
5. Report the tag pushed and the release workflow run link.
fail -> never build or upload artifacts by hand; fix on main, then tag the next patch version.
