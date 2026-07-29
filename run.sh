#!/usr/bin/env bash
# glint-screenshot real-mode launch script
#
# Why is this script needed?
# GNOME's XDG Desktop Portal identifies the calling app id from the systemd
# scope unit name of the process, in the form `app-<app_id>-<pid>.scope`.
# Only processes launched this way are correctly identified by the Portal.
#
# If you run the binary directly from a terminal (e.g. Cursor/Electron's
# built-in terminal, whose process lives in `app-org.chromium.Chromium-*.scope`),
# the Portal misidentifies this app as Chromium, which has "no" in the
# PermissionStore -> the screenshot is rejected.
#
# So this script launches the app in its own scope via
# `systemd-run --user --scope`, with the scope name containing the correct
# app id.
#
# Usage:
#   ./run.sh            real mode (Portal screenshot)
#   GLINT_DEMO=1 ./run.sh   demo mode (colorful test image, no Portal permission needed)

set -euo pipefail

# Use the release build for real runs: the capture path (PNG decode + RGBA->ARGB32
# conversion) is pure Rust and is dramatically faster when optimized. Fall back
# to debug if the release binary isn't built yet.
BIN_RELEASE="$(cd "$(dirname "$0")" && pwd)/target/release/glint-screenshot"
BIN_DEBUG="$(cd "$(dirname "$0")" && pwd)/target/debug/glint-screenshot"
if [[ -x "$BIN_RELEASE" ]]; then
    BIN="$BIN_RELEASE"
elif [[ -x "$BIN_DEBUG" ]]; then
    BIN="$BIN_DEBUG"
else
    echo "ERROR: no executable found (neither release nor debug)." >&2
    echo "   please run: cargo build --release" >&2
    exit 1
fi
APP_ID="io.github.glint.Screenshot"

# Install the drawing-tool symbolic SVG icons into the user's hicolor icon
# theme so GTK loads them as crisp vector symbolic icons (recolor via
# currentColor). The system hicolor index.theme already declares the
# `symbolic/apps` directory, so we only drop the files in place.
ICON_SRC="$(cd "$(dirname "$0")" && pwd)/icons"
ICON_DST="$HOME/.local/share/icons/hicolor/symbolic/apps"
if [[ -d "$ICON_SRC" ]]; then
    mkdir -p "$ICON_DST"
    cp -f "$ICON_SRC"/glint-tool-*-symbolic.svg "$ICON_DST"/ 2>/dev/null || true
    gtk4-update-icon-cache "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
fi

# Launch in an independent scope. $$ is the current shell's PID, used as the
# scope suffix (matching GNOME's own naming convention).
UNIT="app-${APP_ID}-$$.scope"
exec systemd-run --user --quiet --scope --unit="$UNIT" \
    --setenv=RUST_LOG="${RUST_LOG:-info}" \
    --setenv=GLINT_DEMO="${GLINT_DEMO:-}" \
    --setenv=GLINT_USE_SCREENCAST="${GLINT_USE_SCREENCAST:-}" \
    -- "$BIN" "$@"
