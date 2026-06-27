# 🔗 Connect from Claude (Desktop & mobile)

Add mcp-ssh to Claude as a **custom connector** (a remote MCP server). Claude
runs the OAuth 2.1 login in your browser, then `bash` / `job` / `file` show up
as tools in any chat.

> **One thing to internalize first:** a custom connector is **not** a local
> process. When you add the URL, **Claude connects to your server from
> Anthropic's cloud**, not from your laptop or phone. So `https://your-host/mcp`
> must be reachable **over the public internet** — `localhost`, a LAN address, or
> anything behind a firewall/VPN will not work. (Source: Anthropic Help Center,
> accessed 2026-06-27 — links at the bottom.)

## ✅ Prerequisites

| Need | Detail |
|---|---|
| Public HTTPS URL | mcp-ssh deployed **behind a TLS reverse proxy** at `https://your-host/mcp`, reachable from the public internet. See [docs/deploy.md](deploy.md). |
| Credentials set | `mcp-ssh set-auth <user>` run on the host — you log in with this username/password. |
| Allowed host | `MCP_SSH_ALLOWED_HOSTS` includes your **public hostname** (DNS-rebinding guard). |
| Proxy forwards OAuth routes | The proxy must pass `/.well-known/*`, `/authorize`, `/token`, `/register` through to `127.0.0.1:1337` — not just `/mcp`. The Caddy/nginx snippets in [docs/deploy.md](deploy.md) already do this. |
| A plan that allows custom connectors | Free, Pro, Max, Team, or Enterprise. **Free is capped at one custom connector.** On **Team/Enterprise** an Owner must add it org-wide first (below). |

> `/mcp` is **bearer-only** — there is no HTTP Basic fallback. Claude (and every
> spec-compliant GUI client) runs the OAuth 2.1 flow automatically and registers
> itself via `/register`, so you do **not** need to pre-create an OAuth client ID
> or secret. Just enter the URL and log in.

## 🖥️ Claude Desktop

1. **Settings → Connectors.** Open Claude Desktop, go to **Settings**, then the **Connectors** section.
2. **Add custom connector.** Click **Add custom connector** (Team/Enterprise members: see the note below — an Owner adds it first, then it appears here to authenticate).
3. **Enter the URL.** Paste the full endpoint including the path and protocol:

   ```
   https://your-host/mcp
   ```

   Leave **Advanced settings** (OAuth Client ID / Secret) empty — mcp-ssh supports
   dynamic client registration, so Claude provisions its own client.
4. **Add**, then **log in.** Claude opens the OAuth 2.1 flow in your browser. Sign
   in with the **username and password you set via `mcp-ssh set-auth`** and approve
   the request. The browser hands the bearer back to Claude automatically.
5. **Enable it in a chat.** In a conversation, click the **+** button (lower-left
   of the composer) → **Connectors**, and toggle **mcp-ssh** on for that chat.
6. **Confirm the tools.** The connector should list **`bash`**, **`job`**, and
   **`file`**. Ask Claude something like *"run `uname -a` with bash"* — a successful
   call confirms the round-trip end to end.

> **Same path on claude.ai (web):** **Settings → Connectors → Add custom
> connector** → URL → **Add** → browser OAuth login. Identical from there.

## 📱 Claude mobile (iOS & Android)

Custom connectors **work on mobile**, but Anthropic flags mobile install as
**beta** and calls Desktop/web the primary path. The reliable pattern:

1. **Add it once on Desktop or web** (steps above). Connectors are tied to your
   Claude account, so the connector and its OAuth session **sync to your phone** —
   no re-entry of the URL needed.
2. **Enable per chat on mobile.** In the mobile app, open a chat, tap the **+**
   button → **Connectors**, and toggle **mcp-ssh** on.
3. If you must add it directly on the phone: **Customize / Settings → Connectors →
   Add custom connector** → `https://your-host/mcp` → browser OAuth login. Treat
   this as best-effort while mobile install is in beta.

The story in the README — *"from your phone, point Claude at your VPS and say run
the deploy"* — works because the connector you added on Desktop is already live on
mobile.

## 🩺 Troubleshooting

| Symptom | Cause → Fix |
|---|---|
| **Connection fails / "can't reach server"** | Server isn't public. Claude dials from **Anthropic's cloud**, not your device — `localhost`/LAN/VPN-only won't work. Expose `https://your-host/mcp` on the public internet (allowlist Anthropic IP ranges if firewalled). |
| **Discovery 404 / OAuth never starts** | Proxy only forwards `/mcp`. It must also forward `/.well-known/oauth-authorization-server`, `/.well-known/oauth-protected-resource`, `/authorize`, `/token`, `/register`. Verify: `curl -fsS https://your-host/.well-known/oauth-authorization-server` returns JSON. See [docs/deploy.md](deploy.md). |
| **Login page errors / blank, or 421/400 on connect** | Host not allowed. Set `MCP_SSH_ALLOWED_HOSTS` to your **public hostname** (DNS-rebinding guard) and restart: `journalctl -u mcp-ssh -e` shows the rejected `Host`. |
| **Worked, then tools go unauthorized after a while** | Access tokens last 24h; Claude silently renews them with the refresh token, so this should be rare. Tokens also reset when the server restarts — GUI clients re-auth automatically (remove/re-add the connector if not); for headless tokens, re-mint (below). |
| **Tools don't appear in a chat** | Connector added but not enabled for that conversation. Click **+** → **Connectors** → toggle **mcp-ssh** on. |

Quick reachability check from the public side:

```bash
curl -fsS https://your-host/.well-known/oauth-authorization-server | jq .   # JSON ⇒ discovery OK
curl -s -o /dev/null -w '%{http_code}\n' -X POST https://your-host/mcp       # 401 ⇒ /mcp is bearer-only, as expected
```

## ⌨️ Headless / CLI (no browser)

The `claude` CLI, curl, or scripts can't run the browser OAuth flow. Mint a bearer
non-interactively with [`bin/mcp-token`](../bin/mcp-token) and pass it as an
`Authorization: Bearer …` header → **[docs/deploy.md#-connect-a-client](deploy.md#-connect-a-client)**.

---

**Sources** (Anthropic Help Center & docs, accessed **2026-06-27**):

- [Get started with custom connectors using remote MCP](https://support.claude.com/en/articles/11175166-get-started-with-custom-connectors-using-remote-mcp)
- [Use connectors to extend Claude's capabilities](https://support.claude.com/en/articles/11176164-use-connectors-to-extend-claude-s-capabilities)
- [Build custom connectors via remote MCP servers](https://support.claude.com/en/articles/11503834-build-custom-connectors-via-remote-mcp-servers)
- [Third-party connectors with remote MCP (claude.com docs)](https://claude.com/docs/connectors/custom/remote-mcp)
