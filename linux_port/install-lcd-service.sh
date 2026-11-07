#!/bin/bash
#
# install-lcd-service.sh
# Installs the Winbond USBDISPLAY systemd service and udev rules.
#
# Usage:
#   sudo bash install-lcd-service.sh <username> <image_file>
#
# Example:
#   sudo bash install-lcd-service.sh cuey /home/cuey/test2/TRCCCAP/rust_lcd/fett.png

set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "This script must be run as root (use sudo)." >&2
    exit 1
fi

USERNAME="${1:?Usage: $0 <username> <image_file>}"
IMAGE_FILE="${2:?Usage: $0 <username> <image_file>}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=== Installing Winbond USBDISPLAY systemd service ==="
echo

# 1. Install udev rules
echo "[1/4] Installing udev rules..."
install -m 0644 "$SCRIPT_DIR/udev_rules.rules" /etc/udev/rules.d/99-winbond_lcd.rules
udevadm control --reload-rules || true
udevadm trigger --subsystem-match=usb || true
echo "  udev rule installed."

# 2. Make runner executable
echo "[2/4] Making runner executable..."
chmod +x "$SCRIPT_DIR/winbond-lcd-runner"
echo "  runner ready."

# 3. Install systemd service template
echo "[3/4] Installing systemd service..."
install -m 0644 "$SCRIPT_DIR/winbond-lcd@.service" /etc/systemd/system/winbond-lcd@.service
systemctl daemon-reload
echo "  service template installed."

# 4. Enable and start the service for the given user
echo "[4/4] Enabling and starting service for user $USERNAME..."
systemctl enable "winbond-lcd@$USERNAME.service" || true
systemctl start "winbond-lcd@$USERNAME.service" || true
echo "  service enabled and started."

echo
echo "=== Installation complete ==="
echo
echo "Check status:  systemctl status winbond-lcd@$USERNAME.service"
echo "View logs:     journalctl -u winbond-lcd@$USERNAME.service -f"
echo "Stop:          systemctl stop winbond-lcd@$USERNAME.service"
echo "Restart:       systemctl restart winbond-lcd@$USERNAME.service"
echo
echo "The image file is: $IMAGE_FILE"
echo "Edit /etc/systemd/system/winbond-lcd@.service to change the image or options."
