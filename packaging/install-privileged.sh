#!/bin/sh
# Installs the privileged helper + polkit policy + polkit rule for OpenSilexLink.
# Run once as root (sudo ./packaging/install-privileged.sh). Afterwards attach/detach
# runs for administrators WITHOUT a password prompt.
set -eu

SRC_DIR="$(CDPATH= cd "$(dirname "$0")" && pwd)"
HELPER_SRC="$SRC_DIR/helper/silex-usbip-helper"
POLICY_SRC="$SRC_DIR/polkit/org.opensilexlink.policy"
RULES_SRC="$SRC_DIR/polkit/49-opensilexlink.rules"

HELPER_DST="/usr/libexec/silex-usbip-helper"
POLICY_DST="/usr/share/polkit-1/actions/org.opensilexlink.policy"
RULES_DST="/etc/polkit-1/rules.d/49-opensilexlink.rules"

if [ "$(id -u)" -ne 0 ]; then
    echo "Please run as root: sudo $0" >&2
    exit 1
fi

# Helper: root:root, not writable by others (pkexec requires this).
install -D -o root -g root -m 0755 "$HELPER_SRC" "$HELPER_DST"

# Policy (action + auth_admin_keep default): world-readable, root-writable only.
install -D -o root -g root -m 0644 "$POLICY_SRC" "$POLICY_DST"

# Rule (prompt-free for administrators): requires polkit >= 0.106 (JS rules).
install -D -o root -g root -m 0644 "$RULES_SRC" "$RULES_DST"

echo "Installed:"
echo "  $HELPER_DST"
echo "  $POLICY_DST"
echo "  $RULES_DST"
echo
echo "Done. Attach/detach runs for members of sudo/wheel without a password prompt."
echo "Adjust the group in $RULES_DST if needed; polkitd reloads rules.d automatically."
