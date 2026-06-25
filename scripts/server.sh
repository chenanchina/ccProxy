#!/usr/bin/env bash
#
# ccProxy server manager: install / update / uninstall as a systemd service.
#
# Layout (FHS):
#   binary  -> /usr/local/bin/ccproxy
#   config  -> /etc/ccproxy/.env
#   data    -> /var/lib/ccproxy/   (HOME: SQLite db + .codex/auth.json)
#
# Usage:
#   scripts/server.sh install      # build, install, set up + start service
#   scripts/server.sh update       # rebuild and restart (keeps config and data)
#   scripts/server.sh uninstall    # stop and remove the service (keeps config/data)
#
# Flags:
#   --pull              git pull before building (update/install)
#   --no-build          skip cargo build; reuse the binary at CCPROXY_BIN
#   --purge             (uninstall only) also delete config and data
#
# Environment overrides:
#   CCPROXY_USER     service user             (default: root)
#   CCPROXY_CONF     config dir               (default: /etc/ccproxy)
#   CCPROXY_DATA     data dir                 (default: /var/lib/ccproxy)
#   CCPROXY_BIN      prebuilt binary to use   (default: <repo>/target/release/ccproxy)
#
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVICE="ccproxy"
UNIT="/etc/systemd/system/${SERVICE}.service"
BINDIR="/usr/local/bin"
BIN="${BINDIR}/ccproxy"
CONFDIR="${CCPROXY_CONF:-/etc/ccproxy}"
DATADIR="${CCPROXY_DATA:-/var/lib/ccproxy}"
ENVFILE="${CONFDIR}/.env"
RUN_USER="${CCPROXY_USER:-root}"
DEFAULT_BIN="$ROOT_DIR/target/release/ccproxy"

PULL=0
BUILD=1
PURGE=0
CMD=""

for arg in "$@"; do
  case "$arg" in
    install|update|uninstall) CMD="$arg" ;;
    --pull) PULL=1 ;;
    --no-build) BUILD=0 ;;
    --purge) PURGE=1 ;;
    -h|--help) CMD="help" ;;
    *) echo "Unknown argument: $arg" >&2; exit 2 ;;
  esac
done

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  SUDO="sudo"
fi

# Logs go to stderr so functions can return values via stdout/$( ).
log() { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  awk 'NR>2 && /^#/ { sub(/^# ?/, ""); print; next } NR>2 { exit }' "${BASH_SOURCE[0]}"
}

maybe_pull() {
  if [ "$PULL" -eq 1 ]; then
    [ -d "$ROOT_DIR/.git" ] || die "--pull given but $ROOT_DIR is not a git repo"
    log "Pulling latest code"
    git -C "$ROOT_DIR" pull --ff-only
  fi
}

resolve_binary() {
  local bin="${CCPROXY_BIN:-$DEFAULT_BIN}"
  if [ "$BUILD" -eq 1 ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust or pass --no-build with CCPROXY_BIN"
    log "Building release binary"
    cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"
    bin="$DEFAULT_BIN"
  fi
  [ -x "$bin" ] || die "binary not found at $bin (build it, or set CCPROXY_BIN)"
  echo "$bin"
}

write_unit() {
  log "Writing systemd unit to $UNIT"
  local user_line=""
  if [ "$RUN_USER" != "root" ]; then
    user_line="User=${RUN_USER}"
  fi
  $SUDO tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=ccProxy - OpenAI/Anthropic to Codex Responses proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${BIN}
WorkingDirectory=${DATADIR}
EnvironmentFile=${ENVFILE}
Environment=HOME=${DATADIR}
${user_line}
Restart=on-failure
RestartSec=2
DynamicUser=no

[Install]
WantedBy=multi-user.target
EOF
}

service_active() {
  systemctl is-active --quiet "$SERVICE"
}

do_install() {
  maybe_pull
  local bin
  bin="$(resolve_binary)"

  if service_active; then
    log "Stopping running service"
    $SUDO systemctl stop "$SERVICE"
  fi

  log "Installing binary to $BIN"
  $SUDO install -m 0755 "$bin" "$BIN"

  $SUDO mkdir -p "$CONFDIR" "$DATADIR"
  if [ ! -f "$ENVFILE" ]; then
    log "Seeding $ENVFILE from .env.example (edit it before relying on auth)"
    $SUDO install -m 0600 "$ROOT_DIR/.env.example" "$ENVFILE"
  else
    log "Keeping existing $ENVFILE"
  fi

  if [ "$RUN_USER" != "root" ]; then
    $SUDO chown -R "$RUN_USER" "$DATADIR"
  fi

  write_unit
  log "Enabling and starting service"
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable --now "$SERVICE"
  $SUDO systemctl --no-pager --full status "$SERVICE" || true
  log "Done. Edit $ENVFILE then: $SUDO systemctl restart $SERVICE"
}

do_update() {
  [ -f "$UNIT" ] || die "service not installed; run: $0 install"
  maybe_pull
  local bin
  bin="$(resolve_binary)"

  log "Stopping service"
  $SUDO systemctl stop "$SERVICE" || true
  log "Replacing binary at $BIN"
  $SUDO install -m 0755 "$bin" "$BIN"
  log "Starting service"
  $SUDO systemctl start "$SERVICE"
  $SUDO systemctl --no-pager --full status "$SERVICE" || true
  log "Updated. The token database migrates automatically on start."
}

do_uninstall() {
  if [ -f "$UNIT" ]; then
    log "Stopping and disabling service"
    $SUDO systemctl disable --now "$SERVICE" 2>/dev/null || true
    $SUDO rm -f "$UNIT"
    $SUDO systemctl daemon-reload
  else
    log "No unit at $UNIT"
  fi
  $SUDO rm -f "$BIN"
  if [ "$PURGE" -eq 1 ]; then
    log "Purging $CONFDIR and $DATADIR (config, database, auth)"
    $SUDO rm -rf "$CONFDIR" "$DATADIR"
  else
    log "Kept $CONFDIR and $DATADIR. Re-run with --purge to delete config and data."
  fi
  log "Uninstalled."
}

case "$CMD" in
  install) do_install ;;
  update) do_update ;;
  uninstall) do_uninstall ;;
  help|"") usage ;;
  *) usage; exit 2 ;;
esac
