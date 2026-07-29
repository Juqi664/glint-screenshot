#!/bin/sh
# Register the Ctrl+Alt+A global shortcut for glint-screenshot on GNOME.
#
# GNOME Wayland does not allow arbitrary global hotkeys; the standard mechanism
# is a "custom keybinding" registered via gsettings. This script adds one
# entry (path .../glint0) that runs `glint-screenshot --trigger`, which signals
# the running daemon (or falls back to a single-shot capture).
#
# Safe to run multiple times: it is idempotent (it reuses the same path and
# only appends it to the list if missing).
#
# Usage:
#   glint-setup-shortcut              # Ctrl+Alt+A (default)
#   glint-setup-shortcut '<Ctrl><Alt>S'   # custom binding
set -eu

BINDING="${1:-<Ctrl><Alt>a}"
PATH_ID="/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/glint0/"
SCHEMA="org.gnome.settings-daemon.plugins.media-keys"
CB_SCHEMA="org.gnome.settings-daemon.plugins.media-keys.custom-keybinding"
CB_PATH="/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/glint0/"

command -v gsettings >/dev/null 2>&1 || {
    echo "gsettings not found; this script targets GNOME." >&2
    exit 1
}

# 1. Ensure the path is in the custom-keybindings list.
cur="$(gsettings get "$SCHEMA" custom-keybindings)"
if echo "$cur" | grep -qF "$PATH_ID"; then
    : # already present, nothing to do
elif [ "$cur" = "@as []" ] || [ "$cur" = "[]" ]; then
    gsettings set "$SCHEMA" custom-keybindings "['$PATH_ID']"
else
    # Non-empty array like ['/a/', '/b/']; strip the trailing ']' and append.
    trimmed="${cur%%\]}"
    gsettings set "$SCHEMA" custom-keybindings "$trimmed, '$PATH_ID']"
fi

# 2. Configure the binding itself.
gsettings set "$CB_SCHEMA":"$CB_PATH" name "glint-screenshot"
gsettings set "$CB_SCHEMA":"$CB_PATH" command "glint-screenshot --trigger"
gsettings set "$CB_SCHEMA":"$CB_PATH" binding "$BINDING"

echo "Registered global shortcut: $BINDING -> glint-screenshot --trigger"
echo "If the daemon is not running, the trigger falls back to a single-shot capture."
echo "To start the daemon now (and on every login), the autostart entry is installed"
echo "at /etc/xdg/autostart/io.github.glint.Screenshot.daemon.desktop."
