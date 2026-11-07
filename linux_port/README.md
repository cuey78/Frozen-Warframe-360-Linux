# Linux port — Winbond USB Display (0416:5302)

A working Linux implementation of the Windows **USBLCDNEW / TRCC** LCD control
software for the Winbond `USBDISPLAY` panel (USB ID `0416:5302`, firmware
`bcdDevice` 4.07 on the unit this was verified against).

It displays still images, solid colours, videos and a built-in test card on
the real hardware, with rotation and brightness applied host-side.

```
python3 usb_lcd_port.py info
python3 usb_lcd_port.py testcard
python3 usb_lcd_port.py color ff0000
python3 usb_lcd_port.py image photo.jpg --fit contain --brightness 80
python3 usb_lcd_port.py video clip.mp4 --fit cover --fps 15
```

---

## The one thing that matters: keep streaming

This firmware **does not persist a frame**. A single, perfectly-formed frame is
accepted (every USB write returns 512 bytes) and the panel still blanks or
reverts to its boot logo.

The panel only latches and keeps displaying while it is being fed. Every
command in this tool therefore re-sends its frame continuously (about 15–20 fps
on a 240×320 panel) until you stop it with Ctrl-C or `--duration`.

That single detail was the difference between "the USB calls all succeed" and
"there is actually a picture on the glass".

## Verified on hardware

* Handshake succeeds after a USB port reset: `PM=51`, `SUB=0`, `FBL=51`,
  serial `BP0M068`.
* Solid-colour streaming cycles red → green → blue → white → yellow →
  magenta → cyan → orange with no dropped frames.
* A marked 320×240 test card displays **upright**: red bar on top, text
  readable. The host pre-rotates the image 90° clockwise into the panel's
  native 240×320 portrait raster.
* Throughput ≈ 15–20 fps (301 × 512-byte writes per frame).

## Requirements

* Python 3.8+
* `pyusb` (`pip install pyusb`) and libusb-1.0
* `Pillow` (`pip install Pillow`) for images and the test card
* `ffmpeg` / `ffprobe` for video (optional)

## Install

```bash
pip3 install pyusb Pillow

# udev rule: makes /dev/bus/usb/BBB/DDD writable without root
sudo install -m 0644 linux_port/udev_rules.rules /etc/udev/rules.d/99-winbond_lcd.rules
sudo udevadm control --reload-rules
sudo udevadm trigger

python3 linux_port/usb_lcd_port.py info
```

Or run the helper, which also checks dependencies and verifies access:

```bash
bash linux_port/setup_lcd.sh        # rule installation needs sudo
```

You can also skip the udev rule and simply `chmod 666` the device node, or run
the tool as root. Raw USB access needs the node, not the `hidraw` node.

## Commands

Common options (available on every subcommand):

| Option | Meaning |
|---|---|
| `--vid`, `--pid` | USB IDs (default `0x0416:0x5302`) |
| `--fps N` | Target send rate (default 20; the panel tops out around 15–20) |
| `--duration S` | Stop after S seconds (default: stream until Ctrl-C) |
| `--rotation {0,90,180,270}` | Rotate the picture clockwise on the glass |
| `--brightness 0-100` | Dim the picture host-side (default 100) |
| `--no-reset` | Skip the USB port reset before handshaking |
| `-v`, `--verbose` | Handshake details and per-frame progress |

### `info`
Handshakes, prints PM/SUB/FBL, serial, resolution, wire raster, encoder and
byte order. `--json` emits machine-readable output.

### `testcard`
Streams the built-in orientation card (coloured bars + TOP/CENTER/BOT text).
This is the quickest way to confirm the install works.

### `color RRGGBB|name`
Streams a solid colour. Names: `red green blue white black yellow cyan
magenta orange purple pink grey warm`. `--brightness 0` gives black.

### `image FILE`
Streams a still image continuously.

* `--fit contain` (default) — letterbox, keep aspect ratio
* `--fit cover` — fill and centre-crop
* `--fit stretch` — exact fit, ignore aspect ratio

### `video FILE`
Plays a video through `ffmpeg`. Frames are resampled to `--fps`, scaled with
the same three fit modes, rotated and encoded as RGB565 in Python. Without
`--loop` it plays once and exits; with `--loop` it repeats; `--duration`
limits total playback time.

## How the protocol works

The Windows driver (`USBLCDNEW.dll`, `ThreadSendDeviceDataH`) uses LibUsbDotNet
on interface 0:

| Endpoint | Direction | Type | Used for |
|---|---|---|---|
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

> **EP 0x02 gets wedged** after a failed or partial transfer. The tool calls
> `dev.reset()`, waits 2.5 s and re-opens the device before the first
> handshake. This is not optional on a device that has just been used by a
> crashed process or another driver.

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

The packet is sent as **301 separate 512-byte writes**, never one bulk blob.
The firmware reads fixed-size reports and only latches the frame when it
arrives that way.

JPEG panels use `00 00` in place of `01 00` at offset 6 and carry the profile
resolution at offset 8; the payload starts with `FF D8`.

### Orientation and geometry

Handshake PM selects a profile (native landscape size, encoder and rotation
base). For PM 51:

* user canvas: 320 × 240
* encode base: 90° clockwise
* wire raster: 240 × 320
* pixel format: RGB565 little-endian

At user rotation 0 the picture is rotated 90° clockwise before encoding. For
user rotation 90/270 the user canvas becomes 240 × 320 so the wire raster is
still the panel's native portrait buffer.

## Supported device profiles

The PM/FBL table covers the panels documented by the reference project:
PM/FBL 36, 37, 49, 50, 51, 52, 53, 54, 58, 60, 64, 72, 100, 101, 102, 114,
128, 129, 192, 224. Unknown panel codes fall back to the documented 320 × 320
RGB565 default rather than guessing silently.

## Not implemented

* **OTA firmware update.** No firmware image or confirmed OTA packet format is
  available, and flashing blind risks bricking the panel.
* **Windows shared-memory RGB operations.** These (`shareMemory_ImageRGB`,
  block size 691200, etc.) are Windows IPC between `TRCC.exe` and a helper
  process. They are not commands sent to the LCD and have no Linux equivalent.
* **Logo / power-off / backlight device commands.** On this HID panel the
  reference implementation applies brightness and rotation host-side, and the
  backlight is not a device command. Power-off (`AA BB CC DD`) is a Windows
  shared-memory control path. `--brightness` is applied to pixels instead.

## Troubleshooting

**`device not found`** — check `lsusb` for `0416:5302`. If it is missing,
replug it; if it is present but unreadable, fix permissions:
`sudo chmod 666 /dev/bus/usb/BBB/DDD`.

**Handshake times out / `Pipe error`** — the endpoint is wedged. The tool
resets automatically, but if a previous process is still running, kill it
first. `usbreset 0416:5302` (from usbutils) is the equivalent external fix
when available.

**Writes succeed but the panel stays on its logo** — something stopped the
stream. This tool never sends a single frame: keep the process running. A
`--duration` that has elapsed will also stop the feed.

**Image is sideways** — use `--rotation 90` / `--rotation 270`. At 0 the
verified test card is upright on the reference unit; a panel mounted
differently in its cooler may need an offset.

**Video stutters** — the panel accepts ~15–20 fps. Lower `--fps` or use a
smaller source. Encoding is pure Python; a 240 × 320 frame takes ~10 ms.

**`SET_REPORT` / control-pipe errors** — expected. The HID `SET_REPORT`
control transfer returns `EPIPE` on this firmware; raw EP `0x02` interrupt
writes are the transport that works.

## Files

| File | Purpose |
|---|---|
| `usb_lcd_port.py` | The Linux port (this tool) |
| `udev_rules.rules` | Non-root USB access rule |
| `setup_lcd.sh` | Dependency check + rule install + access verification |

## License

Provided as-is for interoperability with the Winbond USB Display. The
original Windows software and its protocol remain the property of their
respective authors.
