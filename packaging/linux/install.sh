#!/bin/sh
# Toucan Camera Server — webcam permissions + runtime deps installer.
#
# Installs a udev rule that grants the active desktop user read/write access
# to /dev/video* via POSIX ACL, and ensures the host libusb-1.0 runtime is
# present. Run once after extracting the release archive. Re-running is safe
# (the rule file is overwritten with the same content).
#
# Why libusb is not bundled: the Canon (libEDSDK.so) and gphoto2 backends reach
# USB through libusb-1.0, which in turn drives the host's udev. libudev is
# coupled to the running systemd-udevd and MUST come from the host — bundling a
# build-time copy mismatches the target's udevd and segfaults on the first USB
# transfer. libusb-1.0 is therefore host-provided too. libudev1 is essential on
# every systemd distro; only libusb-1.0 may be missing on minimal images.

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

# Ensure the host libusb-1.0 runtime is present (needed by the Canon/gphoto2
# backends). Best-effort across the common package managers; a warning with
# manual instructions is printed if none is recognized.
ensure_libusb() {
    if ldconfig -p 2>/dev/null | grep -q 'libusb-1\.0\.so\.0'; then
        echo "libusb-1.0 already present."
        return
    fi
    echo "libusb-1.0 runtime not found — attempting to install it..."
    if command -v apt-get >/dev/null 2>&1; then
        apt-get update && apt-get install -y libusb-1.0-0
    elif command -v dnf >/dev/null 2>&1; then
        dnf install -y libusbx || dnf install -y libusb1
    elif command -v yum >/dev/null 2>&1; then
        yum install -y libusbx || yum install -y libusb1
    elif command -v pacman >/dev/null 2>&1; then
        pacman -Sy --noconfirm libusb
    elif command -v zypper >/dev/null 2>&1; then
        zypper install -y libusb-1_0-0
    else
        echo "Warning: no known package manager — install the libusb-1.0 runtime" >&2
        echo "         package manually (Debian/Ubuntu: libusb-1.0-0)." >&2
        return
    fi
    if ldconfig -p 2>/dev/null | grep -q 'libusb-1\.0\.so\.0'; then
        echo "libusb-1.0 installed."
    else
        echo "Warning: libusb-1.0 still not found after install attempt." >&2
    fi
}

ensure_libusb

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
