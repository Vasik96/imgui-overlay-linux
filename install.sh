#!/bin/sh
# Builds (if needed), installs the binary to ~/.local/bin and creates a launcher that
# file managers and app menus can double-click / start (no terminal needed).
set -e
cd "$(dirname "$0")"
BIN=target/release/imgui-x11-overlay
[ -x "$BIN" ] || cargo build --release

install -Dm755 "$BIN" "$HOME/.local/bin/imgui-x11-overlay"
APPS="$HOME/.local/share/applications"
mkdir -p "$APPS"
sed "s|@EXEC@|$HOME/.local/bin/imgui-x11-overlay|" imgui-x11-overlay.desktop > "$APPS/imgui-x11-overlay.desktop"
chmod +x "$APPS/imgui-x11-overlay.desktop"
update-desktop-database "$APPS" 2>/dev/null || true

# Optional desktop icon.
DESK="$(xdg-user-dir DESKTOP 2>/dev/null || echo "$HOME/Desktop")"
if [ -d "$DESK" ]; then
    cp "$APPS/imgui-x11-overlay.desktop" "$DESK/"
    chmod +x "$DESK/imgui-x11-overlay.desktop"
    gio set "$DESK/imgui-x11-overlay.desktop" metadata::trusted true 2>/dev/null || true
fi
echo "Installed. Start it from the app menu ('ImGui X11 Overlay') or the desktop icon."
echo "Log: /tmp/imgui-x11-overlay-$(id -u).log"
