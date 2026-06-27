#!/usr/bin/env bash
# mcp-ssh one-shot installer.
#
#   curl -fsSL https://raw.githubusercontent.com/developerz-ai/mcp-ssh/main/deploy/install.sh | sudo bash
#
# Downloads the latest .deb from GitHub releases, installs it, prompts for the
# username + password, runs it as a systemd service, and prints how to verify.
# Re-runnable: it just updates the binary and re-applies your settings.
set -euo pipefail

REPO="developerz-ai/mcp-ssh"
CONF_DIR="/etc/mcp-ssh"
CONF="$CONF_DIR/config.toml"
ENV_FILE="$CONF_DIR/mcp-ssh.env"
OVERRIDE_DIR="/etc/systemd/system/mcp-ssh.service.d"

die() { echo "error: $*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (use: curl -fsSL … | sudo bash)"

# Prompts must read from the terminal, not the curl pipe feeding our stdin.
TTY=/dev/tty
ask()      { local v; printf '%s' "$1" >"$TTY"; read -r v <"$TTY"; printf '%s' "$v"; }
ask_secret(){ local v; printf '%s' "$1" >"$TTY"; read -rs v <"$TTY"; printf '\n' >"$TTY"; printf '%s' "$v"; }

# Escape a value for a double-quoted TOML string so a quote, backslash, or
# newline in the username/password can't corrupt or alter config.toml.
toml_escape() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/\\n}
  s=${s//$'\r'/\\r}
  printf '%s' "$s"
}

echo "==> mcp-ssh installer"

# 1. Download the latest release .deb for this architecture.
ARCH="$(dpkg --print-architecture)"   # amd64 / arm64
echo "==> Fetching latest release for $ARCH …"
API="https://api.github.com/repos/$REPO/releases/latest"
URL="$(curl -fsSL "$API" | grep -oE "https://[^\"]*_${ARCH}\.deb" | head -n1)" \
  || die "could not query GitHub releases"
[ -n "$URL" ] || die "no .deb asset for $ARCH in the latest release"

DEB="$(mktemp --suffix=.deb)"
trap 'rm -f "$DEB"' EXIT
echo "==> Downloading $(basename "$URL") …"
curl -fSL --progress-bar "$URL" -o "$DEB"

# 2. Install (apt resolves the .deb's own dependencies).
echo "==> Installing the package …"
apt-get install -y "$DEB" >/dev/null 2>&1 || { dpkg -i "$DEB" || true; apt-get -fy install; }

# 3. Credentials.
echo "==> Set the MCP login (one username + password)"
USER_NAME="$(ask 'Username: ')"; [ -n "$USER_NAME" ] || die "username required"
while :; do
  PASS1="$(ask_secret 'Password: ')";        [ -n "$PASS1" ] || { echo "  password required" >"$TTY"; continue; }
  PASS2="$(ask_secret 'Confirm password: ')"
  [ "$PASS1" = "$PASS2" ] && break || echo "  passwords did not match — try again" >"$TTY"
done

# 4. Which OS user the agent's shell runs as (its ~/.bashrc/aliases/version
#    managers are what `bash` commands will see). Defaults to the dedicated,
#    low-privilege mcp-ssh service user; a broader account is an explicit opt-in.
DEFAULT_RUN_USER="mcp-ssh"
RUN_USER="$(ask "Run the service as which user? [$DEFAULT_RUN_USER]: ")"
RUN_USER="${RUN_USER:-$DEFAULT_RUN_USER}"
# Auto-provision only the dedicated service user; never silently create an
# arbitrary account the operator may have typed by mistake.
if ! id "$RUN_USER" >/dev/null 2>&1; then
  [ "$RUN_USER" = "mcp-ssh" ] || die "user '$RUN_USER' does not exist"
  echo "==> Creating dedicated service user '$RUN_USER' …"
  useradd --system --create-home --shell /bin/bash "$RUN_USER" \
    || die "could not create user '$RUN_USER'"
fi
RUN_GROUP="$(id -gn "$RUN_USER")"

# 5. Public hostname for the DNS-rebinding guard (optional; loopback default).
PUBLIC_HOST="$(ask 'Public hostname for TLS proxy (blank = localhost only): ')"

# 6. Write config (chmod 600, owned by the run-as user so the service reads it).
install -d -m 755 "$CONF_DIR"
umask 077
cat >"$CONF" <<EOF
user = "$(toml_escape "$USER_NAME")"
pass = "$(toml_escape "$PASS1")"
EOF
chown "$RUN_USER:$RUN_GROUP" "$CONF"
chmod 600 "$CONF"

if [ -n "$PUBLIC_HOST" ]; then
  cat >"$ENV_FILE" <<EOF
MCP_SSH_ALLOWED_HOSTS=localhost,127.0.0.1,$PUBLIC_HOST
EOF
else
  # Blank on a rerun must drop any prior allowlist, else a stale public host
  # keeps passing Host validation despite the "localhost only" choice.
  rm -f "$ENV_FILE"
fi

# 7. systemd drop-in: run as the chosen user instead of the packaged default.
install -d -m 755 "$OVERRIDE_DIR"
cat >"$OVERRIDE_DIR/override.conf" <<EOF
[Service]
User=$RUN_USER
Group=$RUN_GROUP
EOF

# 8. Start it.
echo "==> Enabling + starting the service …"
systemctl daemon-reload
systemctl enable --now mcp-ssh
sleep 1

# 9. Verify on loopback.
echo "==> Verifying …"
if curl -fsS http://127.0.0.1:1337/.well-known/oauth-authorization-server >/dev/null; then
  echo "✅ mcp-ssh is up on 127.0.0.1:1337 (running as $RUN_USER)."
else
  echo "⚠️  Service did not answer yet — check: journalctl -u mcp-ssh -e" >&2
fi

cat <<EOF

Next steps:
  • Logs:    journalctl -u mcp-ssh -f
  • Status:  systemctl status mcp-ssh
  • Put TLS in front (Caddy/nginx) → https://your-host/mcp   (see docs/deploy.md)
  • In Claude, add the remote MCP server https://your-host/mcp and log in
    with the username/password you just set.
EOF
