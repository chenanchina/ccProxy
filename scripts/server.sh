#!/usr/bin/env bash
#
# ccProxy server manager: install / update / uninstall as a systemd service.
#
# Usage:
#   scripts/server.sh install      # build, install to PREFIX, set up + start service
#   scripts/server.sh update       # rebuild and restart (keeps .env and database)
#   scripts/server.sh uninstall    # stop and remove the service (keeps PREFIX data)
#
# Flags:
#   --pull              git pull before building (update/install)
#   --no-build          skip cargo build; reuse the binary at CCPROXY_BIN or PREFIX
#   --purge             (uninstall only) also delete PREFIX, including .env and db
#
# Environment overrides:
#   CCPROXY_PREFIX   install dir            (default: /opt/ccproxy)
#   CCPROXY_USER     service user           (default: root)
#   CCPROXY_BIN      prebuilt binary to use (default: <repo>/target/release/ccproxy)
#
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${CCPROXY_PREFIX:-/opt/ccproxy}"
SERVICE="ccproxy"
UNIT="/etc/systemd/system/${SERVICE}.service"
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

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
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
ExecStart=${PREFIX}/ccproxy
WorkingDirectory=${PREFIX}
EnvironmentFile=${PREFIX}/.env
Environment=HOME=${PREFIX}
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

  log "Installing to $PREFIX"
  $SUDO mkdir -p "$PREFIX"

  if service_active; then
    log "Stopping running service"
    $SUDO systemctl stop "$SERVICE"
  fi

  $SUDO install -m 0755 "$bin" "$PREFIX/ccproxy"

  if [ ! -f "$PREFIX/.env" ]; then
    log "Seeding $PREFIX/.env from .env.example (edit it before relying on auth)"
    $SUDO cp "$ROOT_DIR/.env.example" "$PREFIX/.env"
  else
    log "Keeping existing $PREFIX/.env"
  fi

  if [ "$RUN_USER" != "root" ]; then
    $SUDO chown -R "$RUN_USER" "$PREFIX"
  fi

  write_unit
  log "Enabling and starting service"
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable --now "$SERVICE"
  $SUDO systemctl --no-pager --full status "$SERVICE" || true
  log "Done. Edit $PREFIX/.env then: $SUDO systemctl restart $SERVICE"
}

do_update() {
  [ -f "$UNIT" ] || die "service not installed; run: $0 install"
  maybe_pull
  local bin
  bin="$(resolve_binary)"

  log "Stopping service"
  $SUDO systemctl stop "$SERVICE" || true
  $SUDO install -m 0755 "$bin" "$PREFIX/ccproxy"
  if [ "$RUN_USER" != "root" ]; then
    $SUDO chown "$RUN_USER" "$PREFIX/ccproxy"
  fi
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
  if [ "$PURGE" -eq 1 ]; then
    log "Purging $PREFIX (binary, .env, database, auth)"
    $SUDO rm -rf "$PREFIX"
  else
    log "Kept $PREFIX. Remove it manually or re-run with --purge to delete data."
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
