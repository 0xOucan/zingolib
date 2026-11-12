//! - Draws an animated `ur:leek-pczt` in the terminal (stdout) and spawns a
//!   thread that reads one line of stdin to stop it.
//! - Reads the board's `ur:leek-zsig` from pasted lines (stdin), a file, or
//!   (feature `leekwallet-qr-camera`) a V4L2 webcam.

// The UR itself is `leek-ur`, the LeekWallet SDK's, held to the board
// firmware's vectors; this file only draws its frames and reads the reply.
// The drawing (half blocks, one QR version for the whole loop, the offline
// HTML page) and the parts reader are adapted from the LeekWallet
// repository's `host/zecz-cli/src/qr.rs` (Apache-2.0, by the same authors).
// The display follows `leek_ur::display_hints()` for a laptop screen:
// fragment, frame time, error correction L, a white quiet zone on a black
// page, whole pixels per module.

use std::fmt::Write as _;
use std::io::{BufRead, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use leek_ur::{Decoder, Encoder, Received, UR_PCZT, UR_ZSIG};
use qrcode::{Color, EcLevel, QrCode, Version};

/// Bytes per UR fragment for a laptop or tablet screen.
pub(crate) const DEFAULT_FRAGMENT: usize = leek_ur::display::LARGE_SCREEN_FRAGMENT_LEN;
/// Milliseconds a frame stays up on a laptop or tablet screen.
pub(crate) const DEFAULT_INTERVAL_MS: u32 = leek_ur::display::LARGE_SCREEN_FRAME_MS;
/// The frame times `--interval` accepts.
pub(crate) const INTERVAL_RANGE: std::ops::RangeInclusive<u32> = 100..=5000;

const QUIET: usize = leek_ur::display::QUIET_ZONE_MODULES as usize;

#[derive(Debug, thiserror::Error)]
pub(crate) enum QrError {
    #[error("UR: {0}")]
    Ur(#[from] leek_ur::UrError),
    #[error("a frame of {0} characters fits no QR version")]
    TooLong(usize),
    #[error("QR encoding: {0:?}")]
    Encode(qrcode::types::QrError),
    #[error("this is a ur:{0}, not the board's ur:leek-zsig")]
    WrongType(String),
    #[error("the reply ran out before the signature was complete: {0}")]
    Incomplete(String),
    #[error("the terminal: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
    #[error("camera {device}: {detail}")]
    Camera { device: String, detail: String },
}

// Every frame of the encoder's loop, at one QR version (the smallest that
// holds the longest frame, error correction L), so the code keeps its size
// while it animates. The loop wraps at `leek_ur::SEQ_WRAP` parts.
pub(crate) struct Frames {
    version: i16,
    frames: Vec<(String, QrCode)>,
    seq_len: u32,
}

pub(crate) fn frames(encoder: &Encoder) -> Result<Frames, QrError> {
    let count = if encoder.is_single_part() {
        1
    } else {
        u64::from(leek_ur::SEQ_WRAP)
    };
    let texts: Vec<String> = (0..count).map(|i| encoder.frame(i)).collect();
    let longest = texts.iter().map(String::len).max().unwrap_or_default();
    let fits = |text: &str, version| {
        QrCode::with_version(text.as_bytes(), Version::Normal(version), EcLevel::L)
    };
    let version = (1..=40i16)
        .find(|&version| texts.iter().all(|text| fits(text, version).is_ok()))
        .ok_or(QrError::TooLong(longest))?;
    let frames = texts
        .into_iter()
        .map(|text| {
            let qr = fits(&text, version).map_err(QrError::Encode)?;
            Ok((text, qr))
        })
        .collect::<Result<_, QrError>>()?;
    Ok(Frames {
        version,
        frames,
        seq_len: encoder.seq_len(),
    })
}

// Dark modules, row by row, with the quiet zone around them.
fn modules(qr: &QrCode) -> Vec<Vec<bool>> {
    let width = qr.width();
    let colors = qr.to_colors();
    let side = width + 2 * QUIET;
    let inside = |i: usize| (QUIET..width + QUIET).contains(&i);
    (0..side)
        .map(|y| {
            (0..side)
                .map(|x| {
                    inside(x)
                        && inside(y)
                        && colors[(y - QUIET) * width + (x - QUIET)] == Color::Dark
                })
                .collect()
        })
        .collect()
}

// One character per module across, two modules per line down: `▀` top
// dark, `▄` bottom dark, `█` both, space neither.
pub(crate) fn half_blocks(qr: &QrCode) -> Vec<String> {
    modules(qr)
        .chunks(2)
        .map(|pair| {
            let bottom = pair.get(1);
            pair[0]
                .iter()
                .enumerate()
                .map(|(x, &top)| match (top, bottom.is_some_and(|row| row[x])) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect()
        })
        .collect()
}

// The `n-m` of a multi-part frame, `1-1` for a single part.
fn sequence(text: &str) -> &str {
    text.split('/')
        .nth(1)
        .filter(|part| part.contains('-'))
        .unwrap_or("1-1")
}

// One frame as the terminal draws it: the screen cleared, black modules on
// bright white whatever the terminal's colours, and what to do.
pub(crate) fn terminal_frame(frames: &Frames, index: usize, interval_ms: u32) -> String {
    let (text, qr) = &frames.frames[index % frames.frames.len()];
    let mut screen = String::from("\x1b[H\x1b[2J");
    for line in half_blocks(qr) {
        let _ = writeln!(screen, "\x1b[30;107m{line}\x1b[0m");
    }
    let _ = writeln!(
        screen,
        "QR signing. Part {} ({} fragment(s)), QR version {}, {interval_ms} ms a frame.",
        sequence(text),
        frames.seq_len,
        frames.version
    );
    let _ = writeln!(
        screen,
        "On the board: Scan, point its camera here, review and ACCEPT. Press Enter when the \
         board shows its reply."
    );
    screen
}

/// - Writes frames to stdout until a line is read from stdin.
/// - Spawns a thread blocked on that stdin line.
pub(crate) fn show_terminal(frames: &Frames, interval_ms: u32) -> Result<(), QrError> {
    let side = frames.frames[0].1.width() + 2 * QUIET;
    eprintln!(
        "The code needs a terminal of at least {side} columns and {} rows; if it is cut off, \
         enlarge the window, use a smaller font, or `--html <file>` for a browser.",
        side.div_ceil(2) + 3
    );
    let stop = Arc::new(AtomicBool::new(false));
    let stopper = Arc::clone(&stop);
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        stopper.store(true, Ordering::Relaxed);
    });
    let mut out = std::io::stdout();
    let mut index = 0;
    while !stop.load(Ordering::Relaxed) {
        out.write_all(terminal_frame(frames, index, interval_ms).as_bytes())?;
        out.flush()?;
        // Slices, so Enter is seen within ~10 ms.
        let until = std::time::Instant::now() + Duration::from_millis(u64::from(interval_ms));
        while std::time::Instant::now() < until && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }
        index = (index + 1) % frames.frames.len();
    }
    out.write_all(b"\x1b[H\x1b[2J")?;
    Ok(out.flush()?)
}

// A self-contained page: inline script and style under a CSP that allows
// nothing else, so it loads no network resource. It loops the code on a
// canvas at a whole number of pixels per module, with slower, faster and
// pause.
pub(crate) fn html(frames: &Frames, encoder: &Encoder, interval_ms: u32) -> String {
    let side = frames.frames[0].1.width() + 2 * QUIET;
    let data = frames
        .frames
        .iter()
        .map(|(text, qr)| {
            let bits: String = modules(qr)
                .iter()
                .flatten()
                .map(|&dark| if dark { '1' } else { '0' })
                .collect();
            format!("[\"{}\",\"{bits}\"]", sequence(text))
        })
        .collect::<Vec<_>>()
        .join(",");
    HTML.replace("@SIZE@", &side.to_string())
        .replace("@INTERVAL@", &interval_ms.to_string())
        .replace("@VERSION@", &frames.version.to_string())
        .replace("@SEQLEN@", &frames.seq_len.to_string())
        .replace("@FRAGMENT@", &encoder.fragment_len().to_string())
        .replace("@BACKGROUND@", leek_ur::display::PAGE_BACKGROUND)
        .replace("@FRAMES@", &format!("[{data}]"))
}

const HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>LeekWallet: sign with QR (zingo-cli)</title>
<style>
  html, body { margin: 0; height: 100%; background: @BACKGROUND@; color: #fff; font: 16px system-ui, sans-serif; }
  main { display: flex; flex-direction: column; align-items: center; justify-content: center; min-height: 100%; gap: 8px; }
  canvas { image-rendering: pixelated; }
  .bar { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; justify-content: center; }
  button { font: inherit; padding: 6px 14px; }
  p { margin: 0 8px; text-align: center; }
</style>
</head>
<body>
<main>
  <p><strong>Sign with QR.</strong> On the board: Scan, point its camera here, review and ACCEPT.
     Then give zingo-cli the board's reply.</p>
  <canvas id="qr"></canvas>
  <div class="bar">
    <button id="slower">Slower</button>
    <button id="pause">Pause</button>
    <button id="faster">Faster</button>
    <span id="count"></span>
  </div>
  <p>ur:leek-pczt, @SEQLEN@ fragment(s) of @FRAGMENT@ B, QR version @VERSION@ (error correction L).
     Screen brightness up; the board at 2x.</p>
</main>
<script>
"use strict";
const FRAMES = @FRAMES@;
const SIZE = @SIZE@;
let interval = @INTERVAL@, index = 0, paused = false;
const canvas = document.getElementById("qr"), ctx = canvas.getContext("2d");
function draw() {
  const avail = Math.min(window.innerWidth * 0.96, window.innerHeight * 0.80);
  const px = Math.max(1, Math.floor(avail / SIZE));
  canvas.width = canvas.height = px * SIZE;
  ctx.fillStyle = "#fff"; ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.fillStyle = "#000";
  const bits = FRAMES[index][1];
  for (let y = 0; y < SIZE; y++) for (let x = 0; x < SIZE; x++)
    if (bits.charCodeAt(y * SIZE + x) === 49) ctx.fillRect(x * px, y * px, px, px);
  document.getElementById("count").textContent =
    "part " + FRAMES[index][0] + " · " + interval + " ms · frame " + (index + 1) + "/" + FRAMES.length;
}
function tick() { draw(); if (!paused) index = (index + 1) % FRAMES.length; setTimeout(tick, interval); }
function speed(f) { interval = Math.min(2000, Math.max(100, Math.round(interval * f / 50) * 50)); }
document.getElementById("slower").onclick = () => speed(1.5);
document.getElementById("faster").onclick = () => speed(1 / 1.5);
document.getElementById("pause").onclick = (ev) => { paused = !paused; ev.target.textContent = paused ? "Play" : "Pause"; };
window.onresize = draw;
tick();
</script>
</body>
</html>
"##;

fn progress(decoder: &Decoder, read: usize) -> String {
    if decoder.seq_len() == 0 {
        format!("{read} part(s) read, no ur:leek-zsig part yet")
    } else {
        format!(
            "{} of {} fragment(s), {read} part(s) read",
            decoder.received(),
            decoder.seq_len()
        )
    }
}

// One scanned or pasted text. `Some(zsig)` once the reply is complete. Our
// own `ur:leek-pczt` (a reflection) and non-UR text are skipped; another UR
// type is refused. A misread part does not end a scan: the fountain keeps
// sending, so it is skipped too.
fn feed(decoder: &mut Decoder, text: &str) -> Result<Option<Vec<u8>>, QrError> {
    let text = text.trim();
    match leek_ur::part_type(text) {
        Some(found) if found == UR_ZSIG => {}
        Some(found) if found == UR_PCZT => return Ok(None),
        Some(found) => return Err(QrError::WrongType(found)),
        None => return Ok(None),
    }
    match decoder.receive(text) {
        Ok(Received::Complete) => decoder
            .message()
            .map(|message| Ok(leek_ur::unwrap_bytes(message)?.to_vec()))
            .transpose(),
        Ok(_) => Ok(None),
        Err(leek_ur::UrError::Checksum) => {
            eprintln!("The assembled signature failed its checksum; starting over.");
            decoder.reset();
            Ok(None)
        }
        Err(_) => Ok(None),
    }
}

// The reply in `lines`, one UR part per line, any order, repeats and
// fountain parts welcome; blank lines and `#` comments skipped.
pub(crate) fn zsig_from_parts<'a>(
    lines: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<u8>, QrError> {
    let mut decoder = Decoder::new();
    let mut read = 0;
    for line in lines.into_iter().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        read += 1;
        if let Some(zsig) = feed(&mut decoder, line)? {
            return Ok(zsig);
        }
    }
    Err(QrError::Incomplete(progress(&decoder, read)))
}

// A reply file: the `.zsig` itself (from another reader, e.g.
// `zecz qr-scan --out`), or its UR parts as text.
pub(crate) fn zsig_from_reply(bytes: &[u8]) -> Result<Vec<u8>, QrError> {
    if bytes.starts_with(leek_zcash::leek_device::zsig::MAGIC) {
        return Ok(bytes.to_vec());
    }
    zsig_from_parts(String::from_utf8_lossy(bytes).lines())
}

/// - Reads stdin line by line until the reply is complete, an empty line,
///   or end of input.
/// - Writes the progress to stderr.
pub(crate) fn zsig_from_paste() -> Result<Option<Vec<u8>>, QrError> {
    eprintln!(
        "Paste the board's ur:leek-zsig parts, one per line, from any QR reader; an empty line \
         stops without signing (the session stays: `pczt import_qr` or `pczt abandon`)."
    );
    let mut decoder = Decoder::new();
    let mut read = 0;
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            return Ok(None);
        }
        read += 1;
        if let Some(zsig) = feed(&mut decoder, &line)? {
            return Ok(Some(zsig));
        }
        eprintln!("{}", progress(&decoder, read));
    }
    Ok(None)
}

#[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
pub(crate) mod camera {
    use std::io::Write as _;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use leek_ur::Decoder;
    use linuxvideo::Device;
    use linuxvideo::format::{PixFormat, PixelFormat};

    use super::QrError;

    pub(crate) fn decode_grey(width: usize, height: usize, grey: &[u8]) -> Vec<String> {
        let mut image =
            rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| grey[y * width + x]);
        image
            .detect_grids()
            .into_iter()
            .filter_map(|grid| grid.decode().ok().map(|(_, text)| text))
            .collect()
    }

    // The luma of a YUYV frame: every other byte, row by row.
    fn yuyv_luma(buf: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| buf[y * stride + x * 2]))
            .collect()
    }

    /// - Opens and streams the V4L2 `device` until the reply is complete or
    ///   `timeout` passes.
    /// - Writes the progress to stderr.
    pub(crate) fn scan(device: &Path, timeout: Duration) -> Result<Vec<u8>, QrError> {
        let fail = |detail: String| QrError::Camera {
            device: device.display().to_string(),
            detail,
        };
        let camera = Device::open(device).map_err(|e| fail(e.to_string()))?;
        let capture = camera
            .video_capture(PixFormat::new(1280, 720, PixelFormat::YUYV))
            .map_err(|e| fail(format!("would not capture: {e}")))?;
        let format = capture.format();
        if format.pixel_format() != PixelFormat::YUYV {
            return Err(fail(format!(
                "offers {:?}, not YUYV; pass the reply with --reply <file> instead",
                format.pixel_format()
            )));
        }
        let (width, height) = (format.width() as usize, format.height() as usize);
        let stride = (format.bytes_per_line() as usize).max(width * 2);
        eprintln!("Camera {width}x{height}: point it at the board's QR.");
        let mut stream = capture.into_stream().map_err(|e| fail(e.to_string()))?;
        let mut decoder = Decoder::new();
        let mut read = 0;
        let start = Instant::now();
        loop {
            if start.elapsed() > timeout {
                return Err(QrError::Incomplete(super::progress(&decoder, read)));
            }
            let grey = stream
                .dequeue(|buf| Ok(yuyv_luma(&buf, width, height, stride)))
                .map_err(|e| fail(e.to_string()))?;
            for text in decode_grey(width, height, &grey) {
                read += 1;
                if let Some(zsig) = super::feed(&mut decoder, &text)? {
                    eprintln!();
                    return Ok(zsig);
                }
            }
            eprint!("\r{}   ", super::progress(&decoder, read));
            let _ = std::io::stderr().flush();
        }
    }
}

/// - Writes the HTML page to `path`.
pub(crate) fn write_html(
    frames: &Frames,
    encoder: &Encoder,
    interval_ms: u32,
    path: &Path,
) -> std::io::Result<()> {
    std::fs::write(path, html(frames, encoder, interval_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zsig_parts(zsig: &[u8], fragment: usize, count: u64) -> Vec<String> {
        let encoder = Encoder::leek_zsig(zsig, fragment).expect("a 108-byte message encodes");
        (0..count).map(|i| encoder.frame(i)).collect()
    }

    // Back from half blocks to modules: what a reader of the terminal sees.
    fn unblock(lines: &[String]) -> Vec<Vec<bool>> {
        lines
            .iter()
            .flat_map(|line| {
                let (top, bottom): (Vec<bool>, Vec<bool>) = line
                    .chars()
                    .map(|c| (c == '█' || c == '▀', c == '█' || c == '▄'))
                    .unzip();
                [top, bottom]
            })
            .collect()
    }

    #[test]
    fn half_blocks_keep_every_module() {
        let qr = QrCode::with_version(b"UR:LEEK-ZSIG/AE", Version::Normal(1), EcLevel::L)
            .expect("a short text fits version 1");
        let lines = half_blocks(&qr);
        let side = 21 + 2 * QUIET;
        assert_eq!(lines.len(), side.div_ceil(2));
        assert!(lines.iter().all(|line| line.chars().count() == side));
        let mut back = unblock(&lines);
        back.truncate(side);
        assert_eq!(back, modules(&qr));
    }

    #[test]
    fn the_defaults_are_the_laptop_hints() {
        let hints = leek_ur::display_hints();
        assert_eq!(DEFAULT_FRAGMENT, hints.large_screen_fragment_len);
        assert_eq!(DEFAULT_INTERVAL_MS, hints.large_screen_frame_ms);
        assert!(INTERVAL_RANGE.contains(&DEFAULT_INTERVAL_MS));
    }

    #[test]
    fn frames_keep_one_version() {
        let encoder = Encoder::leek_pczt(&[0xA5; 1373], DEFAULT_FRAGMENT)
            .expect("a compact view's size encodes");
        let frames = frames(&encoder).expect("50-byte fragments fit a QR code");
        assert_eq!(frames.frames.len(), leek_ur::SEQ_WRAP as usize);
        assert!(
            frames
                .frames
                .iter()
                .all(|(_, qr)| qr.version() == Version::Normal(frames.version))
        );
        let screen = terminal_frame(&frames, 0, DEFAULT_INTERVAL_MS);
        assert!(screen.starts_with("\x1b[H\x1b[2J") && screen.contains("\x1b[30;107m"));
        assert!(screen.contains("Part 1-28") && screen.contains("QR signing"));
    }

    #[test]
    fn the_html_page_is_offline_and_complete() {
        let encoder = Encoder::leek_pczt(&[7; 300], 90).expect("300 bytes encode");
        let frames = frames(&encoder).expect("90-byte fragments fit a QR code");
        let page = html(&frames, &encoder, 300);
        assert!(!page.contains('@'), "a placeholder was left");
        assert!(!page.contains("http://") && !page.contains("https://"));
        assert!(page.contains("default-src 'none'") && page.contains("Sign with QR."));
        assert_eq!(page.matches("[\"").count(), frames.frames.len());
    }

    #[test]
    fn parts_in_any_order() {
        let zsig: Vec<u8> = (0..108).collect();
        let mut parts = zsig_parts(&zsig, 20, 30);
        parts.remove(1);
        parts.remove(3);
        let repeat = parts[4].clone();
        parts.insert(0, repeat);
        parts.reverse();
        let mut lines = vec![
            "# from a phone reader".to_string(),
            String::new(),
            "zcash:t1notaur".into(),
            Encoder::leek_pczt(&[1; 40], 512)
                .expect("40 bytes encode")
                .frame(0),
        ];
        lines.extend(parts.iter().map(|part| part.to_lowercase()));
        assert_eq!(
            zsig_from_parts(lines.iter().map(String::as_str)).expect("the parts complete"),
            zsig
        );
        let single = zsig_parts(&zsig, 512, 1);
        assert_eq!(
            zsig_from_reply(single[0].as_bytes()).expect("one part is the whole reply"),
            zsig
        );
    }

    #[test]
    fn a_raw_zsig_file_is_taken_as_is() {
        let raw = [b"ZSIG".as_slice(), &[1; 37]].concat();
        assert_eq!(zsig_from_reply(&raw).expect("a .zsig passes through"), raw);
    }

    #[test]
    fn short_or_foreign_replies_are_refused() {
        let parts = zsig_parts(&[9; 108], 20, 2);
        assert!(matches!(
            zsig_from_parts(parts.iter().map(String::as_str)),
            Err(QrError::Incomplete(_))
        ));
        assert!(matches!(
            zsig_from_parts(["UR:CRYPTO-PSBT/AEAEAEAE"]),
            Err(QrError::WrongType(_))
        ));
    }

    #[cfg(all(feature = "leekwallet-qr-camera", target_os = "linux"))]
    #[test]
    fn a_rendered_frame_reads_back() {
        let zsig: Vec<u8> = (0..108).collect();
        let encoder = Encoder::leek_zsig(&zsig, 40).expect("108 bytes encode");
        let frames = frames(&encoder).expect("40-byte fragments fit a QR code");
        let mut decoder = Decoder::new();
        let read = frames.frames.iter().take(10).find_map(|(_, qr)| {
            let grid = modules(qr);
            let px = 4;
            let side = grid.len() * px;
            let grey: Vec<u8> = (0..side * side)
                .map(|i| {
                    if grid[(i / side) / px][(i % side) / px] {
                        0
                    } else {
                        255
                    }
                })
                .collect();
            camera::decode_grey(side, side, &grey)
                .iter()
                .find_map(|text| feed(&mut decoder, text).expect("our own parts feed"))
        });
        assert_eq!(read.expect("ten frames of a 3-part loop complete"), zsig);
    }
}
