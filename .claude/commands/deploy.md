Cut a release. Confirm `main` is green (`bin/check`), pick the next semver tag, then `git tag vX.Y.Z` and `git push origin vX.Y.Z`. CI builds the `.deb` (cargo deb) and the docker image from the tag — do not build or upload artifacts by hand. Report the tag pushed and link the release workflow run.

$ARGUMENTS
