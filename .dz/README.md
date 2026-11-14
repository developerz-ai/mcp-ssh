# `.dz/`

Repo-scoped configuration for the developerz.ai maintainer agent. `CLAUDE.md` binds everything here:
no file in this tree relaxes a NEVER entry.

| Path | What |
|---|---|
| `maintainer/maintainer.yml` | the policy: what the bot triages, closes, reviews, merges and releases. [Reference](https://developerz.ai/docs/maintainer-yml) |
| `maintainer/reviewer.md` | this repo's layer of the developerz.ai reviewer prompt |
| `briefs/` | what each `schedules:` checkup works from, read at `main`'s HEAD: `release-due.md` (weekly: is a release needed), `security-release.md` (the one off-cycle release), `health.md` (weekly: the failures nothing announces), `improve.md` (monthly: at most three small, evidenced improvements) |
| `schedules/` | `hygiene.md` and `devops.md`: what the monthly `improve` pass looks for |
| `onboarding/` | the setup pass's scorecard and base audit |
