# TRCCCAP Linux Port - Thermalright Frozen warframe-360 Control Software

Linux port of the Windows **USBLCDNEW / TRCC** LCD control software for the **Winbond USBDISPLAY** panel (USB ID `0416:5302`, firmware `bcdDevice` 4.07).

Displays still images, solid colours, videos and a built-in test card on the real hardware, with rotation and brightness applied host-side.

NOTE:
Upfront Disclose: This was Created using AI reverse engineering - Since no one had a working option i made one (with help) , im not a software engineer so anyone can fork this and improve it , it works for what i need.

```
Device:        Winbond Electronics Corp. USBDISPLAY
USB ID:        0416:5302
Panel:         320x240 RGB565 (portrait), little-endian
Wire raster:   240x320
Transport:     pyusb / rusb over interrupt EP 0x02 (OUT) + EP 0x81 (IN)
```

---

## Repository layout

```
/
├── README.md                   # This file
├── linux_port/                 # Python port (pyusb + Pillow)
│   ├── usb_lcd_port.py         # Main port: info/testcard/color/image/video
│   ├── README.md               # Python-specific documentation
│   ├── setup_lcd.sh            # Dependency check + udev rule install
│   ├── udev_rules.rules        # Rootless USB access rule
│   └── fett.png                # Sample image
├── rust_lcd/                   # Rust port (rusb 0.9 + image)
│   ├── Cargo.toml
│   ├── src/main.rs             # Main port: info/testcard/color/raw/image/video
│   ├── src/main_test.rs        # Known-compiling reference binary
│   └── .gitignore
└── .gitignore
```

---

## The one thing that matters: keep streaming

This firmware **does not persist a frame**. A single, perfectly-formed frame is accepted (every USB write returns 512 bytes) and the panel still blanks or reverts to its boot logo.

The panel only latches and keeps displaying while it is being fed. Every command in this tool therefore re-sends its frame continuously (about 15–20 fps on a 240×320 panel) until you stop it with `Ctrl-C` or `--duration`.

That single detail was the difference between "the USB calls all succeed" and "there is actually a picture on the glass."

---

## Verified on hardware

* Handshake succeeds after a USB port reset: `PM=51`, `SUB=0`, `FBL=51`, serial `BP0M068`.
* Solid-colour streaming cycles red → green → blue → white → yellow → magenta → cyan → orange with no dropped frames.
* A marked 320×240 test card displays **upright**: red bar on top, text readable. The host pre-rotates the image 90° clockwise into the panel's native 240×320 portrait raster.
* Throughput ≈ 15–20 fps (301 × 512-byte writes per frame).

---

## Quick start

### Python port (recommended — full feature set)

```bash
# 1. Install dependencies
pip3 install pyusb Pillow

# 2. Install udev rule (rootless USB access)
sudo install -m 0644 linux_port/udev_rules.rules /etc/udev/rules.d/99-winbond_lcd.rules
sudo udevadm control --reload-rules
sudo udevadm trigger

# 3. Verify access
python3 linux_port/usb_lcd_port.py info

# 4. Stream the test card
python3 linux_port/usb_lcd_port.py testcard

# 5. Solid colour
python3 linux_port/usb_lcd_port.py color ff0000 --duration 10

# 6. Image (PNG/JPEG)
python3 linux_port/usb_lcd_port.py image photo.jpg --fit contain --brightness 80 --duration 30

# 7. Video (via ffmpeg)
python3 linux_port/usb_lcd_port.py video clip.mp4 --fit cover --fps 15
```

Or run the setup helper (rule installation needs `sudo`):

```bash
bash linux_port/setup_lcd.sh
```

### Rust port

```bash
cd rust_lcd
cargo build --release --bin usb_lcd

./target/release/usb_lcd info
./target/release/usb_lcd testcard --duration 10
./target/release/usb_lcd color ff0000
./target/release/usb_lcd image fett.png --duration 10
./target/release/usb_lcd raw file.rgb24 --duration 10
./target/release/usb_lcd video clip.mp4 --fit cover --fps 15
./target/release/usb_lcd video clip.mp4 --loop --duration 30
```

Video playback in both ports decodes through the external `ffmpeg` binary, so install it first (`sudo dnf install ffmpeg` / `sudo apt install ffmpeg`) — the Rust binary needs it in `PATH` for the `video` command only.

---

## Commands

Common options (every subcommand):

| Option | Meaning |
|--------|---------|
| `--vid`, `--pid` | USB IDs (default `0x0416:0x5302`) |
| `--fps N` | Target send rate (default 20; panel tops out ~15–20) |
| `--duration S` | Stop after S seconds (default: stream until Ctrl-C) |
| `--rotation {0,90,180,270}` | Rotate the picture clockwise on the glass |
| `--brightness 0-100` | Dim the picture host-side (default 100) |
| `--no-reset` | Skip the USB port reset before handshaking |
| `-v`, `--verbose` | Handshake details and per-frame progress |

### `info`
Handshakes, prints PM/SUB/FBL, serial, resolution, wire raster, encoder and byte order. `--json` emits machine-readable output (Python only).

### `testcard`
Streams the built-in orientation card (coloured bars + TOP/CENTER/BOT text). Quickest way to confirm the install works.

### `color RRGGBB|name`
Streams a solid colour. Names: `red green blue white black yellow cyan magenta orange purple pink grey warm`. `--brightness 0` gives black.

### `image FILE` (Python + Rust)
Streams a still image continuously. Python honours the `--fit` modes below; the Rust port currently always stretches the image onto the panel (use Python's `image --fit ...` for fitted stills).

* `--fit contain` (default) — letterbox, keep aspect ratio
* `--fit cover` — fill and centre-crop
* `--fit stretch` — exact fit, ignore aspect ratio

### `video FILE` (Python + Rust)
Plays a video through `ffmpeg` (mp4/mkv/mov/avi/gif — anything ffmpeg decodes). Frames are resampled to `--fps`, scaled with the same three fit modes, rotated and encoded as RGB565 host-side (Python, or Rust in the Rust port). Without `--loop` it plays once and exits; with `--loop` it repeats; `--duration` limits total playback time.

* `--fit cover` (default) — fill and centre-crop
* `--fit contain` — letterbox, keep aspect ratio
* `--fit stretch` — exact fit, ignore aspect ratio

### `raw FILE.rgb24` (Rust only)
Streams a packed 320×240 RGB24 file.

---

## How the protocol works

The Windows driver (`USBLCDNEW.dll`, `ThreadSendDeviceDataH`) uses LibUsbDotNet on interface 0:

| Endpoint | Direction | Type | Used for |
|----------|-----------|------|----------|
| `0x02` | OUT | interrupt, 512 B | handshake and frame data |
| `0x81` | IN | interrupt, 8 B | handshake reply |

### Handshake

A 512-byte packet on EP `0x02`:

```
DA DB DC DD  00×8  01 00 00 00  00×4  ...zero padding...
```

Then read up to 512 bytes from EP `0x81`. On this unit the reply is 36 bytes:

```
da db dc dd 00 33 00 00 00 00 00 00 01 00 00 00 10 00 00 00
42 50 30 4d 30 36 38 03 00 96 a6 21 43 03 e2 78
```

* `reply[5]` = PM (`0x33` = 51)
* `reply[4]` = SUB
* FBL is derived from PM/SUB (PM == FBL unless an override applies)
* when `reply[16] == 0x10`, the serial ASCII starts at `reply[20]`

> **EP 0x02 gets wedged** after a failed or partial transfer. The tool calls `dev.reset()`, waits 2.5 s and re-opens the device before the first handshake. This is not optional on a device that has just been used by a crashed process or another driver.

### Frame packet

```
DA DB DC DD        magic
02 00              command = PICTURE
01 00              RGB565 payload indicator
F0 00 40 01        wire raster 240 × 320, little-endian
02 00 00 00
00 58 02 00        payload length (153 600 bytes), little-endian
<R/G/B 5-6-5 pixels, little-endian>
<zero pad to a multiple of 512>
```

The packet is sent as **301 separate 512-byte writes**, never one bulk blob. The firmware reads fixed-size reports and only latches the frame when it arrives that way.

### Orientation and geometry

Handshake PM selects a profile (native landscape size, encoder and rotation base). For PM 51:

* user canvas: 320 × 240
* encode base: 90° clockwise
* wire raster: 240 × 320
* pixel format: RGB565 little-endian

At user rotation 0 the picture is rotated 90° clockwise before encoding. For user rotation 90/270 the user canvas becomes 240 × 320 so the wire raster is still the panel's native portrait buffer.

---

## Supported device profiles

The PM/FBL table covers the panels documented by the reference project: PM/FBL 36, 37, 49, 50, 51, 52, 53, 54, 58, 60, 64, 72, 100, 101, 102, 114, 128, 129, 192, 224. Unknown panel codes fall back to the documented 320 × 320 RGB565 default rather than guessing silently.

---

## Not implemented

* **OTA firmware update.** No firmware image or confirmed OTA packet format is available, and flashing blind risks bricking the panel.
* **Windows shared-memory RGB operations.** These are Windows IPC between `TRCC.exe` and a helper process. They are not commands sent to the LCD and have no Linux equivalent.
* **Logo / power-off / backlight device commands.** On this HID panel brightness and rotation are applied host-side; the backlight is not a device command. `--brightness` is applied to pixels instead.

---

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `device not found` | Check `lsusb` for `0416:5302`. Replug if missing. |
| Permission denied | Install the udev rule (`sudo install ...`) or `sudo chmod 666 /dev/bus/usb/BBB/DDD`. |
| Handshake times out / `Pipe error` | EP 0x02 is wedged — the tool resets automatically, but kill any other process using the device first. |
| Writes succeed but panel stays on logo | Something stopped the stream. Keep the process running; `--duration` elapsed also stops the feed. |
| Image is sideways | Use `--rotation 90` / `--rotation 270`. |
| Video stutters | Lower `--fps` (panel accepts ~15–20 fps). |
| `sudo` fails with "no new privileges" | This container environment blocks sudo; run as a user with USB node access instead (udev rule `MODE=0666`). |

---

## Branches

| Branch | Contents |
|--------|----------|
| `main` | Both Python and Rust ports |
| `python` | Python port only (`linux_port/`) |
| `rust` | Rust port only (`rust_lcd/`) |

---

## Running as a background service (boot + reboot)

The `linux_port/` directory includes a systemd service template and an
installer so the panel keeps displaying an image automatically on every boot.

### Install (run once as root)

```bash
sudo bash install-usb-lcd.sh
```

The installer will interactively prompt you for:

1. **System user** to run the LCD service as (defaults to your current non-root user)
2. **Image file** to display (defaults to `linux_port/fett.png` if present)

It then:

1. Copies the Rust binary to `/usr/local/bin/usb-lcd`
2. Copies your image to `/var/lib/usb-lcd/display-image.<ext>`
3. Installs a udev rule for rootless USB access (group-restricted to the service user)
4. Installs a systemd template service `usb-lcd@.service`
5. Enables and starts `usb-lcd@<username>.service`

Example:

```bash
sudo bash install-usb-lcd.sh
# System user to run the LCD service as [cuey]: <Enter>
# Image file to display (PNG/JPEG) [/home/.../linux_port/fett.png]: /path/to/my-image.png
```

### Manage the service

The service is installed as a template instance `usb-lcd@<username>.service`:

```bash
# Check status
sudo systemctl status usb-lcd@cuey.service

# View live logs
sudo journalctl -u usb-lcd@cuey.service -f

# Stop / start / restart
sudo systemctl stop usb-lcd@cuey.service
sudo systemctl start usb-lcd@cuey.service
sudo systemctl restart usb-lcd@cuey.service

# Disable (stop running on boot)
sudo systemctl disable usb-lcd@cuey.service
```

### How it works

The installer installs a systemd template service (`usb-lcd@.service`). When
enabled for a user (e.g. `usb-lcd@cuey.service`), it runs:

```
/usr/local/bin/usb-lcd image /var/lib/usb-lcd/display-image.png --fit cover --fps 20
```

as that user, restarting on failure. The binary continuously streams the image
to the panel (~15–20 fps). Because the panel blanks when data stops, the stream
never stops until the service is stopped.

The service runs as a non-root user, with the udev rule granting that user's
primary group access to the `0416:5302` USB device.

To change the image or options, edit
`/etc/systemd/system/usb-lcd@.service` and run
`sudo systemctl daemon-reload && sudo systemctl restart usb-lcd@<username>.service`.

---

## Releases

**v1.1.0 — Rust 0.2.0 (video playback).** The Rust port gains the `video`
command (ffmpeg decode, RGB565 encode in Rust, verified on hardware), plus
`--vid`/`--pid`, `--fit`, `--rotation`, `--brightness` and `--loop` support
on `video`, and `--brightness` on the other Rust commands.

Build the release binary with:

```bash
cd rust_lcd
cargo build --release --bin usb_lcd
./target/release/usb_lcd video clip.mp4 --fit cover --fps 15
```

---

## License

MIT — see [LICENSE](LICENSE). Do whatever you want with the code; forks and
improvements are welcome.

### Protocol note

This project is an independent, clean-room-style interoperability effort for
the Winbond USB Display panel. The original Windows software (`USBLCDNEW.dll`,
`TRCC.exe`) and its protocol remain the property of their respective authors.
No original binaries are included in this repository; the protocol details
here were derived from observing USB traffic on hardware the author owns.
