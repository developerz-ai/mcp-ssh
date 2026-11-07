# developerz-ai/mcp-ssh — docs

> Index — established by dz-runner setup mode. Add a line when you add a page.

| Where | What lives there |
|---|---|
| [`architecture.md`](architecture.md) | How the binary is put together: stack, modules, request and job lifecycles. |
| [`usage.md`](usage.md) | The three MCP tools — `bash`, `job`, `file` — their params and examples. |
| [`deploy.md`](deploy.md) | Install, reverse proxy, systemd, and connecting a client. |
| [`connect-claude.md`](connect-claude.md) | Claude desktop/mobile as a custom connector, with the public-internet caveat. |
| [`plans/`](plans/) | Dated implementation plans behind shipped work. |
| [`prompts/`](prompts/) | The agent prompt and skill text this repo runs on. |

The brain itself is the repo root's [`CLAUDE.md`](../CLAUDE.md); the
commands are `bin/{setup,dev,check,stop}`.
