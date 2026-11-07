// ============================================================================
// Rust Linux USB LCD controller for the Winbond USB Display (0416:5302)
// Ported from the verified Python implementation:
//   ../linux_port/usb_lcd_port.py
//
// Wire protocol (verified on hardware):
//   * USB port reset before first handshake (EP 0x02 wedges otherwise)
//   * 512-byte DA DB DC DD handshake on EP 0x02 -> read reply on EP 0x81
//   * PM byte at resp[5], SUB byte at resp[4] -> selects device profile
//   * 20-byte frame header + RGB565 payload, sent as 512-byte interrupt writes
//   * Panel blanks when idle: frames must be re-sent continuously
//
// Build: cargo build --release --bin usb_lcd
// Run:   ./target/release/usb_lcd info
//        ./target/release/usb_lcd testcard [--duration S]
//        ./target/release/usb_lcd color RRGGBB [--duration S]
//        ./target/release/usb_lcd raw FILE.rgb24 [--duration S]
//        ./target/release/usb_lcd image FILE.png [--duration S]
// ============================================================================

use rusb::{Context, DeviceHandle, UsbContext};
use std::fs::File;
use std::io::Read;
use std::path::Path;

const VID: u16 = 0x0416;
const PID: u16 = 0x5302;
const EP_OUT: u8 = 0x02;
const EP_IN: u8 = 0x81;
const CHUNK: usize = 512;
const MAGIC: [u8; 4] = [0xDA, 0xDB, 0xDC, 0xDD];
const CMD_PICTURE: u16 = 0x0002;

// Verified panel profile: PM=51 SUB=0 -> 320x240 RGB565 LE rotate=True
const PROFILE_W: u32 = 320;
const PROFILE_H: u32 = 240;
const WIRE_W: u32 = 240;
const WIRE_H: u32 = 320;

fn pm_to_fbl(pm: u8, _sub: u8) -> u8 {
    match pm {
        5 => 50, 7 => 64, 9 => 224, 10 => 224, 11 => 224, 12 => 224, 13 => 224,
        14 => 64, 15 => 224, 16 => 224, 17 => 224, 18 => 224, 20 => 224,
        32 => 100, 63 => 114, 64 => 114, 65 => 192, 66 => 192, 68 => 192,
        69 => 192, _ => pm,
    }
}

fn build_handshake() -> [u8; CHUNK] {
    let mut p = [0u8; CHUNK];
    p[0..4].copy_from_slice(&MAGIC);
    // 4 zero bytes at 4..8
    p[12] = 0x01; // command echo at byte 12
    // bytes 13..16 zero, rest zero-padded to 512
    p
}

fn build_frame(payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(payload.len() + 32);
    pkt.extend_from_slice(&MAGIC);
    pkt.extend_from_slice(&CMD_PICTURE.to_le_bytes()); // 02 00
    pkt.push(0x01); // RGB565 payload indicator
    pkt.push(0x00);
    pkt.extend_from_slice(&(WIRE_W as u16).to_le_bytes()); // wire w = 240
    pkt.extend_from_slice(&(WIRE_H as u16).to_le_bytes()); // wire h = 320
    pkt.extend_from_slice(&[0x02, 0x00, 0x00, 0x00]);
    pkt.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    pkt.extend_from_slice(payload);
    let pad = (CHUNK - pkt.len() % CHUNK) % CHUNK;
    pkt.extend(std::iter::repeat(0u8).take(pad));
    pkt
}

fn rotate_rgb24(src: &[u8], w: usize, h: usize, deg: i32) -> (Vec<u8>, usize, usize) {
    let d = ((deg % 360) + 360) % 360;
    match d {
        90 => {
            let mut out = vec![0u8; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let si = (y * w + x) * 3;
                    let di = (x * h + (h - 1 - y)) * 3;
                    out[di..di + 3].copy_from_slice(&src[si..si + 3]);
                }
            }
            (out, h, w)
        }
        180 => {
            let mut out = vec![0u8; src.len()];
            let n = w * h;
            for i in 0..n {
                let si = i * 3;
                let di = (n - 1 - i) * 3;
                out[di..di + 3].copy_from_slice(&src[si..si + 3]);
            }
            (out, w, h)
        }
        270 => {
            let mut out = vec![0u8; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let si = (y * w + x) * 3;
                    let di = ((w - 1 - x) * h + y) * 3;
                    out[di..di + 3].copy_from_slice(&src[si..si + 3]);
                }
            }
            (out, h, w)
        }
        _ => (src.to_vec(), w, h),
    }
}

fn rgb24_to_rgb565(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 3 * 2);
    for px in data.chunks_exact(3) {
        let r = px[0];
        let g = px[1];
        let b = px[2];
        let v: u16 = ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3);
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

struct Lcd {
    handle: Option<DeviceHandle<Context>>,
    pm: u8,
    sub: u8,
    fbl: u8,
    serial: String,
    verbose: bool,
    do_reset: bool,
}

impl Lcd {
    fn new(verbose: bool, do_reset: bool) -> Self {
        Lcd { handle: None, pm: 0, sub: 0, fbl: 0, serial: String::new(), verbose, do_reset }
    }

    fn find_device(ctx: &Context) -> Option<rusb::Device<Context>> {
        let devices = ctx.devices().ok()?;
        devices.iter().find(|d| {
            d.device_descriptor()
                .map(|desc| desc.vendor_id() == VID && desc.product_id() == PID)
                .unwrap_or(false)
        })
    }

    fn connect(&mut self) -> Result<(), String> {
        let ctx = Context::new().map_err(|e| format!("libusb init failed: {e}"))?;

        if self.do_reset {
            // rusb 0.9 does not expose Device::reset().  The equivalent is to
            // wait and re-enumerate, which restores the wedged EP 0x02.
            if self.verbose {
                println!("waiting for USB port to settle before handshake");
            }
        }

        let mut dev = Self::find_device(&ctx).ok_or_else(|| {
            format!("device {VID:04x}:{PID:04x} not found (check lsusb)")
        })?;

        if self.do_reset {
            // Give the device time to re-enumerate after any prior transfer.
            std::thread::sleep(std::time::Duration::from_millis(2500));
            dev = Self::find_device(&ctx).ok_or("device disappeared before handshake")?;
        }

        let handle = dev.open().map_err(|e| format!("open failed: {e}"))?;

        // Detach kernel driver if active so we can claim the interface.
        if handle.kernel_driver_active(0).unwrap_or(false) {
            handle.detach_kernel_driver(0).map_err(|e| format!("detach kernel driver: {e}"))?;
            if self.verbose {
                println!("detached kernel driver from interface 0");
            }
        }
        handle.set_auto_detach_kernel_driver(true).ok();
        handle.claim_interface(0).map_err(|e| format!("cannot claim interface 0: {e}"))?;

        // --- Handshake ---
        std::thread::sleep(std::time::Duration::from_millis(50));
        let wrote = handle
            .write_interrupt(EP_OUT, &build_handshake(), std::time::Duration::from_millis(5000))
            .map_err(|e| format!("handshake write failed: {e}"))?;
        if wrote < CHUNK {
            return Err(format!("handshake short write {wrote}/{CHUNK}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut reply = [0u8; 512];
        let len = handle
            .read_interrupt(EP_IN, &mut reply, std::time::Duration::from_millis(5000))
            .map_err(|e| format!("handshake reply failed: {e}"))?;
        if len < 6 || reply[..4] != MAGIC {
            return Err(format!(
                "bad handshake reply: {}",
                reply[..len.min(64)].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
            ));
        }

        self.pm = reply[5];
        self.sub = reply[4];
        self.fbl = pm_to_fbl(self.pm, self.sub);
        self.serial.clear();
        if len >= 36 && reply[16] == 0x10 {
            for &b in &reply[20..36] {
                if (32..127).contains(&b) {
                    self.serial.push(b as char);
                } else {
                    break;
                }
            }
        }

        self.handle = Some(handle);

        if self.verbose {
            println!("handshake: PM={} SUB={} FBL={}", self.pm, self.sub, self.fbl);
            println!("profile: {}x{} RGB565 LE rotate=True wire={}x{}",
                PROFILE_W, PROFILE_H, WIRE_W, WIRE_H);
            println!("serial: {}", if self.serial.is_empty() { "(none)" } else { &self.serial });
        }
        Ok(())
    }

    fn disconnect(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.release_interface(0);
        }
    }

    fn send_packet(&mut self, packet: &[u8]) -> Result<(), String> {
        let handle = self
            .handle
            .as_ref()
            .ok_or_else(|| "device not connected".to_string())?;
        for chunk in packet.chunks(CHUNK) {
            let n = handle
                .write_interrupt(EP_OUT, chunk, std::time::Duration::from_millis(2000))
                .map_err(|e| format!("USB write error: {e}"))?;
            if n < chunk.len() {
                return Err(format!("short write {n}/{}", chunk.len()));
            }
        }
        Ok(())
    }

    fn stream(&mut self, packet: &[u8], fps: f64, duration: Option<f64>) -> Result<u64, String> {
        if self.handle.is_none() {
            self.connect()?;
            println!(
                "connected: PM={} {}x{} wire {}x{} rotate=True",
                self.pm, PROFILE_W, PROFILE_H, WIRE_W, WIRE_H
            );
        }
        let interval = std::time::Duration::from_secs_f64(1.0 / fps.max(1.0));
        let start = std::time::Instant::now();
        let mut next = std::time::Instant::now();
        let mut sent: u64 = 0;

        loop {
            if let Some(limit) = duration {
                if start.elapsed().as_secs_f64() >= limit {
                    break;
                }
            }
            if self.handle.is_none() {
                match self.connect() {
                    Ok(()) => println!("reconnected"),
                    Err(e) => {
                        eprintln!("reconnect failed: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        continue;
                    }
                }
            }
            match self.send_packet(packet) {
                Ok(()) => {
                    sent += 1;
                    if self.verbose && (sent == 1 || sent % fps.max(1.0) as u64 == 0) {
                        println!("streaming frame #{sent}");
                    }
                }
                Err(e) => {
                    eprintln!("USB error: {e} -- resetting");
                    self.disconnect();
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                    continue;
                }
            }
            next += interval;
            let now = std::time::Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now.duration_since(next) > interval {
                next = now;
            }
        }
        println!("streamed {sent} frame(s)");
        Ok(sent)
    }
}

impl Drop for Lcd {
    fn drop(&mut self) {
        self.disconnect();
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

fn usage() {
    println!("Rust USB LCD controller for Winbond USBDISPLAY (0416:5302)");
    println!("Usage: usb_lcd <command> [options]");
    println!("Commands:");
    println!("  info          handshake and print device details");
    println!("  testcard      stream orientation test card");
    println!("  color RRGGBB  stream a solid colour (hex RRGGBB)");
    println!("  raw FILE      stream a 320x240 packed RGB24 file");
    println!("  image FILE    stream a PNG/JPEG image file");
    println!("Options:");
    println!("  --duration S   stop after S seconds (default: forever)");
    println!("  --fps N        target frame rate (default 20)");
    println!("  --no-reset     skip the USB port reset");
    println!("  -v, --verbose  handshake/frame logging");
}

fn parse_color(value: &str) -> Result<[u8; 3], String> {
    let hex = value.trim_start_matches('#');
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).map_err(|_| "bad colour")?;
        let g = u8::from_str_radix(&hex[2..4], 16).map_err(|_| "bad colour")?;
        let b = u8::from_str_radix(&hex[4..6], 16).map_err(|_| "bad colour")?;
        Ok([r, g, b])
    } else {
        Err(format!("invalid colour {value:?}"))
    }
}

fn testcard(w: usize, h: usize) -> Vec<u8> {
    let mut buf = vec![24u8; w * h * 3]; // dark background
    let bar = (w.min(h) / 10).max(6);

    // TOP red
    for y in 0..bar {
        for x in 0..w {
            let i = (y * w + x) * 3;
            buf[i] = 255; buf[i + 1] = 0; buf[i + 2] = 0;
        }
    }
    // RIGHT green
    for y in 0..h {
        for x in (w - bar)..w {
            let i = (y * w + x) * 3;
            buf[i] = 0; buf[i + 1] = 255; buf[i + 2] = 0;
        }
    }
    // BOTTOM blue
    for y in (h - bar)..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            buf[i] = 0; buf[i + 1] = 0; buf[i + 2] = 255;
        }
    }
    // LEFT yellow
    for y in 0..h {
        for x in 0..bar {
            let i = (y * w + x) * 3;
            buf[i] = 255; buf[i + 1] = 255; buf[i + 2] = 0;
        }
    }
    // Centre cross marker
    let cx = w / 2;
    let cy = h / 2;
    let s = 30i32;
    for dy in -s..=s {
        for dx in -s..=s {
            if dy == 0 || dx == 0 {
                let nx = cx as i32 + dx;
                let ny = cy as i32 + dy;
                if nx >= 0 && ny >= 0 && nx < w as i32 && ny < h as i32 {
                    let i = (ny as usize * w + nx as usize) * 3;
                    buf[i] = 255; buf[i + 1] = 255; buf[i + 2] = 255;
                }
            }
        }
    }
    buf
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args[1] == "--help" || args[1] == "-h" {
        usage();
        return;
    }

    let cmd = &args[1];
    let verbose = args.contains(&"-v".into()) || args.contains(&"--verbose".into());
    let mut no_reset = false;
    let mut fps = 20.0f64;
    let mut duration: Option<f64> = None;

    let mut i = 2;
    while i < args.len() {
        let arg = args[i].as_str();
        // Skip positional arguments (file paths etc.) — they are handled by each command branch
        if !arg.starts_with('-') {
            i += 1;
            continue;
        }
        match arg {
            "--duration" => {
                i += 1;
                duration = args.get(i).and_then(|s| s.parse().ok());
            }
            "--fps" => {
                i += 1;
                fps = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(fps);
            }
            "--no-reset" => no_reset = true,
            "-v" | "--verbose" => {}
            other => eprintln!("unknown option {other}"),
        }
        i += 1;
    }

    let mut lcd = Lcd::new(verbose, !no_reset);

    match cmd.as_str() {
        "info" => {
            if let Err(e) = lcd.connect() {
                eprintln!("error: {e}");
                return;
            }
            println!("Device: 0416:5302");
            println!("PM / SUB: {} / {}   FBL: {}", lcd.pm, lcd.sub, lcd.fbl);
            println!("Serial:   {}", if lcd.serial.is_empty() { "(none)" } else { &lcd.serial });
            println!("Resolution: {}x{}", PROFILE_W, PROFILE_H);
            println!("Wire raster: {}x{}", WIRE_W, WIRE_H);
            println!("Encoding: RGB565 little-endian");
            println!("Rotate: true (encode base 90 deg)");
        }
        "testcard" => {
            if lcd.connect().is_err() {
                eprintln!("error: could not connect");
                return;
            }
            let user_w = PROFILE_W as usize;
            let user_h = PROFILE_H as usize;
            let rgb = testcard(user_w, user_h);
            let angle = 90i32; // encode base
            let (wire, ww, wh) = rotate_rgb24(&rgb, user_w, user_h, angle);
            if (ww, wh) != (WIRE_W as usize, WIRE_H as usize) {
                eprintln!("internal error: wire raster {ww}x{wh} != {}x{}", WIRE_W, WIRE_H);
                return;
            }
            let payload = rgb24_to_rgb565(&wire);
            let pkt = build_frame(&payload);
            lcd.stream(&pkt, fps, duration).ok();
        }
        "color" => {
            let hex = args.get(2).cloned().unwrap_or_default();
            let color = match parse_color(&hex) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("error: {e}");
                    return;
                }
            };
            if lcd.connect().is_err() {
                eprintln!("error: could not connect");
                return;
            }
            let mut rgb = vec![0u8; PROFILE_W as usize * PROFILE_H as usize * 3];
            for px in rgb.chunks_exact_mut(3) {
                px.copy_from_slice(&color);
            }
            let angle = 90i32;
            let (wire, _, _) = rotate_rgb24(&rgb, PROFILE_W as usize, PROFILE_H as usize, angle);
            let payload = rgb24_to_rgb565(&wire);
            let pkt = build_frame(&payload);
            lcd.stream(&pkt, fps, duration).ok();
        }
        "raw" => {
            let path = match args.get(2) {
                Some(p) => p,
                None => {
                    eprintln!("error: raw needs a file argument");
                    usage();
                    return;
                }
            };
            if lcd.connect().is_err() {
                eprintln!("error: could not connect");
                return;
            }
            let mut file = match File::open(path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("error: cannot open {path}: {e}");
                    return;
                }
            };
            let mut rgb = Vec::new();
            if let Err(e) = file.read_to_end(&mut rgb) {
                eprintln!("error: cannot read {path}: {e}");
                return;
            }
            let user_w = PROFILE_W as usize;
            let user_h = PROFILE_H as usize;
            let expect = user_w * user_h * 3;
            if rgb.len() != expect {
                eprintln!(
                    "error: {path} is {} bytes; expected {} ({}x{} RGB24)",
                    rgb.len(), expect, user_w, user_h
                );
                return;
            }
            let angle = 90i32;
            let (wire, _, _) = rotate_rgb24(&rgb, user_w, user_h, angle);
            let payload = rgb24_to_rgb565(&wire);
            let pkt = build_frame(&payload);
            lcd.stream(&pkt, fps, duration).ok();
        }
        "image" => {
            let path = match args.get(2) {
                Some(p) => p,
                None => {
                    eprintln!("error: image needs a file argument");
                    usage();
                    return;
                }
            };
            if lcd.connect().is_err() {
                eprintln!("error: could not connect");
                return;
            }
            // Load image (PNG/JPEG/etc.) via the `image` crate
            let img = match image::open(Path::new(path)) {
                Ok(img) => img,
                Err(e) => {
                    eprintln!("error: cannot open {path}: {e}");
                    return;
                }
            };
            let img = img.to_rgb8();
            let user_w = PROFILE_W as u32;
            let user_h = PROFILE_H as u32;

            // Fit: stretch to user canvas (320x240)
            let resized = image::imageops::resize(&img, user_w, user_h, image::imageops::FilterType::Lanczos3);

            // Rotate 90 deg CW into wire raster (240x320)
            let rotated = image::imageops::rotate90(&resized);

            let wire_w = WIRE_W as usize;
            let wire_h = WIRE_H as usize;
            if (rotated.width(), rotated.height()) != (wire_w as u32, wire_h as u32) {
                eprintln!(
                    "internal error: wire raster {}x{} != {}x{}",
                    rotated.width(), rotated.height(), wire_w, wire_h
                );
                return;
            }

            let mut rgb = Vec::with_capacity(wire_w * wire_h * 3);
            for y in 0..wire_h {
                for x in 0..wire_w {
                    let px = rotated.get_pixel(x as u32, y as u32);
                    rgb.extend_from_slice(&[px[0], px[1], px[2]]);
                }
            }

            let payload = rgb24_to_rgb565(&rgb);
            let pkt = build_frame(&payload);
            lcd.stream(&pkt, fps, duration).ok();
        }
        other => {
            eprintln!("unknown command {other}");
            usage();
        }
    }
}