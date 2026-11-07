#!/bin/bash
# Setup helper for the Winbond USB Display Linux port (0416:5302).
#
# Installs the udev rule that makes /dev/bus/usb/BBB/DDD writable without
# root, checks the Python dependencies, and verifies that pyusb can see the
# device.  Run it with sudo (only the rule installation needs root).

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RULE_SRC="$SCRIPT_DIR/udev_rules.rules"
RULE_DST="/etc/udev/rules.d/99-winbond_lcd.rules"

echo "=== Winbond USB Display Linux port setup ==="
echo

# --- dependencies ---------------------------------------------------------
echo "Checking Python dependencies..."
python3 - <<'PY'
import sys
missing = []
try:
    import usb
    print(f"  pyusb:  {usb.__version__}")
except Exception:
    missing.append("pyusb")
try:
    import PIL
    print(f"  Pillow: {PIL.__version__}")
except Exception:
    missing.append("Pillow")
if missing:
    print("  missing:", ", ".join(missing))
    print("  install with: pip3 install " + " ".join(missing))
    sys.exit(1)
PY

if command -v ffmpeg >/dev/null 2>&1; then
    echo "  ffmpeg: $(command -v ffmpeg)"
else
    echo "  ffmpeg: not found (video playback will be unavailable)"
fi
echo

# --- udev rule ------------------------------------------------------------
if [ "$(id -u)" -eq 0 ]; then
    install -m 0644 "$RULE_SRC" "$RULE_DST"
    udevadm control --reload-rules || true
    udevadm trigger --subsystem-match=usb || true
    echo "Installed $RULE_DST"
else
    echo "Not running as root; skipping udev rule installation."
    echo "To install it, run:"
    echo "  sudo install -m 0644 '$RULE_SRC' '$RULE_DST'"
    echo "  sudo udevadm control --reload-rules && sudo udevadm trigger"
fi
echo

# --- verify access --------------------------------------------------------
echo "Checking device access..."
python3 - <<'PY'
import usb.core
dev = usb.core.find(idVendor=0x0416, idProduct=0x5302)
if dev is None:
    print("  device not found -- is it plugged in?")
    raise SystemExit(1)
print(f"  found 0416:5302 on bus {dev.bus}, address {dev.address} "
      f"(bcdDevice {dev.bcdDevice:#06x})")
try:
    # A configuration read proves the process may open the device.
    dev.get_active_configuration()
    print("  configuration readable -- permissions OK")
except Exception as exc:
    print(f"  cannot open device: {exc}")
    print("  install the udev rule above, or run: "
          f"sudo chmod 666 /dev/bus/usb/{dev.bus:03d}/{dev.address:03d}")
    raise SystemExit(1)
PY

echo
echo "=== Setup complete ==="
echo "Try:  python3 '$SCRIPT_DIR/usb_lcd_port.py' info"
echo "      python3 '$SCRIPT_DIR/usb_lcd_port.py' testcard"
