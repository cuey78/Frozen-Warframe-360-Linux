#!/usr/bin/env bash
# install-usb-lcd.sh - Installer for the Winbond USBDISPLAY LCD port.
# Run with: sudo bash install-usb-lcd.sh
#
# Supports still images (PNG/JPG) and videos (MP4/MKV/MOV/AVI/GIF/WebM/MPEG)
# via the Rust `video` command (needs ffmpeg in PATH for video).

set -Eeuo pipefail

info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32mOK\033[0m   %s\n' "$*"; }
warn() { printf '\033[1;33mWARN\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31mERR\033[0m  %s\n' "$*" >&2; }

ask() {
    local prompt=$1 default=${2:-} answer
    if [[ -n $default ]]; then
        printf '%s [%s]: ' "$prompt" "$default" >&2
    else
        printf '%s: ' "$prompt" >&2
    fi
    IFS= read -r answer || true
    printf '%s\n' "${answer:-$default}"
}

require_root() {
    if (( EUID != 0 )); then
        err "This installer must be run as root."
        err "Run: sudo bash $0"
        exit 1
    fi
}

on_error() {
    local exit_code=$?
    err "Installation failed at line ${BASH_LINENO[0]} (exit ${exit_code})."
    exit "$exit_code"
}
trap on_error ERR
require_root

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"

# Locate the prebuilt Rust executable.
RUST_BIN=""
for candidate in \
    "$SCRIPT_DIR/rust_lcd/target/release/usb_lcd" \
    "$SCRIPT_DIR/target/release/usb_lcd" \
    "$SCRIPT_DIR/usb_lcd"; do
    if [[ -f $candidate && -x $candidate ]]; then
        RUST_BIN=$candidate
        break
    fi
done

if [[ -z $RUST_BIN ]]; then
    err "Could not find the compiled Rust binary (usb_lcd)."
    err "Build it first: cd '$SCRIPT_DIR/rust_lcd' && cargo build --release --bin usb_lcd"
    exit 1
fi

command -v systemctl >/dev/null 2>&1 || { err "systemctl was not found; this installer requires systemd."; exit 1; }
command -v install >/dev/null 2>&1 || { err "The install command is required."; exit 1; }

printf '\n'
info "USB LCD Installer"
printf '%s\n' '=================='
info "Found Rust binary: $RUST_BIN"

# Prefer the invoking desktop/login user. Do not silently run the service as root.
DEFAULT_USER="${SUDO_USER:-}"
if [[ -z $DEFAULT_USER || $DEFAULT_USER == root ]]; then
    DEFAULT_USER="$(logname 2>/dev/null || true)"
fi
if [[ $DEFAULT_USER == root ]]; then DEFAULT_USER=""; fi

SERVICE_USER="$(ask 'System user to run the LCD service as' "$DEFAULT_USER")"
if [[ -z $SERVICE_USER || $SERVICE_USER == root ]] || ! id "$SERVICE_USER" >/dev/null 2>&1; then
    err "Please select an existing, non-root system user."
    exit 1
fi
SERVICE_GROUP="$(id -gn "$SERVICE_USER")"

DEFAULT_FILE=""
if [[ -f $SCRIPT_DIR/rust_lcd/fett.png ]]; then
    DEFAULT_FILE="$SCRIPT_DIR/rust_lcd/fett.png"
elif [[ -f $SCRIPT_DIR/linux_port/fett.png ]]; then
    DEFAULT_FILE="$SCRIPT_DIR/linux_port/fett.png"
fi

DISPLAY_FILE="$(ask 'File to display (image PNG/JPG or video MP4/MKV/MOV/AVI/GIF)' "$DEFAULT_FILE")"
if [[ ! -f $DISPLAY_FILE || ! -r $DISPLAY_FILE ]]; then
    err "File does not exist or is not readable: $DISPLAY_FILE"
    exit 1
fi
DISPLAY_ABS="$(realpath -- "$DISPLAY_FILE")"
DISPLAY_EXT="${DISPLAY_ABS##*.}"
DISPLAY_EXT="${DISPLAY_EXT,,}"
case "$DISPLAY_EXT" in
    png|jpg|jpeg)
        DISPLAY_KIND=image
        ;;
    mp4|m4v|mkv|mov|avi|gif|webm|mpeg|mpg)
        DISPLAY_KIND=video
        ;;
    *)
        err "Unsupported file extension '.$DISPLAY_EXT'. Use PNG/JPG/JPEG for an image, or MP4/MKV/MOV/AVI/GIF/WEBM for a video."
        exit 1
        ;;
esac

VIDEO_FPS=""
VIDEO_FIT=""
VIDEO_ROTATION=""
VIDEO_BRIGHTNESS=""
if [[ $DISPLAY_KIND == video ]]; then
    command -v ffmpeg >/dev/null 2>&1 || {
        err "ffmpeg is required for video playback but was not found in PATH."
        err "Install it first, e.g.: sudo dnf install ffmpeg   (Fedora)"
        err "                        or: sudo apt install ffmpeg  (Debian/Ubuntu)"
        exit 1
    }
    VIDEO_FPS="$(ask 'Video FPS (panel tops out at ~15-20)' '15')"
    if ! [[ $VIDEO_FPS =~ ^[0-9]+$ ]] || (( VIDEO_FPS < 1 || VIDEO_FPS > 30 )); then
        err "Video FPS must be an integer between 1 and 30."
        exit 1
    fi
    VIDEO_FIT="$(ask 'Video fit (contain|cover|stretch)' 'cover')"
    VIDEO_FIT="${VIDEO_FIT,,}"
    case "$VIDEO_FIT" in
        contain|cover|stretch) ;;
        *) err "Video fit must be contain, cover, or stretch."; exit 1 ;;
    esac
    VIDEO_ROTATION="$(ask 'Video rotation in degrees (0|90|180|270)' '0')"
    case "$VIDEO_ROTATION" in
        0|90|180|270) ;;
        *) err "Video rotation must be 0, 90, 180, or 270."; exit 1 ;;
    esac
    VIDEO_BRIGHTNESS="$(ask 'Video brightness percent (0-100)' '100')"
    if ! [[ $VIDEO_BRIGHTNESS =~ ^[0-9]+$ ]] || (( VIDEO_BRIGHTNESS > 100 )); then
        err "Video brightness must be an integer between 0 and 100."
        exit 1
    fi
fi

SERVICE_INSTANCE="usb-lcd@${SERVICE_USER}.service"
DISPLAY_DEST_DIR="/var/lib/usb-lcd"
if [[ $DISPLAY_KIND == video ]]; then
    DISPLAY_DEST="$DISPLAY_DEST_DIR/display-video.$DISPLAY_EXT"
    EXEC_START="/usr/local/bin/usb-lcd video $DISPLAY_DEST --fit $VIDEO_FIT --fps $VIDEO_FPS --rotation $VIDEO_ROTATION --brightness $VIDEO_BRIGHTNESS --loop"
    TEST_CMD="/usr/local/bin/usb-lcd video \"$DISPLAY_DEST\" --fit $VIDEO_FIT --fps $VIDEO_FPS --rotation $VIDEO_ROTATION --brightness $VIDEO_BRIGHTNESS --loop --duration 10"
else
    DISPLAY_DEST="$DISPLAY_DEST_DIR/display-image.$DISPLAY_EXT"
    EXEC_START="/usr/local/bin/usb-lcd image $DISPLAY_DEST --fit cover --fps 20"
    TEST_CMD="/usr/local/bin/usb-lcd image \"$DISPLAY_DEST\" --fit cover --fps 20 --duration 10"
fi

info "Selected file: $DISPLAY_ABS"
info "Selected kind: $DISPLAY_KIND"
if [[ $DISPLAY_KIND == video ]]; then
    info "Video options: fps=$VIDEO_FPS fit=$VIDEO_FIT rotation=$VIDEO_ROTATION brightness=$VIDEO_BRIGHTNESS loop=on"
fi
info "Service user/group: $SERVICE_USER:$SERVICE_GROUP"

info "Installing binary ..."
install -D -o root -g root -m 0755 "$RUST_BIN" /usr/local/bin/usb-lcd
ok "Binary installed: /usr/local/bin/usb-lcd"

info "Installing $DISPLAY_KIND ..."
install -d -o root -g root -m 0755 "$DISPLAY_DEST_DIR"
install -o root -g root -m 0644 "$DISPLAY_ABS" "$DISPLAY_DEST"
ok "$DISPLAY_KIND installed: $DISPLAY_DEST"

# Keep project-specific rules and add a narrowly scoped permission rule.
# Never chmod every USB device on the host.
UDEV_DIR="/etc/udev/rules.d"
UDEV_FILE="$UDEV_DIR/99-usb-lcd.rules"
UDEV_SRC="$SCRIPT_DIR/linux_port/udev_rules.rules"
install -d -m 0755 "$UDEV_DIR"
if [[ -f $UDEV_SRC ]]; then
    install -m 0644 "$UDEV_SRC" "$UDEV_FILE"
else
    warn "linux_port/udev_rules.rules was not found; creating a device permission rule only."
    : > "$UDEV_FILE"
fi
cat >> "$UDEV_FILE" <<UDEV_EOF

# Allow only the selected service account's primary group to access this LCD.
SUBSYSTEM=="usb", ATTR{idVendor}=="0416", ATTR{idProduct}=="5302", MODE="0660", GROUP="$SERVICE_GROUP"
UDEV_EOF
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules
    udevadm trigger --subsystem-match=usb --attr-match=idVendor=0416 --attr-match=idProduct=5302 || true
    udevadm settle --timeout=10 || true
    ok "udev rules installed and reloaded."
else
    warn "udevadm is unavailable; udev rules were installed but not reloaded."
fi

SERVICE_FILE="/etc/systemd/system/usb-lcd@.service"
info "Installing systemd service template ..."
cat > "$SERVICE_FILE" <<SERVICE_EOF
[Unit]
Description=Winbond USBDISPLAY (0416:5302) LCD $DISPLAY_KIND runner for %i
After=systemd-udevd.service
StartLimitIntervalSec=60
StartLimitBurst=10

[Service]
Type=simple
User=%i
Group=$SERVICE_GROUP
WorkingDirectory=/usr/local/bin
ExecStart=$EXEC_START
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
SERVICE_EOF
chmod 0644 "$SERVICE_FILE"
ok "Service template installed: $SERVICE_FILE"

info "Enabling and starting $SERVICE_INSTANCE ..."
systemctl daemon-reload
systemctl enable "$SERVICE_INSTANCE"
systemctl restart "$SERVICE_INSTANCE" || systemctl start "$SERVICE_INSTANCE"

sleep 2
if systemctl is-active --quiet "$SERVICE_INSTANCE"; then
    ok "Service is running: $SERVICE_INSTANCE"
else
    warn "The service did not become active. Recent logs follow:"
    systemctl --no-pager --full status "$SERVICE_INSTANCE" || true
    journalctl -u "$SERVICE_INSTANCE" -n 30 --no-pager || true
    warn "Check USB detection and the udev rule before retrying."
fi

printf '\n'
info "Installation complete"
printf '\n'
printf 'Binary:  %s\n' /usr/local/bin/usb-lcd
printf 'Kind:    %s\n' "$DISPLAY_KIND"
printf 'File:    %s\n' "$DISPLAY_DEST"
if [[ $DISPLAY_KIND == video ]]; then
    printf 'Video:   fps=%s fit=%s rotation=%s brightness=%s loop=on\n' "$VIDEO_FPS" "$VIDEO_FIT" "$VIDEO_ROTATION" "$VIDEO_BRIGHTNESS"
fi
printf 'Service: %s\n' "$SERVICE_INSTANCE"
printf '\n'
printf 'Test manually (10 seconds):\n  sudo %s\n' "$TEST_CMD"
printf '\nManage the service:\n'
printf '  sudo systemctl status %s\n' "$SERVICE_INSTANCE"
printf '  sudo journalctl -u %s -f\n' "$SERVICE_INSTANCE"
printf '  sudo systemctl restart %s\n' "$SERVICE_INSTANCE"
