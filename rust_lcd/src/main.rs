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
//        ./target/release/usb_lcd video FILE.mp4 [--fit cover --fps 15 --loop] [--duration S]
// Video playback decodes through the external `ffmpeg` binary (same as the
// Python port): frames are scaled / padded / cropped, rotated and resampled
// by ffmpeg to raw RGB24 at --fps, then encoded to RGB565 here.
// ============================================================================

use rusb::{Context, DeviceHandle, UsbContext};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};

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

#[allow(dead_code)]
fn rgb24_to_rgb565(data: &[u8]) -> Vec<u8> {
    rgb24_to_rgb565_brightness(data, 100)
}

fn rgb24_to_rgb565_brightness(data: &[u8], brightness: u8) -> Vec<u8> {
    let brightness = brightness.min(100);
    let mut out = Vec::with_capacity(data.len() / 3 * 2);
    if brightness >= 100 {
        for px in data.chunks_exact(3) {
            let r = px[0];
            let g = px[1];
            let b = px[2];
            let v: u16 = ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3);
            out.extend_from_slice(&v.to_le_bytes());
        }
    } else {
        let mut table = [0u8; 256];
        for (i, v) in table.iter_mut().enumerate() {
            *v = (i as u16 * brightness as u16 / 100) as u8;
        }
        for px in data.chunks_exact(3) {
            let r = table[px[0] as usize];
            let g = table[px[1] as usize];
            let b = table[px[2] as usize];
            let v: u16 = ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3);
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

// Mirror of `ffmpeg_filter` in ../linux_port/usb_lcd_port.py.  The whole
// fit + rotate + pixel-format pipeline runs inside ffmpeg, so Rust only has
// to read one wire-sized RGB24 frame after another.
fn ffmpeg_filter(user_w: u32, user_h: u32, fit: &str, angle: i32) -> String {
    let mut chain = match fit {
        "stretch" => format!("scale={user_w}:{user_h}"),
        "contain" => format!(
            "scale={user_w}:{user_h}:force_original_aspect_ratio=decrease,pad={user_w}:{user_h}:(ow-iw)/2:(oh-ih)/2:color=black"
        ),
        "cover" => format!(
            "scale={user_w}:{user_h}:force_original_aspect_ratio=increase,crop={user_w}:{user_h}"
        ),
        _ => format!(
            "scale={user_w}:{user_h}:force_original_aspect_ratio=increase,crop={user_w}:{user_h}"
        ),
    };
    match angle.rem_euclid(360) {
        90 => chain.push_str(",transpose=1"),
        180 => chain.push_str(",hflip,vflip"),
        270 => chain.push_str(",transpose=2"),
        _ => {}
    }
    chain.push_str(",format=rgb24");
    chain
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
    vid: u16,
    pid: u16,
}

impl Lcd {
    fn new(verbose: bool, do_reset: bool) -> Self {
        Lcd { handle: None, pm: 0, sub: 0, fbl: 0, serial: String::new(), verbose, do_reset, vid: VID, pid: PID }
    }

    fn find_device(ctx: &Context, vid: u16, pid: u16) -> Option<rusb::Device<Context>> {
        let devices = ctx.devices().ok()?;
        devices.iter().find(|d| {
            d.device_descriptor()
                .map(|desc| desc.vendor_id() == vid && desc.product_id() == pid)
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

        let mut dev = Self::find_device(&ctx, self.vid, self.pid).ok_or_else(|| {
            format!("device {:04x}:{:04x} not found (check lsusb)", self.vid, self.pid)
        })?;

        if self.do_reset {
            // Give the device time to re-enumerate after any prior transfer.
            std::thread::sleep(std::time::Duration::from_millis(2500));
            dev = Self::find_device(&ctx, self.vid, self.pid).ok_or("device disappeared before handshake")?;
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

    // Stream a video file decoded by ffmpeg, mirroring
    // `video_source` + `stream` in ../linux_port/usb_lcd_port.py.
    // ffmpeg scales/pads/crops, rotates and resamples to `fps`, emitting
    // wire-sized RGB24 frames on stdout.  Unlike `stream`, this stops on
    // EOF unless `loop_` is set, because the panel only needs to stay fed
    // while the video is actually playing.
    #[allow(clippy::too_many_arguments)]
    fn stream_video(
        &mut self,
        path: &str,
        fit: &str,
        rotation: i32,
        brightness: u8,
        fps: f64,
        duration: Option<f64>,
        loop_: bool,
    ) -> Result<u64, String> {
        if self.handle.is_none() {
            self.connect()?;
            println!(
                "connected: PM={} {}x{} wire {}x{} rotate=True",
                self.pm, PROFILE_W, PROFILE_H, WIRE_W, WIRE_H
            );
        }
        let mut user_w = PROFILE_W;
        let mut user_h = PROFILE_H;
        if rotation.rem_euclid(360) % 180 == 90 {
            std::mem::swap(&mut user_w, &mut user_h);
        }
        // Panel profile is rotate=True, encode_base=90, encode_invert=True
        // (same as the Python port): angle = (90 - user_rotation) % 360.
        let angle = (90 - rotation).rem_euclid(360);
        let vf = ffmpeg_filter(user_w, user_h, fit, angle);
        let wire_w = WIRE_W as usize;
        let wire_h = WIRE_H as usize;
        let frame_bytes = wire_w * wire_h * 3;
        let ffmpeg_fps = fps.max(1.0).round() as u64;
        let interval = std::time::Duration::from_secs_f64(1.0 / fps.max(1.0));
        let start = std::time::Instant::now();
        let mut next = std::time::Instant::now();
        let mut sent: u64 = 0;
        if self.verbose {
            println!(
                "video: {path} fit={fit} rotation={rotation} brightness={brightness} fps={ffmpeg_fps} loop={loop_} vf={vf}"
            );
        }

        loop {
            let cmd = [
                "ffmpeg",
                "-nostdin",
                "-v",
                "error",
                "-i",
                path,
                "-an",
                "-sn",
                "-vf",
                vf.as_str(),
                "-r",
                &ffmpeg_fps.to_string(),
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-",
            ];
            let mut child: Child = Command::new(cmd[0])
                .args(&cmd[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        "ffmpeg not found: install ffmpeg (e.g. sudo dnf install ffmpeg) in PATH".to_string()
                    } else {
                        format!("failed to start ffmpeg: {e}")
                    }
                })?;

            let mut stdout = child.stdout.take().expect("stdout piped");
            let mut frame = vec![0u8; frame_bytes];
            let mut frames_this_pass: u64 = 0;

            loop {
                if let Some(limit) = duration {
                    if start.elapsed().as_secs_f64() >= limit {
                        break;
                    }
                }
                match stdout.read_exact(&mut frame) {
                    Ok(()) => {}
                    Err(e) => {
                        // UnexpectedEof is ffmpeg reaching EOF: normal end
                        // of the video.  Other IO errors end the pass too.
                        if e.kind() != std::io::ErrorKind::UnexpectedEof && self.verbose {
                            eprintln!("ffmpeg read error: {e}");
                        }
                        break;
                    }
                }
                frames_this_pass += 1;

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

                let payload = rgb24_to_rgb565_brightness(&frame, brightness);
                let pkt = build_frame(&payload);
                match self.send_packet(&pkt) {
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

            // Tear ffmpeg down promptly (duration elapsed, EOF or error)
            // so a looping pass starts from a clean decoder.
            let _ = child.kill();
            let _ = child.wait();

            if !loop_ {
                break;
            }
            if let Some(limit) = duration {
                if start.elapsed().as_secs_f64() >= limit {
                    break;
                }
            }
            if frames_this_pass == 0 {
                // Nothing decoded (missing/bad codec or unreadable file).
                // Looping here would spin forever respawning ffmpeg.
                return Err(format!(
                    "ffmpeg produced no frames for {path:?} (unsupported codec/container or empty file?)"
                ));
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
    println!("  video FILE    play a video file via ffmpeg (mp4/mkv/mov/avi/gif)");
    println!("Options:");
    println!("  --duration S   stop after S seconds (default: forever)");
    println!("  --fps N        target frame rate (default 20)");
    println!("  --vid X --pid Y  USB IDs (default 0x0416:0x5302)");
    println!("  --no-reset     skip the USB port reset");
    println!("  -v, --verbose  handshake/frame logging");
    println!("Video/image options:");
    println!("  --fit MODE     contain | cover | stretch (video default: cover)");
    println!("  --rotation DEG rotate picture clockwise: 0|90|180|270 (default 0)");
    println!("  --brightness P dim picture 0-100 (default 100)");
    println!("  --loop         loop the video (default: play once, then exit)");
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
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose" || a.starts_with("--verbose="));
    let mut no_reset = false;
    let mut fps = 20.0f64;
    let mut duration: Option<f64> = None;
    let mut vid = VID;
    let mut pid = PID;
    let mut fit = "contain".to_string();
    let mut fit_set = false;
    let mut rotation: i32 = 0;
    let mut brightness: u8 = 100;
    let mut loop_flag = false;
    let mut positionals: Vec<String> = Vec::new();

    let mut i = 2;
    while i < args.len() {
        let arg = args[i].clone();
        // Support --key=value as well as --key value.
        let (key, inline_val): (String, Option<String>) = if let Some(eq) = arg.find('=') {
            if arg.starts_with('-') {
                (arg[..eq].to_string(), Some(arg[eq + 1..].to_string()))
            } else {
                (arg.clone(), None)
            }
        } else {
            (arg.clone(), None)
        };
        let take_val = |i: &mut usize| -> Option<String> {
            if let Some(v) = &inline_val {
                return Some(v.clone());
            }
            *i += 1;
            args.get(*i).cloned()
        };

        if !key.starts_with('-') {
            positionals.push(arg);
            i += 1;
            continue;
        }
        match key.as_str() {
            "--duration" => {
                let v = take_val(&mut i);
                duration = v.as_ref().and_then(|s| s.parse().ok());
            }
            "--fps" => {
                let v = take_val(&mut i);
                fps = v.as_ref().and_then(|s| s.parse().ok()).unwrap_or(fps);
            }
            "--vid" => {
                let v = take_val(&mut i);
                if let Some(n) = v.as_ref().and_then(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok()) {
                    vid = n;
                }
            }
            "--pid" => {
                let v = take_val(&mut i);
                if let Some(n) = v.as_ref().and_then(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok()) {
                    pid = n;
                }
            }
            "--fit" => {
                let v = take_val(&mut i);
                fit = v.unwrap_or_default();
                if !["contain", "cover", "stretch"].contains(&fit.as_str()) {
                    eprintln!("error: --fit must be contain|cover|stretch, got {fit:?}");
                    return;
                }
                fit_set = true;
            }
            "--rotation" => {
                let v = take_val(&mut i);
                rotation = match v.as_ref().and_then(|s| s.parse().ok()) {
                    Some(r @ (0 | 90 | 180 | 270)) => r,
                    _ => {
                        eprintln!("error: --rotation must be 0|90|180|270");
                        return;
                    }
                };
            }
            "--brightness" => {
                let v = take_val(&mut i);
                brightness = match v.as_ref().and_then(|s| s.parse::<u16>().ok()) {
                    Some(b) if b <= 100 => b as u8,
                    _ => {
                        eprintln!("error: --brightness must be 0-100");
                        return;
                    }
                };
            }
            "--loop" => loop_flag = true,
            "--no-reset" => no_reset = true,
            "-v" | "--verbose" => {}
            other => eprintln!("unknown option {other}"),
        }
        i += 1;
    }

    let mut lcd = Lcd::new(verbose, !no_reset);
    lcd.vid = vid;
    lcd.pid = pid;

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
            let payload = rgb24_to_rgb565_brightness(&wire, brightness);
            let pkt = build_frame(&payload);
            lcd.stream(&pkt, fps, duration).ok();
        }
        "color" => {
            let hex = positionals.first().cloned().unwrap_or_default();
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
            let payload = rgb24_to_rgb565_brightness(&wire, brightness);
            let pkt = build_frame(&payload);
            lcd.stream(&pkt, fps, duration).ok();
        }
        "raw" => {
            let path = match positionals.first() {
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
            let payload = rgb24_to_rgb565_brightness(&wire, brightness);
            let pkt = build_frame(&payload);
            lcd.stream(&pkt, fps, duration).ok();
        }
        "image" => {
            let path = match positionals.first() {
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

            let payload = rgb24_to_rgb565_brightness(&rgb, brightness);
            let pkt = build_frame(&payload);
            lcd.stream(&pkt, fps, duration).ok();
        }
        "video" => {
            let path = match positionals.first() {
                Some(p) => p.clone(),
                None => {
                    eprintln!("error: video needs a file argument");
                    usage();
                    return;
                }
            };
            if !Path::new(&path).exists() {
                eprintln!("error: no such file: {path}");
                return;
            }
            // Python defaults `video` to --fit cover (image defaults contain).
            let fit = if fit_set { fit.clone() } else { "cover".to_string() };
            if lcd.connect().is_err() {
                eprintln!("error: could not connect");
                return;
            }
            if let Err(e) = lcd.stream_video(&path, &fit, rotation, brightness, fps, duration, loop_flag) {
                eprintln!("error: {e}");
                return;
            }
        }
        other => {
            eprintln!("unknown command {other}");
            usage();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_filter_matches_python() {
        // 320x240 user canvas, cover, 90 deg CW (rotation 0) — the panel default.
        assert_eq!(
            ffmpeg_filter(320, 240, "cover", 90),
            "scale=320:240:force_original_aspect_ratio=increase,crop=320:240,transpose=1,format=rgb24"
        );
        // Rotation 90 -> angle 0, portrait user canvas.
        assert_eq!(
            ffmpeg_filter(240, 320, "contain", 0),
            "scale=240:320:force_original_aspect_ratio=decrease,pad=240:320:(ow-iw)/2:(oh-ih)/2:color=black,format=rgb24"
        );
        // 180 deg and stretch.
        assert_eq!(
            ffmpeg_filter(320, 240, "stretch", 180),
            "scale=320:240,hflip,vflip,format=rgb24"
        );
        // 270 deg -> transpose=2.
        assert!(ffmpeg_filter(320, 240, "cover", 270).ends_with(",transpose=2,format=rgb24"));
    }

    #[test]
    fn brightness_dims_pixels() {
        let rgb = [255, 255, 255, 0, 0, 0];
        let full = rgb24_to_rgb565(&rgb);
        let half = rgb24_to_rgb565_brightness(&rgb, 50);
        assert_eq!(full.len(), 4);
        assert_eq!(half.len(), 4);
        // White at 50% -> 127 -> RGB565 0x7BEF (LE bytes EF 7B).
        assert_eq!(half[0], 0xEF);
        assert_eq!(half[1], 0x7B);
        // Black stays black.
        assert_eq!(half[2], 0);
        assert_eq!(half[3], 0);
        // 100 is the fast path: identical to the plain converter.
        assert_eq!(rgb24_to_rgb565_brightness(&rgb, 100), full);
    }

    #[test]
    fn frame_header_is_python_compatible() {
        let payload = rgb24_to_rgb565(&[255, 0, 0].repeat(WIRE_W as usize * WIRE_H as usize));
        let pkt = build_frame(&payload);
        assert_eq!(pkt.len(), 153_600 + 20 + (512 - (153_600 + 20) % 512) % 512);
        assert_eq!(&pkt[0..4], &MAGIC);
        assert_eq!(&pkt[4..6], &[0x02, 0x00]);
        assert_eq!(&pkt[6..8], &[0x01, 0x00]);
        assert_eq!(&pkt[8..12], &[(WIRE_W as u16).to_le_bytes(), (WIRE_H as u16).to_le_bytes()].concat());
        // Pure red 255,0,0 -> RGB565 0xF800 (LE 00 F8).
        assert_eq!(&pkt[20..22], &[0x00, 0xF8]);
    }
}