#!/usr/bin/env python3
"""
Linux port of the Windows USBLCDNEW / TRCC LCD control software.

Target hardware: Winbond USB Display, USB ID 0416:5302.
  Interface 0 (HID):
    EP 0x02 OUT, interrupt, 512-byte packets  -> handshake, frames
    EP 0x81 IN,  interrupt, 8-byte packets    -> handshake reply

The Windows driver (``USBLCDNEW.dll``, ``ThreadSendDeviceDataH``) speaks a
20-byte "type 2" packet header followed by a 512-byte-aligned payload.  This
port reproduces that wire format exactly, including the detail that matters
most on this firmware:

    The panel must be fed continuously.  A single frame is accepted but the
    firmware blanks/reverts when it stops receiving data.  Every command in
    this tool therefore streams its frame at a steady rate until stopped.

Protocol summary (verified against 0416:5302, bcdDevice 4.07, PM=51):

  Handshake (512 bytes, EP 0x02 OUT):
      DA DB DC DD  + 8 zero bytes + 01 00 00 00 + 4 zero bytes + padding
    Reply (EP 0x81 IN, 36 bytes on this unit):
      DA DB DC DD ?? PM SUB ... serial ...
      PM = byte[5], SUB = byte[4], serial ASCII at bytes[20:36]

  Frame (EP 0x02 OUT, split into 512-byte writes):
      DA DB DC DD 02 00 01 00 <wire_w:u16 LE> <wire_h:u16 LE>
      02 00 00 00 <payload_len:u32 LE> <RGB565 pixels> <zero padding>
    JPEG payloads use ``00 00`` at [6:8] and the profile resolution at [8:12].

Orientation: the panel is physically portrait.  The host pre-rotates the
landscape user image 90 degrees clockwise (PIL ``ROTATE_270``) into the
240x320 wire raster.  Verified on hardware: red bar on top, text upright.

Usage:
    python3 usb_lcd_port.py info
    python3 usb_lcd_port.py testcard
    python3 usb_lcd_port.py color ff0000
    python3 usb_lcd_port.py image photo.jpg --fit contain --brightness 80
    python3 usb_lcd_port.py video clip.mp4 --fit cover --fps 15

Requires: pyusb, Pillow; ffmpeg/ffprobe for video.
"""

from __future__ import annotations

import argparse
import io
import os
import signal
import struct
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from typing import Iterator, Optional

try:
    import usb.core
    import usb.util
except ImportError:  # pragma: no cover - dependency guard
    sys.exit("pyusb is required: pip install pyusb")

try:
    from PIL import Image, ImageDraw
except ImportError:  # pragma: no cover - dependency guard
    sys.exit("Pillow is required: pip install Pillow")

LANCZOS = getattr(Image, "Resampling", Image).LANCZOS

# ---------------------------------------------------------------------------
# Wire constants
# ---------------------------------------------------------------------------

VENDOR_ID = 0x0416
PRODUCT_ID = 0x5302

MAGIC = bytes([0xDA, 0xDB, 0xDC, 0xDD])
CMD_PICTURE = 0x0002
EP_OUT = 0x02
EP_IN = 0x81
CHUNK = 512

HANDSHAKE = MAGIC + b"\x00" * 8 + b"\x01\x00\x00\x00" + b"\x00" * 4
HANDSHAKE += b"\x00" * (512 - len(HANDSHAKE))

# The type-2 RGB565 header always advertises the 320x240 family's wire
# raster.  Kept as in the Windows driver / reference implementation.
RGB565_HEADER_W = 240
RGB565_HEADER_H = 320

FRAME_DELAY = 0.001  # small settle after a full frame, as in the C# driver


# ---------------------------------------------------------------------------
# Device profiles (PM/FBL -> geometry + encoder)
# ---------------------------------------------------------------------------

@dataclass(frozen=True)
class DeviceProfile:
    """Everything needed to render and encode for a panel."""

    width: int                 # user/landscape width
    height: int                # user/landscape height
    jpeg: bool = False         # JPEG payload vs RGB565
    big_endian: bool = False   # RGB565 byte order
    rotate: bool = False       # panel is portrait-native: pre-rotate 90 CW
    encode_base: int = 0       # C# encode-table base angle for user 0 deg
    encode_invert: bool = True

    @property
    def wire_size(self) -> tuple[int, int]:
        if self.rotate:
            return (self.height, self.width)
        return (self.width, self.height)

    @property
    def wire_angle(self) -> int:
        """Wire rotation at user orientation 0."""
        if not self.rotate:
            return 0
        return self.encode_base


# FBL -> profile.  Mirrors the reference FBL_PROFILES table for the
# resolutions reachable over the HID type-2 wire.  Unknown panels fall back to
# the documented 320x320 default rather than silently inventing a size.
FBL_PROFILES: dict[int, DeviceProfile] = {
    36:  DeviceProfile(240, 240),
    37:  DeviceProfile(240, 240),
    49:  DeviceProfile(320, 240, big_endian=True, rotate=True, encode_base=90),
    50:  DeviceProfile(320, 240, rotate=True, encode_base=90),
    51:  DeviceProfile(320, 240, rotate=True, encode_base=90),
    52:  DeviceProfile(320, 240, rotate=True, encode_base=90),
    53:  DeviceProfile(320, 240, rotate=True, encode_base=90),
    54:  DeviceProfile(360, 360, jpeg=True, encode_base=0),
    58:  DeviceProfile(320, 240, rotate=True, encode_base=90),
    60:  DeviceProfile(320, 176, rotate=True, encode_base=90),
    64:  DeviceProfile(640, 480, rotate=True, encode_base=90),
    72:  DeviceProfile(480, 480, encode_base=0),
    100: DeviceProfile(320, 320, big_endian=True, encode_base=0),
    101: DeviceProfile(320, 320, big_endian=True, encode_base=0),
    102: DeviceProfile(320, 320, big_endian=True, encode_base=0),
    114: DeviceProfile(1600, 720, jpeg=True, rotate=True, encode_base=90),
    128: DeviceProfile(1280, 480, jpeg=True, rotate=True, encode_base=90),
    129: DeviceProfile(480, 480, encode_base=0),
    192: DeviceProfile(1920, 462, jpeg=True, rotate=True, encode_base=90),
    224: DeviceProfile(854, 480, jpeg=True, rotate=True, encode_base=90),
}

DEFAULT_PROFILE = DeviceProfile(320, 320, big_endian=True, encode_base=0)

# PM -> FBL where the two differ.  Everything else is PM == FBL.
PM_TO_FBL: dict[int, int] = {
    5: 50, 7: 64, 9: 224, 10: 224, 11: 224, 12: 224, 13: 224, 14: 64,
    15: 224, 16: 224, 17: 224, 18: 224, 20: 224, 32: 100, 50: 50,
    63: 114, 64: 114, 65: 192, 66: 192, 68: 192, 69: 192,
}

# FBLs shared by several resolutions, disambiguated by PM.
FBL_192_BY_PM: dict[int, tuple[int, int]] = {
    1: (1920, 462), 65: (1920, 462), 66: (1920, 462),
    68: (1280, 480), 69: (1920, 440),
}
FBL_224_BY_PM: dict[int, tuple[int, int]] = {
    9: (854, 480), 10: (960, 540), 11: (854, 480), 12: (800, 480),
    13: (960, 320), 15: (640, 172), 16: (960, 540), 17: (960, 320),
    18: (960, 320), 20: (854, 480),
}
SHARED_FBLS: dict[int, tuple[dict[int, tuple[int, int]], tuple[int, int]]] = {
    192: (FBL_192_BY_PM, (1920, 462)),
    224: (FBL_224_BY_PM, (854, 480)),
}

NAMED_COLORS = {
    "red": (255, 0, 0), "green": (0, 255, 0), "blue": (0, 0, 255),
    "white": (255, 255, 255), "black": (0, 0, 0), "yellow": (255, 255, 0),
    "cyan": (0, 255, 255), "magenta": (255, 0, 255), "orange": (255, 165, 0),
    "purple": (128, 0, 128), "pink": (255, 192, 203), "grey": (128, 128, 128),
    "gray": (128, 128, 128), "warm": (255, 180, 107),
}


def pm_to_fbl(pm: int, sub: int = 0) -> int:
    if (pm, sub) == (1, 48):
        return 114
    if (pm, sub) == (1, 49):
        return 192
    return PM_TO_FBL.get(pm, pm)


def get_profile(fbl: int, pm: int = 0, sub: int = 0) -> DeviceProfile:
    profile = FBL_PROFILES.get(fbl, DEFAULT_PROFILE)
    shared = SHARED_FBLS.get(fbl)
    if shared is not None:
        by_pm, fallback = shared
        w, h = by_pm.get(pm, fallback)
        profile = DeviceProfile(
            w, h, profile.jpeg, profile.big_endian, profile.rotate,
            profile.encode_base, profile.encode_invert,
        )
    # A JPEG panel may take RGB565 on a particular SUB byte (CORE VISION SUB 4).
    if profile.jpeg and (profile.width, profile.height) == (360, 360) and sub == 4:
        profile = DeviceProfile(
            profile.width, profile.height, False, profile.big_endian,
            profile.rotate, profile.encode_base, profile.encode_invert,
        )
    # Resolve the encode base for the common RGB565 families.
    if not profile.jpeg and profile.rotate and profile.encode_base == 0:
        if (profile.width, profile.height) in ((320, 240), (320, 176)):
            profile = DeviceProfile(
                profile.width, profile.height, profile.jpeg,
                profile.big_endian, profile.rotate, 90, profile.encode_invert,
            )
    return profile


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

class LcdError(RuntimeError):
    pass


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def ceil512(n: int) -> int:
    return (n + CHUNK - 1) // CHUNK * CHUNK


def rotate_cw(im: "Image.Image", degrees: int) -> "Image.Image":
    """Rotate a PIL image clockwise by 0/90/180/270 degrees."""
    degrees %= 360
    if degrees == 0:
        return im
    if degrees == 90:
        return im.transpose(Image.ROTATE_270)
    if degrees == 180:
        return im.transpose(Image.ROTATE_180)
    if degrees == 270:
        return im.transpose(Image.ROTATE_90)
    raise ValueError(f"rotation must be a multiple of 90, got {degrees}")


def fit_image(src: "Image.Image", dst_w: int, dst_h: int, mode: str) -> "Image.Image":
    """Fit *src* into a dst_w x dst_h RGB canvas."""
    src = flatten(src)
    if mode == "stretch":
        return src.resize((dst_w, dst_h), LANCZOS)
    sw, sh = src.size
    if mode not in ("contain", "cover"):
        raise ValueError(f"unknown fit mode {mode!r}")
    scale_w = dst_w / sw
    scale_h = dst_h / sh
    scale = max(scale_w, scale_h) if mode == "cover" else min(scale_w, scale_h)
    nw = max(1, int(round(sw * scale)))
    nh = max(1, int(round(sh * scale)))
    resized = src.resize((nw, nh), LANCZOS)
    if mode == "contain":
        canvas = Image.new("RGB", (dst_w, dst_h), (0, 0, 0))
        canvas.paste(resized, ((dst_w - nw) // 2, (dst_h - nh) // 2))
        return canvas
    left = max(0, (nw - dst_w) // 2)
    top = max(0, (nh - dst_h) // 2)
    return resized.crop((left, top, left + dst_w, top + dst_h))


def flatten(im: "Image.Image") -> "Image.Image":
    """Return an RGB image, compositing alpha over black."""
    if im.mode == "RGB":
        return im
    if im.mode in ("RGBA", "LA") or (im.mode == "P" and "transparency" in im.info):
        im = im.convert("RGBA")
        bg = Image.new("RGB", im.size, (0, 0, 0))
        bg.paste(im, mask=im.split()[-1])
        return bg
    return im.convert("RGB")


def apply_brightness(im: "Image.Image", percent: int) -> "Image.Image":
    percent = max(0, min(100, percent))
    if percent == 100:
        return im
    if percent == 0:
        return Image.new("RGB", im.size, (0, 0, 0))
    table = [min(255, x * percent // 100) for x in range(256)]
    return im.point(table * 3 if im.mode == "RGB" else table, "RGB")


def rgb24_to_rgb565(data: bytes, brightness: int = 100,
                    big_endian: bool = False) -> bytes:
    """Convert packed RGB24 to packed RGB565 (little- or big-endian)."""
    n = len(data) // 3
    out = bytearray(n * 2)
    j = 0
    if brightness >= 100:
        for i in range(0, n * 3, 3):
            r = data[i]
            g = data[i + 1]
            b = data[i + 2]
            v = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
            if big_endian:
                out[j] = (v >> 8) & 0xFF
                out[j + 1] = v & 0xFF
            else:
                out[j] = v & 0xFF
                out[j + 1] = (v >> 8) & 0xFF
            j += 2
    else:
        table = [min(255, x * brightness // 100) for x in range(256)]
        for i in range(0, n * 3, 3):
            r = table[data[i]]
            g = table[data[i + 1]]
            b = table[data[i + 2]]
            v = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
            if big_endian:
                out[j] = (v >> 8) & 0xFF
                out[j + 1] = v & 0xFF
            else:
                out[j] = v & 0xFF
                out[j + 1] = (v >> 8) & 0xFF
            j += 2
    return bytes(out)


def parse_color(value: str) -> tuple[int, int, int]:
    value = value.strip().lower().lstrip("#")
    if value in NAMED_COLORS:
        return NAMED_COLORS[value]
    if len(value) == 3:
        value = "".join(c * 2 for c in value)
    if len(value) != 6:
        raise argparse.ArgumentTypeError(
            f"invalid colour {value!r}: use RRGGBB or a name"
        )
    try:
        return tuple(int(value[i:i + 2], 16) for i in (0, 2, 4))  # type: ignore
    except ValueError:
        raise argparse.ArgumentTypeError(f"invalid colour {value!r}")


# ---------------------------------------------------------------------------
# USB device
# ---------------------------------------------------------------------------

class LcdDevice:
    """Owns the USB handle and the type-2 wire protocol."""

    def __init__(self, vid: int = VENDOR_ID, pid: int = PRODUCT_ID,
                 verbose: bool = False, do_reset: bool = True):
        self.vid = vid
        self.pid = pid
        self.verbose = verbose
        self.do_reset = do_reset
        self.dev = None
        self.profile: Optional[DeviceProfile] = None
        self.pm = 0
        self.sub = 0
        self.fbl = 0
        self.serial = ""
        self.raw_reply = b""
        self._detached = False
        self._lock = threading.Lock()

    # -- discovery/open ---------------------------------------------------

    def _find(self):
        return usb.core.find(idVendor=self.vid, idProduct=self.pid)

    def open(self) -> "DeviceProfile":
        with self._lock:
            self._open_locked()
        return self.profile  # type: ignore[return-value]

    def _open_locked(self) -> None:
        dev = self._find()
        if dev is None:
            raise LcdError(
                f"device {self.vid:04x}:{self.pid:04x} not found "
                "(check `lsusb` and the cable)"
            )

        if self.do_reset:
            # EP 0x02 stays wedged after a failed transfer; a port reset is the
            # only reliable way to get it accepting packets again.
            try:
                dev.reset()
                if self.verbose:
                    log("USB port reset")
            except Exception as exc:  # reset can fail if the hub refuses
                if self.verbose:
                    log(f"reset skipped: {exc}")
            time.sleep(2.5)
            dev = self._find()
            if dev is None:
                raise LcdError("device disappeared after reset")

        self._detached = False
        try:
            active = dev.is_kernel_driver_active(0)
        except (NotImplementedError, usb.core.USBError):
            active = False
        if active:
            try:
                dev.detach_kernel_driver(0)
                self._detached = True
                if self.verbose:
                    log("detached kernel driver from interface 0")
            except usb.core.USBError as exc:
                if self.verbose:
                    log(f"kernel detach failed: {exc}")

        try:
            dev.set_configuration()
        except usb.core.USBError as exc:
            # "Resource busy" just means it is already configured.
            if exc.errno not in (16, 6):  # EBUSY, ENXIO
                if self.verbose:
                    log(f"set_configuration: {exc}")

        try:
            usb.util.claim_interface(dev, 0)
        except usb.core.USBError as exc:
            raise LcdError(f"cannot claim interface 0: {exc}") from exc

        self.dev = dev
        try:
            self._handshake()
        except Exception:
            self.close()
            raise

        if self.verbose:
            p = self.profile
            log(f"handshake: PM={self.pm} SUB={self.sub} FBL={self.fbl} "
                f"serial={self.serial!r}")
            log(f"profile: {p.width}x{p.height} "
                f"{'JPEG' if p.jpeg else 'RGB565'} "
                f"{'BE' if p.big_endian else 'LE'} "
                f"rotate={p.rotate} wire={p.wire_size[0]}x{p.wire_size[1]}")

    def _handshake(self) -> None:
        assert self.dev is not None
        time.sleep(0.05)
        n = self.dev.write(EP_OUT, HANDSHAKE, timeout=5000)
        if n < CHUNK:
            raise LcdError(f"handshake write returned {n}/{CHUNK}")
        time.sleep(0.2)
        try:
            resp = bytes(self.dev.read(EP_IN, 512, timeout=5000))
        except usb.core.USBError as exc:
            raise LcdError(f"handshake reply timed out: {exc}") from exc
        if len(resp) < 6 or resp[:4] != MAGIC:
            raise LcdError(f"bad handshake reply: {resp.hex(' ')}")
        # Reference validation: byte 12 is the command echo (0x01).
        if len(resp) > 12 and resp[12] not in (0x00, 0x01):
            if self.verbose:
                log(f"unexpected handshake echo byte 12: {resp[12]:#04x}")

        self.raw_reply = resp
        self.pm = resp[5]
        self.sub = resp[4]
        self.fbl = pm_to_fbl(self.pm, self.sub)
        self.profile = get_profile(self.fbl, self.pm, self.sub)
        self.serial = ""
        if len(resp) >= 36 and resp[16] == 0x10:
            raw = resp[20:36]
            # The field is ASCII text followed by binary counters/flags; keep
            # the leading printable run and ignore the rest.
            prefix = bytearray()
            for byte in raw:
                if 32 <= byte < 127:
                    prefix.append(byte)
                else:
                    break
            self.serial = prefix.decode("ascii") if prefix else raw.hex().upper()

    def close(self) -> None:
        with self._lock:
            dev = self.dev
            self.dev = None
            if dev is None:
                return
            try:
                usb.util.release_interface(dev, 0)
            except Exception:
                pass
            if self._detached:
                try:
                    dev.attach_kernel_driver(0)
                except Exception:
                    pass
            try:
                usb.util.dispose_resources(dev)
            except Exception:
                pass
            self._detached = False

    # -- frame encoding/sending ------------------------------------------

    def build_frame(self, payload: bytes) -> bytes:
        """Wrap encoded image bytes in the type-2 packet header."""
        assert self.profile is not None
        is_jpeg = payload[:2] == b"\xff\xd8"
        header = bytearray(MAGIC)
        header += struct.pack("<H", CMD_PICTURE)
        if is_jpeg:
            header += b"\x00\x00"
            header += struct.pack("<HH", self.profile.width,
                                  self.profile.height)
        else:
            header += b"\x01\x00"
            header += struct.pack("<HH", RGB565_HEADER_W, RGB565_HEADER_H)
        header += b"\x02\x00\x00\x00"
        header += struct.pack("<I", len(payload))
        raw = bytes(header) + payload
        return raw.ljust(ceil512(len(raw)), b"\x00")

    def render_image(self, im: "Image.Image", fit: str = "contain",
                     rotation: int = 0, brightness: int = 100) -> bytes:
        """Turn a user image into one complete wire packet."""
        assert self.profile is not None
        p = self.profile
        # The user canvas is landscape at 0/180 and portrait at 90/270; the
        # wire raster is always the panel's native portrait buffer.
        user_w, user_h = p.width, p.height
        if rotation % 180 == 90:
            user_w, user_h = user_h, user_w
        canvas = fit_image(im, user_w, user_h, fit)
        canvas = apply_brightness(canvas, brightness)
        angle = 0
        if p.rotate:
            signed = -rotation if p.encode_invert else rotation
            angle = (p.encode_base + signed) % 360
        wire = rotate_cw(canvas, angle)
        if wire.size != p.wire_size:
            # Should not happen, but never send a mis-sized raster.
            wire = fit_image(wire, p.wire_size[0], p.wire_size[1], "cover")
        return self.build_frame(self._encode(wire))

    def _encode(self, wire: "Image.Image") -> bytes:
        assert self.profile is not None
        p = self.profile
        if p.jpeg:
            buf = io.BytesIO()
            wire.convert("RGB").save(buf, format="JPEG", quality=90)
            payload = buf.getvalue()
            # Firmware ceiling: drop quality until under 450 kB.
            quality = 90
            while len(payload) >= 450_000 and quality > 20:
                quality -= 10
                buf = io.BytesIO()
                wire.convert("RGB").save(buf, format="JPEG", quality=quality)
                payload = buf.getvalue()
            return payload
        return rgb24_to_rgb565(wire.tobytes(), 100, p.big_endian)

    def send_packet(self, packet: bytes) -> None:
        """Send one complete frame as 512-byte interrupt writes."""
        assert self.dev is not None
        with self._lock:
            view = memoryview(packet)
            total = len(view)
            for offset in range(0, total, CHUNK):
                chunk = view[offset:offset + CHUNK]
                n = self.dev.write(EP_OUT, chunk, timeout=2000)
                if n < len(chunk):
                    raise LcdError(
                        f"short write {n}/{len(chunk)} at offset {offset}"
                    )
            if FRAME_DELAY:
                time.sleep(FRAME_DELAY)

    # -- streaming --------------------------------------------------------

    def stream(self, packets: Iterator[bytes], fps: float,
               duration: Optional[float], label: str = "frame") -> int:
        """Send packets continuously until exhausted or *duration* elapses.

        On a USB error the handle is reset and the handshake re-run; streaming
        then resumes.  The panel blanks whenever data stops, so there is no
        "send once" mode.
        """
        interval = 1.0 / fps if fps and fps > 0 else 0.0
        start = time.monotonic()
        sent = 0
        next_at = start
        interrupted = False

        try:
            while True:
                if duration is not None:
                    remaining = duration - (time.monotonic() - start)
                    if remaining <= 0:
                        break
                try:
                    packet = next(packets)
                except StopIteration:
                    break

                try:
                    if self.dev is None:
                        self.open()
                        log(f"connected: PM={self.pm} "
                            f"{self.profile.width}x{self.profile.height}")
                    self.send_packet(packet)
                    sent += 1
                except (usb.core.USBError, LcdError, OSError) as exc:
                    log(f"USB error: {exc} -- resetting and reconnecting")
                    self.close()
                    time.sleep(1.5)
                    continue

                if sent == 1 or sent % max(1, int(fps)) == 0:
                    if self.verbose:
                        log(f"streaming {label} #{sent}")

                if interval:
                    next_at += interval
                    delay = next_at - time.monotonic()
                    if delay > 0:
                        time.sleep(delay)
                    elif delay < -interval:
                        # We fell behind; don't try to catch up in a burst.
                        next_at = time.monotonic()
        except KeyboardInterrupt:
            interrupted = True
            log("stopped")
        finally:
            if not interrupted:
                log(f"streamed {sent} {label}(s)")
            # Close a generator source so video/ffmpeg subprocesses are torn
            # down promptly when --duration stops us early.
            closer = getattr(packets, "close", None)
            if closer is not None:
                try:
                    closer()
                except Exception:
                    pass
        return sent


# ---------------------------------------------------------------------------
# Frame sources
# ---------------------------------------------------------------------------

def static_source(device: LcdDevice, image: "Image.Image", fit: str,
                  rotation: int, brightness: int) -> Iterator[bytes]:
    packet = device.render_image(image, fit, rotation, brightness)
    while True:
        yield packet


def ffmpeg_filter(user_w: int, user_h: int, fit: str,
                  angle: int) -> str:
    if fit == "stretch":
        chain = f"scale={user_w}:{user_h}"
    elif fit == "contain":
        chain = (f"scale={user_w}:{user_h}:force_original_aspect_ratio=decrease,"
                 f"pad={user_w}:{user_h}:(ow-iw)/2:(oh-ih)/2:color=black")
    else:  # cover
        chain = (f"scale={user_w}:{user_h}:force_original_aspect_ratio=increase,"
                 f"crop={user_w}:{user_h}")
    angle %= 360
    if angle == 90:
        chain += ",transpose=1"
    elif angle == 180:
        chain += ",hflip,vflip"
    elif angle == 270:
        chain += ",transpose=2"
    chain += ",format=rgb24"
    return chain


def video_source(device: LcdDevice, path: str, fit: str, rotation: int,
                 brightness: int, fps: float, loop: bool) -> Iterator[bytes]:
    assert device.profile is not None
    p = device.profile
    user_w, user_h = p.width, p.height
    if rotation % 180 == 90:
        user_w, user_h = user_h, user_w
    angle = 0
    if p.rotate:
        signed = -rotation if p.encode_invert else rotation
        angle = (p.encode_base + signed) % 360
    wire_w, wire_h = p.wire_size
    frame_bytes = wire_w * wire_h * 3
    vf = ffmpeg_filter(user_w, user_h, fit, angle)

    while True:
        cmd = [
            "ffmpeg", "-nostdin", "-v", "error", "-i", path,
            "-an", "-sn", "-vf", vf,
            "-r", str(max(1, int(round(fps)))),
            "-f", "rawvideo", "-pix_fmt", "rgb24", "-",
        ]
        proc = subprocess.Popen(cmd, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, bufsize=0)
        try:
            while True:
                data = _read_exact(proc.stdout, frame_bytes)
                if data is None:
                    break
                payload = rgb24_to_rgb565(data, brightness, p.big_endian)
                yield device.build_frame(payload)
        finally:
            try:
                proc.stdout.close()
            except Exception:
                pass
            if proc.poll() is None:
                try:
                    proc.terminate()
                except Exception:
                    pass
            try:
                proc.wait(timeout=5)
            except Exception:
                try:
                    proc.kill()
                    proc.wait(timeout=5)
                except Exception:
                    pass
            err = b""
            try:
                if proc.stderr:
                    err = proc.stderr.read()
                    proc.stderr.close()
            except Exception:
                pass
            if err and device.verbose:
                text = err.decode("utf-8", "replace").strip()
                if text:
                    log(f"ffmpeg: {text}")
        if not loop:
            break


def _read_exact(stream, n: int) -> Optional[bytes]:
    buf = bytearray()
    while len(buf) < n:
        chunk = stream.read(n - len(buf))
        if not chunk:
            return None if not buf else None
        buf += chunk
    return bytes(buf)


# ---------------------------------------------------------------------------
# Built-in test card
# ---------------------------------------------------------------------------

def make_testcard(user_w: int, user_h: int) -> "Image.Image":
    im = Image.new("RGB", (user_w, user_h), (24, 24, 24))
    d = ImageDraw.Draw(im)
    bar = max(10, min(user_w, user_h) // 10)
    d.rectangle([0, 0, user_w - 1, bar - 1], fill=(255, 0, 0))              # TOP
    d.rectangle([user_w - bar, 0, user_w - 1, user_h - 1], fill=(0, 255, 0))  # RIGHT
    d.rectangle([0, user_h - bar, user_w - 1, user_h - 1], fill=(0, 0, 255))  # BOTTOM
    d.rectangle([0, 0, bar - 1, user_h - 1], fill=(255, 255, 0))            # LEFT
    cx = user_w // 2
    d.text((cx - 18, bar + 8), "TOP", fill=(255, 255, 255))
    d.text((cx - 28, user_h // 2 - 5), "CENTER", fill=(255, 255, 255))
    d.text((cx - 16, user_h - bar - 24), "BOT", fill=(255, 255, 255))
    d.polygon([(cx, user_h // 4), (cx - 24, user_h // 4 + 38),
               (cx + 24, user_h // 4 + 38)], fill=(255, 140, 0))
    d.rectangle([cx - 8, user_h // 4 + 38, cx + 8, user_h // 4 + 70],
                fill=(255, 140, 0))
    return im


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--vid", type=lambda s: int(s, 0), default=VENDOR_ID,
                        help="USB vendor ID (default 0x0416)")
    common.add_argument("--pid", type=lambda s: int(s, 0), default=PRODUCT_ID,
                        help="USB product ID (default 0x5302)")
    common.add_argument("-v", "--verbose", action="store_true",
                        help="log handshake details and per-frame progress")
    common.add_argument("--no-reset", action="store_true",
                        help="skip the USB port reset before handshaking")
    common.add_argument("--fps", type=float, default=20.0,
                        help="target frames per second (default 20)")
    common.add_argument("--duration", type=float, default=None,
                        help="stop after N seconds (default: until stopped)")
    common.add_argument("--rotation", type=int, default=0,
                        choices=[0, 90, 180, 270],
                        help="display rotation in degrees clockwise (default 0)")
    common.add_argument("--brightness", type=int, default=100,
                        help="brightness percent 0-100 (default 100)")

    p = argparse.ArgumentParser(
        description="Linux port of the TRCC/USBLCDNEW LCD control software "
                    "for the Winbond USB Display (0416:5302).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    sub = p.add_subparsers(dest="command", required=True)

    s = sub.add_parser("info", parents=[common], help="handshake and print device details")
    s.add_argument("--json", action="store_true", help="print JSON")

    s = sub.add_parser("testcard", parents=[common],
                       help="stream the built-in orientation test card")
    s.add_argument("--fit", default="stretch",
                   choices=["stretch", "contain", "cover"])

    s = sub.add_parser("color", parents=[common],
                       help="stream a solid colour (RRGGBB or name)")
    s.add_argument("colour", type=parse_color)

    s = sub.add_parser("image", parents=[common],
                       help="stream a still image continuously")
    s.add_argument("file")
    s.add_argument("--fit", default="contain",
                   choices=["stretch", "contain", "cover"])

    s = sub.add_parser("video", parents=[common],
                       help="play a video file (via ffmpeg)")
    s.add_argument("file")
    s.add_argument("--fit", default="cover",
                   choices=["stretch", "contain", "cover"])
    s.add_argument("--loop", action="store_true", help="loop the video")

    return p


def cmd_info(args) -> int:
    dev = LcdDevice(args.vid, args.pid, args.verbose, not args.no_reset)
    profile = dev.open()
    try:
        info = {
            "vid": f"{args.vid:04x}",
            "pid": f"{args.pid:04x}",
            "pm": dev.pm,
            "sub": dev.sub,
            "fbl": dev.fbl,
            "serial": dev.serial,
            "resolution": f"{profile.width}x{profile.height}",
            "wire_raster": f"{profile.wire_size[0]}x{profile.wire_size[1]}",
            "encoding": "JPEG" if profile.jpeg else "RGB565",
            "byte_order": "big-endian" if profile.big_endian else "little-endian",
            "portrait_native": profile.rotate,
            "raw_reply": dev.raw_reply.hex(" "),
        }
        if args.json:
            import json
            print(json.dumps(info, indent=2))
        else:
            print(f"Device:      {args.vid:04x}:{args.pid:04x} "
                  f"(bcdDevice reported in `lsusb -v`)")
            print(f"PM / SUB:    {dev.pm} / {dev.sub}   FBL: {dev.fbl}")
            print(f"Serial:      {dev.serial or '(none)'}")
            print(f"Resolution:  {profile.width}x{profile.height}")
            print(f"Wire raster: {profile.wire_size[0]}x{profile.wire_size[1]}")
            print(f"Encoding:    {'JPEG' if profile.jpeg else 'RGB565'} "
                  f"({'big' if profile.big_endian else 'little'}-endian)")
            print(f"Rotate:      {profile.rotate} "
                  f"(encode base {profile.encode_base} deg)")
            if args.verbose:
                print(f"Reply:       {dev.raw_reply.hex(' ')}")
    finally:
        dev.close()
    return 0


def cmd_testcard(args) -> int:
    dev = LcdDevice(args.vid, args.pid, args.verbose, not args.no_reset)
    dev.open()
    try:
        p = dev.profile
        user_w, user_h = p.width, p.height
        if args.rotation % 180 == 90:
            user_w, user_h = user_h, user_w
        image = make_testcard(user_w, user_h)
        source = static_source(dev, image, args.fit, args.rotation,
                               args.brightness)
        dev.stream(source, args.fps, args.duration, label="test card")
    finally:
        dev.close()
    return 0


def cmd_color(args) -> int:
    dev = LcdDevice(args.vid, args.pid, args.verbose, not args.no_reset)
    dev.open()
    try:
        p = dev.profile
        user_w, user_h = p.width, p.height
        if args.rotation % 180 == 90:
            user_w, user_h = user_h, user_w
        image = Image.new("RGB", (user_w, user_h), args.colour)
        source = static_source(dev, image, "stretch", args.rotation,
                               args.brightness)
        dev.stream(source, args.fps, args.duration, label="frame")
    finally:
        dev.close()
    return 0


def cmd_image(args) -> int:
    try:
        image = Image.open(args.file)
    except Exception as exc:
        sys.exit(f"cannot open image {args.file!r}: {exc}")
    dev = LcdDevice(args.vid, args.pid, args.verbose, not args.no_reset)
    dev.open()
    try:
        source = static_source(dev, image, args.fit, args.rotation,
                               args.brightness)
        dev.stream(source, args.fps, args.duration, label="frame")
    finally:
        dev.close()
    return 0


def cmd_video(args) -> int:
    if not os.path.exists(args.file):
        sys.exit(f"no such file: {args.file}")
    dev = LcdDevice(args.vid, args.pid, args.verbose, not args.no_reset)
    dev.open()
    try:
        source = video_source(dev, args.file, args.fit, args.rotation,
                              args.brightness, args.fps, args.loop)
        dev.stream(source, args.fps, args.duration, label="frame")
    finally:
        dev.close()
    return 0


def main(argv=None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    # Clean Ctrl-C: the finally blocks release the interface.
    def _sigint(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGINT, _sigint)

    handlers = {
        "info": cmd_info,
        "testcard": cmd_testcard,
        "color": cmd_color,
        "image": cmd_image,
        "video": cmd_video,
    }
    try:
        return handlers[args.command](args)
    except LcdError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
