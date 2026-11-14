#!/usr/bin/env bash
# mcp-ssh one-shot installer.
#
#   curl -fsSL https://raw.githubusercontent.com/developerz-ai/mcp-ssh/main/deploy/install.sh | sudo bash
#
# Downloads the latest .deb from GitHub releases, verifies it against the
# release's SHA256SUMS, installs it, prompts for the username + password, runs
# it as a systemd service, and prints how to verify.
# Re-runnable: it updates the binary, re-applies your settings and restarts.
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

# 1. Download the latest release .deb for this architecture and verify it.
ARCH="$(dpkg --print-architecture)"
case "$ARCH" in
  amd64|arm64) ;;
  *) die "unsupported architecture '$ARCH' (releases ship amd64 and arm64 only)" ;;
esac
echo "==> Fetching latest release for $ARCH …"
API="https://api.github.com/repos/$REPO/releases/latest"
RELEASE_JSON="$(curl -fsSL "$API")" || die "could not query GitHub releases"
URL="$(grep -oE "https://[^\"]*_${ARCH}\.deb" <<<"$RELEASE_JSON" | head -n1 || true)"
[ -n "$URL" ] || die "no .deb asset for $ARCH in the latest release"
SUMS_URL="$(grep -oE 'https://[^"]*/SHA256SUMS' <<<"$RELEASE_JSON" | head -n1 || true)"
[ -n "$SUMS_URL" ] || die "latest release has no SHA256SUMS asset; refusing to install unverified"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
DEB_NAME="$(basename "$URL")"
DEB="$WORK/$DEB_NAME"
echo "==> Downloading $DEB_NAME …"
curl -fSL --progress-bar "$URL" -o "$DEB"
curl -fsSL "$SUMS_URL" -o "$WORK/SHA256SUMS"
echo "==> Verifying checksum …"
grep -E "  ${DEB_NAME//./\\.}\$" "$WORK/SHA256SUMS" >"$WORK/expected" \
  || die "$DEB_NAME is not listed in SHA256SUMS"
(cd "$WORK" && sha256sum --quiet -c expected) || die "checksum mismatch for $DEB_NAME"

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

# 5. Public hostname for the DNS-rebinding guard + OAuth issuer URL (optional;
#    loopback default). Validated: it is written into a systemd env file.
PUBLIC_HOST="$(ask 'Public hostname for TLS proxy (blank = localhost only): ')"
if [ -n "$PUBLIC_HOST" ] && ! [[ "$PUBLIC_HOST" =~ ^[A-Za-z0-9.-]+(:[0-9]+)?$ ]]; then
  die "invalid hostname '$PUBLIC_HOST' (expected host or host:port)"
fi

# 6. Write config (chmod 600, owned by the run-as user so the service reads it).
install -d -m 750 -o "$RUN_USER" -g "$RUN_GROUP" "$CONF_DIR"
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
MCP_SSH_PUBLIC_URL=https://$PUBLIC_HOST
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

# 7b. Grant the run user passwordless sudo. By design the agent's `bash` tool is
#     full-power: it self-manages the host (installs updates, restarts itself,
#     manages services). The unit ships NoNewPrivileges=false so setuid sudo
#     works. This makes the auth-gated MCP shell root-capable — lock it down
#     (remove this file + set NoNewPrivileges=true) if that's not what you want.
SUDOERS="/etc/sudoers.d/mcp-ssh"
echo "==> Granting '$RUN_USER' passwordless sudo (agent self-management) …"
printf '%s ALL=(ALL) NOPASSWD:ALL\n' "$RUN_USER" >"$SUDOERS"
chmod 440 "$SUDOERS"
visudo -cf "$SUDOERS" >/dev/null || { rm -f "$SUDOERS"; die "invalid sudoers entry for '$RUN_USER'"; }

# 8. Start it. `restart`, not `enable --now`: a rerun must pick up new
#    credentials, hosts and the upgraded binary on an already-running service.
echo "==> Enabling + (re)starting the service …"
systemctl daemon-reload
systemctl enable mcp-ssh
systemctl restart mcp-ssh
sleep 1

# 9. Verify on loopback.
echo "==> Verifying …"
if curl -fsS http://127.0.0.1:1337/healthz >/dev/null; then
  echo "✅ mcp-ssh is up on 127.0.0.1:1337 (running as $RUN_USER)."
else
  echo "⚠️  Service did not answer yet — check: journalctl -u mcp-ssh -e" >&2
fi

cat <<EOF

⚠️  The '$RUN_USER' shell has passwordless sudo — the agent can run anything as
    root. To lock it down: remove $SUDOERS, set NoNewPrivileges=true, restart.

Next steps:
  • Logs:    journalctl -u mcp-ssh -f
  • Status:  systemctl status mcp-ssh
  • Put TLS in front (Caddy/nginx) → https://your-host/mcp   (see docs/deploy.md)
  • In Claude, add the remote MCP server https://your-host/mcp and log in
    with the username/password you just set.
EOF
