//! Helpers shared by the decoder-truthfulness suites: black-box producer
//! lookups, planar-frame comparison, and the drive-a-decoder loop.
//!
//! Every suite passes by reporting "unavailable" (never `#[ignore]`)
//! when the framework, the hardware codec, or a black-box producer is
//! absent on the host.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Decoder, Error, Frame, Packet, PixelFormat, TimeBase, VideoFrame};

/// `true` when the VideoToolbox framework loads on this host.
pub fn vt_available() -> bool {
    oxideav_videotoolbox::sys::vtable().is_ok()
}

/// Path of a black-box tool (`ffmpeg`, `magick`, …), or `None`.
pub fn tool(name: &str) -> Option<PathBuf> {
    let candidates = [
        format!("/opt/homebrew/bin/{name}"),
        format!("/usr/local/bin/{name}"),
        name.to_string(),
    ];
    candidates.into_iter().map(PathBuf::from).find(|p| {
        Command::new(p)
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// `true` when this ffmpeg build lists `encoder` in `-encoders`.
pub fn ffmpeg_has_encoder(ffmpeg: &Path, encoder: &str) -> bool {
    Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.split_whitespace().nth(1) == Some(encoder))
        })
        .unwrap_or(false)
}

/// Per-suite scratch directory.
pub fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oxideav-vt-r476-{tag}"));
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// The crate's vendored fixtures.
pub fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Reference decode of `input` by the black-box reference decoder as
/// tightly packed planar frames of layout `pix_fmt`.
pub fn reference_decode(ffmpeg: &Path, input: &Path, pix_fmt: &str) -> Option<Vec<u8>> {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args(["-f", "rawvideo", "-pix_fmt", pix_fmt, "-"])
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// PSNR (dB) between two equally long 8-bit sample runs; 99 when equal.
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "psnr: length mismatch");
    if a.is_empty() {
        return 99.0;
    }
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// Tightly packed bytes of a frame's image planes, in order.
pub fn packed(frame: &VideoFrame) -> Vec<u8> {
    let mut out = Vec::new();
    for p in frame.image_planes() {
        out.extend_from_slice(&p.data);
    }
    out
}

/// Byte count of one tightly packed `label` picture of `w × h`.
pub fn picture_bytes(label: PixelFormat, w: u32, h: u32) -> usize {
    (0..label.plane_count())
        .map(|i| {
            let row = label.plane_row_bytes(i, w).unwrap();
            let (_, rows) = label.plane_dimensions(i, w, h).unwrap();
            row * rows as usize
        })
        .sum()
}

/// Assert every image plane of `frame` has exactly the geometry `label`
/// prescribes for `w × h`.
pub fn assert_geometry(frame: &VideoFrame, label: PixelFormat, w: u32, h: u32, what: &str) {
    assert!(
        oxideav_videotoolbox::pixbuf::geometry_matches(frame, label, w, h),
        "{what}: planes {:?} do not match {label:?} {w}x{h}",
        frame
            .image_planes()
            .iter()
            .map(|p| (p.stride, p.data.len()))
            .collect::<Vec<_>>()
    );
}

/// Frames the decoder produced before `flush` and the ones it released
/// at `flush`.
pub struct Decoded {
    pub before_flush: Vec<VideoFrame>,
    pub from_flush: Vec<VideoFrame>,
}

impl Decoded {
    pub fn all(&self) -> Vec<&VideoFrame> {
        self.before_flush
            .iter()
            .chain(self.from_flush.iter())
            .collect()
    }
}

fn drain(dec: &mut dyn Decoder, into: &mut Vec<VideoFrame>) -> Result<(), Error> {
    loop {
        match dec.receive_frame() {
            Ok(Frame::Video(v)) => into.push(v),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// Feed `packets` in order, then flush; frames are split by whether they
/// arrived before or at the flush. The first decode error is returned.
pub fn decode_all(dec: &mut dyn Decoder, packets: &[Packet]) -> Result<Decoded, Error> {
    let mut before_flush = Vec::new();
    for pkt in packets {
        dec.send_packet(pkt)?;
        drain(dec, &mut before_flush)?;
    }
    let mut from_flush = Vec::new();
    dec.flush()?;
    drain(dec, &mut from_flush)?;
    Ok(Decoded {
        before_flush,
        from_flush,
    })
}

/// One packet per payload, `pts` left `None` unless `stamps` is given.
pub fn packets(payloads: Vec<Vec<u8>>, stamps: Option<&[i64]>, tb: TimeBase) -> Vec<Packet> {
    payloads
        .into_iter()
        .enumerate()
        .map(|(i, data)| {
            let p = Packet::new(0, tb, data);
            match stamps {
                Some(s) => p.with_pts(s[i]),
                None => p,
            }
        })
        .collect()
}

/// Frames of an IVF file as `(timestamp, payload)`.
pub fn parse_ivf(buf: &[u8]) -> Option<Vec<(i64, Vec<u8>)>> {
    if buf.len() < 32 || &buf[0..4] != b"DKIF" {
        return None;
    }
    let mut frames = Vec::new();
    let mut i = 32;
    while i + 12 <= buf.len() {
        let size = u32::from_le_bytes(buf[i..i + 4].try_into().unwrap()) as usize;
        let ts = u64::from_le_bytes(buf[i + 4..i + 12].try_into().unwrap()) as i64;
        let start = i + 12;
        let end = start.checked_add(size)?;
        if end > buf.len() {
            return None;
        }
        frames.push((ts, buf[start..end].to_vec()));
        i = end;
    }
    Some(frames)
}
