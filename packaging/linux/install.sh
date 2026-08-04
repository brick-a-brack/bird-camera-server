#!/bin/sh
# Toucan Camera Server — webcam permissions installer.
#
# Installs a udev rule that grants the active desktop user read/write access
# to /dev/video* via POSIX ACL. Run once after extracting the release archive.
# Re-running is safe (the rule file is overwritten with the same content).
#
# libusb-1.0 is bundled next to the binary (a single copy shared by the Canon,
# gphoto2 and Sony backends — see build.rs), so no libusb host package is
# required. libudev is intentionally NOT bundled (it is coupled to the host's
# systemd-udevd) and is present on every systemd distro.

set -e

RULE="99-toucan-camera.rules"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="$SCRIPT_DIR/$RULE"
DST="/etc/udev/rules.d/$RULE"

if [ ! -f "$SRC" ]; then
    echo "Error: $RULE not found next to install.sh" >&2
    exit 1
fi

# Self-elevate. Re-exec with sudo and pass through any args.
if [ "$(id -u)" -ne 0 ]; then
    echo "Root required — re-running with sudo..."
    exec sudo "$0" "$@"
fi

install -m 644 "$SRC" "$DST"
echo "Installed $DST"

if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules
    udevadm trigger --subsystem-match=video4linux
    echo "udev rules reloaded."
else
    echo "Warning: udevadm not found — a reboot will be needed to apply the rule." >&2
fi

echo
echo "Done. Webcams are now accessible to the active desktop user without"
echo "any group membership change. Re-plug any device that was already connected."
