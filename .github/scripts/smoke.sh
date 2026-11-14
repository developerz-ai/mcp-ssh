#!/usr/bin/env bash
# Smoke-test a running mcp-ssh: /healthz, OAuth login (bin/mcp-token), MCP
# initialize, an inline `bash`, and `job kill` of a backgrounded job. Gates
# publishing the Docker image and the .deb in CI.
#
#   MCP_SSH_USER=… MCP_SSH_PASS=… .github/scripts/smoke.sh http://127.0.0.1:1337
set -euo pipefail

BASE="${1:?usage: smoke.sh <base-url>}"
: "${MCP_SSH_USER:?set MCP_SSH_USER}"
: "${MCP_SSH_PASS:?set MCP_SSH_PASS}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

fail() { echo "smoke: $*" >&2; exit 1; }

echo "smoke: waiting for $BASE/healthz"
curl -fsS --retry 30 --retry-connrefused --retry-all-errors --retry-delay 1 \
  "$BASE/healthz" | grep -qx ok || fail "/healthz did not answer ok"

TOKEN="$(MCP_SSH_URL="$BASE" "$ROOT/bin/mcp-token")" || fail "could not mint a token"

mcp() {
  curl -fsS -X POST "$BASE/mcp" -K - \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' "$@" <<CFG
header = "Authorization: Bearer $TOKEN"
CFG
}

SID="$(mcp -D - -o /dev/null \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | tr -d '\r' | sed -n 's/^mcp-session-id: *//Ip')"
[ -n "$SID" ] || fail "initialize returned no mcp-session-id"
mcp -H "mcp-session-id: $SID" -o /dev/null \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'

call() { # id tool arguments-json
  mcp -H "mcp-session-id: $SID" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":$1,\"method\":\"tools/call\",\"params\":{\"name\":\"$2\",\"arguments\":$3}}"
}

# The server's shell does the arithmetic, proving the command really ran.
# shellcheck disable=SC2016
out="$(call 2 bash '{"cmd":"echo smoke-$((40+2))"}')"
grep -q 'smoke-42' <<<"$out" || fail "inline bash: $out"

out="$(call 3 bash '{"cmd":"sleep 300","bg":true,"title":"smoke"}')"
id="$(grep -oE 'smoke-[0-9]{2}-[0-9]{2}-[0-9]{2}[0-9a-z-]*' <<<"$out" | head -n1)"
[ -n "$id" ] || fail "bg bash returned no job id: $out"

out="$(call 4 job "{\"action\":\"kill\",\"id\":\"$id\"}")"
grep -q '"isError":false' <<<"$out" || fail "job kill: $out"

echo "smoke: ok"
