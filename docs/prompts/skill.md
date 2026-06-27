---
name: mcp-ssh-agent
description: Use whenever the user wants to run work on a remote host through an mcp-ssh server — shell commands, reading/searching/editing files, launching long-running tasks and polling them, or kicking off autonomous coding tools (if installed) such as a multi-session orchestrator or a one-shot `claude -p`. Triggers on "run on the server/host", "do it on the VPS", "kick off the agent", "start the orchestrator", "use claude -p", "check progress", "is it still running", or any goal where work should happen on the remote host rather than inline.
---

# mcp-ssh Agent Skill

Execute work on a remote host via mcp-ssh tools (`bash`, `file`, `job`). Do real work — don't
describe it. Replace `<repo>` with the target project.

**Recommended pattern — dispatch, don't carry.** For heavy work, dispatch it to the host: call
`bash` with `bg=true` to launch an autonomous agent server-side (`claude -p ...` or an
orchestrator), then poll with `job action="poll"`. Work runs in the background; this session
stays light and responsive.

## Tools (three, constant)
- `bash` — run a command. `cmd`, `cwd?`, `timeout?`, `bg?`, `interactive?`, `title?`.
- `job` — manage background jobs. `action` (`poll`/`list`/`kill`), `id?`, `cursor?`, `limit?`.
- `file` — file ops. `action` (`read`/`write`/`append`/`delete`/`list`/`grep`/`move`),
  `path?`, `content?`, `pattern?`, `recursive?`, `src?`, `dest?`, `cursor?`, `limit?`.

## Execution model
`bash` returns inline if the command finishes within the inline window (default 2s); slower
commands auto-background to a job id.

```
# fast / reads:    bash  cmd="..."
# long-running:    bash  bg=true  cmd="cd <repo> && ..."     then:  job action="poll" id="<job-id>"
```

- Anything >2s ⇒ `bg=true`, poll with `job action="poll"` (byte/line-capped, paginated via
  `cursor`/`limit` — chatty output never floods context).
- `interactive=true` sources `~/.bashrc` (aliases, mise/nvm/rbenv); default is faster `sh -c`.
- `title="deploy-check"` labels the job id (`deploy-check-HH:MM:SS`). Keep titles secret-free.
- Use `file` for reads/edits/search (`grep` with `recursive=true`), not shelling out.

## Optional: autonomous coding tools (only if installed on the host)
**Not** part of mcp-ssh — external CLIs launched through `bash` (long-running ⇒ `bg=true` + poll).
An orchestrator (Claude Agent SDK based): opens PRs, monitors CI, loops until green and comments resolved.
For repo work, not arbitrary shell tasks.

### When to use what
| Reach for | When |
|---|---|
| orchestrator `start "<goal>"` | BIG coding task scoped to ONE repo — multi-step feature/refactor needing planning + PRs. |
| `claude -p "<prompt>"` | small one-off around the VPS — quick fix, script, single change, ops task; not just repos. |
| orchestrator `merge-pr` | a PR already exists (e.g. one a `claude -p`/agent run opened) — loop the orchestrator: monitor CI, resolve review comments, merge once green. |

1. **Pre-flight** — check status before launching:
   ```
   bash cmd="cd <repo> && <orchestrator> status"
   ```
2. **Launch** a big goal (multi-session, opens PRs, may auto-merge):
   ```
   bash bg=true cmd="cd <repo> && <orchestrator> start '<one tight imperative goal>'"
   ```
   One tight imperative goal. Confirm whether auto-merge defaults on.
3. **Monitor**:
   ```
   bash cmd="cd <repo> && <orchestrator> status && <orchestrator> progress"
   ```
4. **One-shot** task instead of a full session:
   ```
   bash bg=true cmd="cd <repo> && claude -p '<prompt>' --dangerously-skip-permissions"
   ```
   Flags: `--system-prompt`, `--add-dir`, `--bare`, `-c`.
   `--dangerously-skip-permissions` runs unattended with no permission prompts — only on a host
   you fully control.

## Multi-repo combo (only if these tools are installed)
Fan the **same** goal across many repos in one pass — keeps a fleet of repos in sync.

- **One-shot, many repos:** `claude -p "/goal ..."` invokes a slash-command goal
  non-interactively. Loop, one `bg=true` job per repo:
  ```
  for repo in <repo-a> <repo-b> <repo-c>; do
    bash bg=true cmd="cd $repo && claude -p '/goal <the goal>' --dangerously-skip-permissions"
  done
  ```
  Then poll each job id with `job action="poll"`.
- **End-to-end loop:** `<orchestrator> start "<goal>"` does the work and opens a PR per repo,
  then `<orchestrator> merge-pr` merges it — a full goal → PR → merge loop across many repos.

## QA after autonomous work — always
- Verify each acceptance criterion the goal defined.
- Review merged PRs + scan diffs (`gh pr list --state merged -L 20`).
- Run the repo check script (`bin/check`, `bin/test`).
- Verdict: pass / needs-attention / fail — one line, with PR numbers and test status.
