#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$ROOT_DIR/dist/ccProxy.app"
MODULE_CACHE_DIR="$ROOT_DIR/dist/module-cache"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
SERVER_BIN="$ROOT_DIR/target/release/ccproxy"

rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR" "$MODULE_CACHE_DIR"

cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"

swiftc \
  -O \
  -module-cache-path "$MODULE_CACHE_DIR" \
  -target arm64-apple-macos13.0 \
  "$ROOT_DIR/macos/ccProxyMenu/App.swift" \
  -o "$MACOS_DIR/ccProxyMenu"

cp "$ROOT_DIR/macos/ccProxyMenu/Info.plist" "$CONTENTS_DIR/Info.plist"
cp "$SERVER_BIN" "$RESOURCES_DIR/ccproxy-server"
cp "$ROOT_DIR/assets/logo.png" "$RESOURCES_DIR/logo.png"
cp "$ROOT_DIR/assets/logo.svg" "$RESOURCES_DIR/logo.svg"
xattr -c "$RESOURCES_DIR/logo.png" "$RESOURCES_DIR/logo.svg" "$RESOURCES_DIR/ccproxy-server" 2>/dev/null || true
chmod +x "$RESOURCES_DIR/ccproxy-server" "$MACOS_DIR/ccProxyMenu"

xattr -cr "$APP_DIR" 2>/dev/null || true
codesign --force --deep --sign - "$APP_DIR" >/dev/null

echo "Built $APP_DIR"
echo "Open it with: open \"$APP_DIR\""
