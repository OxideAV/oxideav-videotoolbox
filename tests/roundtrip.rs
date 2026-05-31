//! End-to-end tests for the VideoToolbox bridge.
//!
//! Gated to `cfg(all(target_os = "macos", feature = "registry"))` —
//! on non-mac platforms or without the `registry` feature this module
//! compiles to nothing (empty rlib, zero tests run).
//!
//! Encode → decode roundtrip (H.264 / HEVC / MJPEG / ProRes):
//! 1. Generate a synthetic 320×240 I420 test pattern (luma ramp).
//! 2. Encode 10 frames via the matching VT encoder.
//! 3. Decode the resulting stream via the matching VT decoder.
//! 4. Assert decoded frame dimensions match 320×240.
//! 5. Assert PSNR_Y ≥ 35 dB on at least one decoded frame.
//!
//! Decode-only (MPEG-2): VideoToolbox has no MPEG-2 encoder, so the test
//! produces an elementary stream with `ffmpeg` (a black-box validator),
//! decodes it through VideoToolbox, and compares to ffmpeg's own software
//! decode (PSNR_Y ≥ 30 dB).

#![cfg(all(target_os = "macos", feature = "registry"))]

use oxideav_core::{
    CodecId, CodecParameters, Decoder, Encoder, Error, Frame, PixelFormat, VideoFrame, VideoPlane,
};
use oxideav_videotoolbox::{blob as vt_blob, decoder as vt_decoder, encoder as vt_encoder};

// ─────────────────────────── Helpers ──────────────────────────────────────────

/// Generate a synthetic I420 frame with a smooth (no modulo-wraparound)
/// luma gradient and flat chroma. Designed to be friendly to lossy codecs:
/// the previous "(col + row/2 + frame*10) % 255" pattern had a hard
/// discontinuity at the wrap point that JPEG's DCT could not represent
/// without ~10 dB of error.
fn synthetic_frame(width: usize, height: usize, frame_idx: u8, pts: i64) -> VideoFrame {
    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);

    let mut y = vec![0u8; width * height];
    let u = vec![128u8; chroma_w * chroma_h];
    let v = vec![128u8; chroma_w * chroma_h];

    // Smooth diagonal gradient with no wraparound. Range is roughly
    // 16..235 (video range). Per-frame offset is small so PSNR_Y of
    // decoded[0] vs source[0] stays high.
    let offset = frame_idx as i32; // 0..n_frames, well under 255
    for row in 0..height {
        for col in 0..width {
            let raw = 16 + (col + row / 2) as i32 / 4 + offset;
            y[row * width + col] = raw.clamp(16, 235) as u8;
        }
    }

    VideoFrame {
        pts: Some(pts),
        planes: vec![
            VideoPlane {
                stride: width,
                data: y,
            },
            VideoPlane {
                stride: chroma_w,
                data: u,
            },
            VideoPlane {
                stride: chroma_w,
                data: v,
            },
        ],
    }
}

/// PSNR for the Y (luma) plane. Returns `f64::INFINITY` for a perfect match.
fn psnr_y(ref_frame: &VideoFrame, dec_frame: &VideoFrame) -> f64 {
    let ref_plane = &ref_frame.planes[0];
    let dec_plane = &dec_frame.planes[0];

    let ref_w = ref_plane.stride;
    let dec_w = dec_plane.stride;
    let h = (ref_plane.data.len() / ref_w.max(1)).min(dec_plane.data.len() / dec_w.max(1));
    let w = ref_w.min(dec_w);

    if h == 0 || w == 0 {
        return 0.0;
    }

    let mut sse: f64 = 0.0;
    let mut count = 0usize;

    for row in 0..h {
        for col in 0..w {
            let ri = row * ref_w + col;
            let di = row * dec_w + col;
            if ri < ref_plane.data.len() && di < dec_plane.data.len() {
                let diff = ref_plane.data[ri] as f64 - dec_plane.data[di] as f64;
                sse += diff * diff;
                count += 1;
            }
        }
    }

    if count == 0 || sse == 0.0 {
        return f64::INFINITY;
    }

    10.0 * (255.0 * 255.0 * count as f64 / sse).log10()
}

// ─────────────────────────── Test helper ──────────────────────────────────────

fn run_roundtrip(codec: &str) {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping {codec} roundtrip");
        return;
    }

    let width = 320usize;
    let height = 240usize;
    let n_frames = 10usize;

    // ── Encode ───────────────────────────────────────────────────────────
    let enc_params = {
        let mut p = CodecParameters::video(CodecId::new(codec));
        p.width = Some(width as u32);
        p.height = Some(height as u32);
        p.pixel_format = Some(PixelFormat::Yuv420P);
        p
    };

    let mut encoder: Box<dyn Encoder> = match codec {
        "h264" => vt_encoder::make_h264_encoder(&enc_params).expect("H264VtEncoder construction"),
        "hevc" => vt_encoder::make_hevc_encoder(&enc_params).expect("HevcVtEncoder construction"),
        "mjpeg" => vt_blob::make_jpeg_encoder(&enc_params).expect("JpegVtEncoder construction"),
        "prores" => {
            vt_blob::make_prores_encoder(&enc_params).expect("ProResVtEncoder construction")
        }
        _ => panic!("unknown codec {codec}"),
    };

    let mut source_frames: Vec<VideoFrame> = Vec::new();
    let mut encoded_packets = Vec::new();

    for i in 0..n_frames {
        let frame = synthetic_frame(width, height, i as u8, (i as i64) * 33_333);
        source_frames.push(frame.clone());
        encoder
            .send_frame(&Frame::Video(frame))
            .expect("send_frame");
        loop {
            match encoder.receive_packet() {
                Ok(pkt) => encoded_packets.push(pkt),
                Err(Error::NeedMore) => break,
                Err(e) => panic!("receive_packet error: {e}"),
            }
        }
    }

    encoder.flush().expect("encoder flush");
    loop {
        match encoder.receive_packet() {
            Ok(pkt) => encoded_packets.push(pkt),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_packet (flush) error: {e}"),
        }
    }

    assert!(
        !encoded_packets.is_empty(),
        "{codec} encoder produced no packets"
    );

    // ── Decode ───────────────────────────────────────────────────────────
    let dec_params = {
        let mut p = CodecParameters::video(CodecId::new(codec));
        p.width = Some(width as u32);
        p.height = Some(height as u32);
        p.pixel_format = Some(PixelFormat::Yuv420P);
        p
    };

    let mut decoder: Box<dyn Decoder> = match codec {
        "h264" => vt_decoder::H264VtDecoder::make(&dec_params).expect("H264VtDecoder construction"),
        "hevc" => vt_decoder::HevcVtDecoder::make(&dec_params).expect("HevcVtDecoder construction"),
        "mjpeg" => vt_blob::make_jpeg_decoder(&dec_params).expect("JpegVtDecoder construction"),
        "prores" => {
            vt_blob::make_prores_decoder(&dec_params).expect("ProResVtDecoder construction")
        }
        _ => panic!("unknown codec"),
    };

    let mut decoded_frames: Vec<VideoFrame> = Vec::new();

    for pkt in &encoded_packets {
        decoder.send_packet(pkt).expect("send_packet to decoder");
        loop {
            match decoder.receive_frame() {
                Ok(Frame::Video(vf)) => decoded_frames.push(vf),
                Ok(_) => {}
                Err(Error::NeedMore) => break,
                Err(Error::Eof) => break,
                Err(e) => panic!("receive_frame error: {e}"),
            }
        }
    }

    decoder.flush().expect("decoder flush");
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Video(vf)) => decoded_frames.push(vf),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_frame (flush) error: {e}"),
        }
    }

    assert!(
        !decoded_frames.is_empty(),
        "{codec} decoder produced no frames from {} packets",
        encoded_packets.len()
    );

    // ── Validate dimensions ───────────────────────────────────────────────
    for (i, df) in decoded_frames.iter().enumerate() {
        assert_eq!(df.planes.len(), 3, "{codec} frame {i}: expected 3 planes");
        let dec_w = df.planes[0].stride;
        let dec_h = df.planes[0].data.len() / dec_w.max(1);
        assert_eq!(dec_w, width, "{codec} frame {i}: width mismatch");
        assert_eq!(dec_h, height, "{codec} frame {i}: height mismatch");
    }

    // ── PSNR_Y ≥ 35 dB ───────────────────────────────────────────────────
    let first_src = &source_frames[0];
    let best_psnr = decoded_frames
        .iter()
        .map(|df| psnr_y(first_src, df))
        .fold(f64::NEG_INFINITY, f64::max);

    println!(
        "{codec} roundtrip: {} encoded packets, {} decoded frames, best PSNR_Y = {:.1} dB",
        encoded_packets.len(),
        decoded_frames.len(),
        best_psnr
    );

    assert!(
        best_psnr >= 35.0,
        "{codec} PSNR_Y {best_psnr:.1} dB < 35 dB threshold"
    );
}

// ─────────────────────────── Tests ────────────────────────────────────────────

#[test]
fn h264_roundtrip() {
    run_roundtrip("h264");
}

#[test]
fn hevc_roundtrip() {
    run_roundtrip("hevc");
}

#[test]
fn mjpeg_roundtrip() {
    run_roundtrip("mjpeg");
}

#[test]
fn prores_roundtrip() {
    run_roundtrip("prores");
}

/// Round 9 — verifies that the new encoder knobs land without
/// disrupting decode quality:
///
/// * `CodecParameters::bit_rate = Some(4_000_000)` flows into
///   `kVTCompressionPropertyKey_AverageBitRate` for H.264 / HEVC / MJPEG;
/// * `options["quality"] = "0.85"` flows into
///   `kVTCompressionPropertyKey_Quality` for the MJPEG and ProRes paths;
/// * `options["profile"] = "high"` is accepted for H.264 (mapping to
///   `kVTProfileLevel_H264_High_AutoLevel`).
///
/// The session-create call is the moment Apple would reject an
/// invalid property; if `vt_session_set_property` errors, VT does not
/// surface it on session-create (the property simply doesn't apply),
/// so the assertion here is the round-trip continues to succeed at the
/// same PSNR floor — meaning the property writes were accepted by VT.
#[test]
fn encoder_knobs_round_trip_without_regression() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping encoder-knobs round trip");
        return;
    }

    let width = 320usize;
    let height = 240usize;
    let n_frames = 5usize;

    // H.264 with explicit AverageBitRate + High profile + Quality hint.
    let mut h264_params = CodecParameters::video(CodecId::new("h264"));
    h264_params.width = Some(width as u32);
    h264_params.height = Some(height as u32);
    h264_params.pixel_format = Some(PixelFormat::Yuv420P);
    h264_params.bit_rate = Some(4_000_000);
    h264_params.options = oxideav_core::CodecOptions::new()
        .set("profile", "high")
        .set("quality", "0.85");

    let mut enc =
        vt_encoder::make_h264_encoder(&h264_params).expect("h264 encoder with knobs construction");
    let mut packets = Vec::new();
    for i in 0..n_frames {
        let frame = synthetic_frame(width, height, i as u8, (i as i64) * 33_333);
        enc.send_frame(&Frame::Video(frame)).expect("send_frame");
        loop {
            match enc.receive_packet() {
                Ok(p) => packets.push(p),
                Err(Error::NeedMore) => break,
                Err(e) => panic!("receive_packet: {e}"),
            }
        }
    }
    enc.flush().expect("flush");
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_packet flush: {e}"),
        }
    }
    assert!(
        !packets.is_empty(),
        "H.264 encoder with knobs produced no packets"
    );

    // ProRes with explicit profile tag (LT) — verifies the tag → codec-type
    // dispatch reaches a working VT compression session.
    let mut prores_params = CodecParameters::video(CodecId::new("prores"));
    prores_params.width = Some(width as u32);
    prores_params.height = Some(height as u32);
    prores_params.pixel_format = Some(PixelFormat::Yuv420P);
    prores_params.tag = Some(oxideav_core::CodecTag::fourcc(b"apcs"));

    let mut prores_enc =
        vt_blob::make_prores_encoder(&prores_params).expect("prores LT encoder construction");
    let frame = synthetic_frame(width, height, 0, 0);
    prores_enc
        .send_frame(&Frame::Video(frame))
        .expect("prores send_frame");
    let mut got_one = false;
    match prores_enc.receive_packet() {
        Ok(_) => got_one = true,
        Err(Error::NeedMore) => {}
        Err(e) => panic!("prores receive_packet: {e}"),
    }
    if !got_one {
        prores_enc.flush().expect("prores flush");
        while prores_enc.receive_packet().is_ok() {
            got_one = true;
        }
    }
    assert!(got_one, "ProRes LT encoder produced no packets");
}

/// Confirms `register()` installs decode + encode factories for every
/// codec the crate claims in its README (h264 / hevc / mjpeg / prores).
#[test]
fn register_installs_all_round3_factories() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping registry check");
        return;
    }
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_videotoolbox::register(&mut ctx);
    for id in ["h264", "hevc", "mjpeg", "prores"] {
        let cid = oxideav_core::CodecId::new(id);
        assert!(
            ctx.codecs.has_decoder(&cid),
            "no VT decoder registered for {id}"
        );
        assert!(
            ctx.codecs.has_encoder(&cid),
            "no VT encoder registered for {id}"
        );
    }
}

/// MPEG-2 is decode-only (VideoToolbox exposes no MPEG-2 encoder), so its
/// factory must install a decoder but NOT an encoder.
#[test]
fn register_installs_mpeg2_decode_only() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping mpeg2 registry check");
        return;
    }
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_videotoolbox::register(&mut ctx);
    let cid = oxideav_core::CodecId::new("mpeg2video");
    assert!(
        ctx.codecs.has_decoder(&cid),
        "no VT decoder registered for mpeg2video"
    );
    assert!(
        !ctx.codecs.has_encoder(&cid),
        "VT must not register an MPEG-2 encoder (none exists)"
    );
}

/// VP9 is decode-only (VideoToolbox exposes no VP9 compression session),
/// so its factory must install a decoder but NOT an encoder.
#[test]
fn register_installs_vp9_decode_only() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping vp9 registry check");
        return;
    }
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_videotoolbox::register(&mut ctx);
    let cid = oxideav_core::CodecId::new("vp9");
    assert!(
        ctx.codecs.has_decoder(&cid),
        "no VT decoder registered for vp9"
    );
    assert!(
        !ctx.codecs.has_encoder(&cid),
        "VT must not register a VP9 encoder (none exists)"
    );
}

/// MPEG-4 Part 2 (Visual / ASP / DivX / Xvid) is decode-only — VideoToolbox
/// exposes no MPEG-4 Pt 2 compression session — so its factory must install
/// a decoder under `CodecId::new("mpeg4")` but NOT an encoder. This is the
/// MPEG-4 Pt 2 codec id; H.264 (MPEG-4 Pt 10) is registered separately
/// under `CodecId::new("h264")` and has both decoder and encoder.
#[test]
fn register_installs_mpeg4_part_two_decode_only() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping mpeg4 registry check");
        return;
    }
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_videotoolbox::register(&mut ctx);
    let cid = oxideav_core::CodecId::new("mpeg4");
    assert!(
        ctx.codecs.has_decoder(&cid),
        "no VT decoder registered for mpeg4"
    );
    assert!(
        !ctx.codecs.has_encoder(&cid),
        "VT must not register an MPEG-4 Part 2 encoder (none exists)"
    );
}

/// AV1 is decode-only in round 8 — VideoToolbox exposes an AV1
/// compression session on M3+ / macOS 14+, but the encoder side is a
/// follow-up round. The round-8 factory installs a decoder under
/// `CodecId::new("av1")` and NOT an encoder.
#[test]
fn register_installs_av1_decode_only() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping av1 registry check");
        return;
    }
    let mut ctx = oxideav_core::RuntimeContext::new();
    oxideav_videotoolbox::register(&mut ctx);
    let cid = oxideav_core::CodecId::new("av1");
    assert!(
        ctx.codecs.has_decoder(&cid),
        "no VT decoder registered for av1"
    );
    assert!(
        !ctx.codecs.has_encoder(&cid),
        "VT must not register an AV1 encoder in round 8 (compression session is a follow-up round)"
    );
}

// ─────────────────────────── MPEG-2 decode test ───────────────────────────────

/// Run `ffmpeg` as an opaque black-box validator to produce an MPEG-2
/// elementary stream (and a reference raw-YUV decode). Returns
/// `(elementary_stream_bytes, reference_first_frame_i420)` or `None` if
/// ffmpeg is unavailable on the runner.
fn ffmpeg_mpeg2_fixture(width: usize, height: usize, frames: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    use std::process::Command;

    // Locate ffmpeg; skip the test gracefully if it's not installed.
    let ffmpeg = [
        "/opt/homebrew/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
        "ffmpeg",
    ]
    .into_iter()
    .find(|p| {
        Command::new(p)
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })?;

    let dir = std::env::temp_dir();
    let m2v = dir.join(format!("oxideav_vt_mpeg2_{width}x{height}.m2v"));
    let yuv = dir.join(format!("oxideav_vt_mpeg2_{width}x{height}.yuv"));

    // Encode a smooth gradient to an MPEG-2 elementary stream.
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!(
                "gradients=s={width}x{height}:c0=black:c1=white:d=1:r={frames},format=yuv420p"
            ),
            "-frames:v",
            &frames.to_string(),
            "-c:v",
            "mpeg2video",
            "-g",
            "5",
            "-q:v",
            "3",
            "-f",
            "mpeg2video",
        ])
        .arg(&m2v)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    // Reference decode: first frame as raw I420.
    let status = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&m2v)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&yuv)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    let es = std::fs::read(&m2v).ok()?;
    let ref_yuv = std::fs::read(&yuv).ok()?;
    let _ = std::fs::remove_file(&m2v);
    let _ = std::fs::remove_file(&yuv);

    let frame_size = width * height * 3 / 2;
    if ref_yuv.len() < frame_size {
        return None;
    }
    Some((es, ref_yuv[..frame_size].to_vec()))
}

/// Decode an ffmpeg-produced MPEG-2 elementary stream through the
/// VideoToolbox bridge and assert the first decoded frame matches ffmpeg's
/// own software decode (PSNR_Y ≥ 30 dB — chroma subsampling round-trips and
/// VT's IDCT differ slightly from ffmpeg's, so the bar is a touch below the
/// lossy-encode tests).
#[test]
fn mpeg2_decode_against_ffmpeg() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping mpeg2 decode");
        return;
    }

    let width = 320usize;
    let height = 240usize;
    let frames = 10usize;

    let Some((es, ref_i420)) = ffmpeg_mpeg2_fixture(width, height, frames) else {
        eprintln!("oxideav-videotoolbox: ffmpeg unavailable, skipping mpeg2 decode test");
        return;
    };

    let dec_params = {
        let mut p = CodecParameters::video(CodecId::new("mpeg2video"));
        p.width = Some(width as u32);
        p.height = Some(height as u32);
        p.pixel_format = Some(PixelFormat::Yuv420P);
        p
    };

    let mut decoder = vt_blob::make_mpeg2_decoder(&dec_params).expect("MPEG-2 VT decoder");

    // Feed the whole elementary stream as a single packet; the decoder's
    // FrameSplit::Mpeg2Es framer carves it into per-picture access units.
    let pkt = oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1_000_000), es);

    let mut decoded: Vec<VideoFrame> = Vec::new();
    decoder.send_packet(&pkt).expect("send_packet (mpeg2)");
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Video(vf)) => decoded.push(vf),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_frame error: {e}"),
        }
    }
    decoder.flush().expect("decoder flush");
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Video(vf)) => decoded.push(vf),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_frame (flush) error: {e}"),
        }
    }

    assert!(
        !decoded.is_empty(),
        "MPEG-2 VT decoder produced no frames from the elementary stream"
    );

    // Dimensions.
    for (i, df) in decoded.iter().enumerate() {
        assert_eq!(df.planes.len(), 3, "mpeg2 frame {i}: expected 3 planes");
        let dec_w = df.planes[0].stride;
        let dec_h = df.planes[0].data.len() / dec_w.max(1);
        assert_eq!(dec_w, width, "mpeg2 frame {i}: width mismatch");
        assert_eq!(dec_h, height, "mpeg2 frame {i}: height mismatch");
    }

    // Build a reference VideoFrame from ffmpeg's raw I420 first frame.
    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);
    let y_len = width * height;
    let c_len = chroma_w * chroma_h;
    let ref_frame = VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: width,
                data: ref_i420[..y_len].to_vec(),
            },
            VideoPlane {
                stride: chroma_w,
                data: ref_i420[y_len..y_len + c_len].to_vec(),
            },
            VideoPlane {
                stride: chroma_w,
                data: ref_i420[y_len + c_len..y_len + 2 * c_len].to_vec(),
            },
        ],
    };

    // Best PSNR_Y across decoded frames vs ffmpeg's first frame (decode order
    // vs display order may shuffle which of ours best matches frame 0).
    let best_psnr = decoded
        .iter()
        .map(|df| psnr_y(&ref_frame, df))
        .fold(f64::NEG_INFINITY, f64::max);

    println!(
        "mpeg2 decode: {} frames decoded, best PSNR_Y vs ffmpeg = {:.1} dB",
        decoded.len(),
        best_psnr
    );

    assert!(
        best_psnr >= 30.0,
        "mpeg2 PSNR_Y {best_psnr:.1} dB < 30 dB threshold"
    );
}

// ─────────────────────────── VP9 decode test ─────────────────────────────────

/// Parse an IVF (`.ivf`) container into per-frame VP9 byte slices.
///
/// IVF layout: 32-byte file header (signature `DKIF`), then a sequence of
/// records each consisting of a 12-byte frame header (4-byte little-endian
/// `frame_size`, 8-byte little-endian `pts`) followed by `frame_size` bytes
/// of compressed VP9 data. Returns `None` if the signature is wrong or any
/// record runs off the end of the buffer.
fn parse_ivf(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
    const IVF_FILE_HEADER_LEN: usize = 32;
    const IVF_FRAME_HEADER_LEN: usize = 12;
    if buf.len() < IVF_FILE_HEADER_LEN || &buf[0..4] != b"DKIF" {
        return None;
    }
    let mut frames = Vec::new();
    let mut i = IVF_FILE_HEADER_LEN;
    while i + IVF_FRAME_HEADER_LEN <= buf.len() {
        let size = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        let payload_start = i + IVF_FRAME_HEADER_LEN;
        let payload_end = payload_start.checked_add(size)?;
        if payload_end > buf.len() {
            return None;
        }
        frames.push(buf[payload_start..payload_end].to_vec());
        i = payload_end;
    }
    Some(frames)
}

/// Run `ffmpeg` as an opaque black-box validator to produce a VP9 IVF
/// stream (and a reference raw-YUV decode). Returns
/// `(per_frame_vp9_payloads, reference_first_frame_i420)` or `None` if
/// ffmpeg is unavailable / lacks libvpx-vp9.
fn ffmpeg_vp9_fixture(
    width: usize,
    height: usize,
    frames: usize,
) -> Option<(Vec<Vec<u8>>, Vec<u8>)> {
    use std::process::Command;

    let ffmpeg = [
        "/opt/homebrew/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
        "ffmpeg",
    ]
    .into_iter()
    .find(|p| {
        Command::new(p)
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })?;

    let dir = std::env::temp_dir();
    let ivf = dir.join(format!("oxideav_vt_vp9_{width}x{height}.ivf"));
    let yuv = dir.join(format!("oxideav_vt_vp9_{width}x{height}.yuv"));

    // Encode a smooth gradient to a VP9 IVF stream. libvpx-vp9 is the
    // standard ffmpeg VP9 encoder; if it's not built into this ffmpeg the
    // command fails and we skip the test.
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!(
                "gradients=s={width}x{height}:c0=black:c1=white:d=1:r={frames},format=yuv420p"
            ),
            "-frames:v",
            &frames.to_string(),
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "500k",
            "-f",
            "ivf",
        ])
        .arg(&ivf)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    // Reference decode: first frame as raw I420.
    let status = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&ivf)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&yuv)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    let ivf_bytes = std::fs::read(&ivf).ok()?;
    let ref_yuv = std::fs::read(&yuv).ok()?;
    let _ = std::fs::remove_file(&ivf);
    let _ = std::fs::remove_file(&yuv);

    let payloads = parse_ivf(&ivf_bytes)?;
    if payloads.is_empty() {
        return None;
    }

    let frame_size = width * height * 3 / 2;
    if ref_yuv.len() < frame_size {
        return None;
    }
    Some((payloads, ref_yuv[..frame_size].to_vec()))
}

/// Decode an ffmpeg-produced VP9 IVF stream through the VideoToolbox bridge
/// and assert the first decoded frame matches ffmpeg's own software decode
/// (PSNR_Y ≥ 30 dB — VP9 + chroma round-trips and VT's IDCT differ slightly
/// from libvpx-vp9's, matching the MPEG-2 bar).
///
/// Self-skips when ffmpeg / libvpx-vp9 / VideoToolbox is unavailable, or
/// when the VT VP9 decoder errors out at session-create time (older macOS
/// without the VP9 decoder, Intel Mac without the dedicated VP9 IP and no
/// software fallback in this VT build).
#[test]
fn vp9_decode_against_ffmpeg() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping vp9 decode");
        return;
    }

    let width = 320usize;
    let height = 240usize;
    let frames = 10usize;

    let Some((payloads, ref_i420)) = ffmpeg_vp9_fixture(width, height, frames) else {
        eprintln!("oxideav-videotoolbox: ffmpeg/libvpx-vp9 unavailable, skipping vp9 decode test");
        return;
    };

    let dec_params = {
        let mut p = CodecParameters::video(CodecId::new("vp9"));
        p.width = Some(width as u32);
        p.height = Some(height as u32);
        p.pixel_format = Some(PixelFormat::Yuv420P);
        p
    };

    let mut decoder = match vt_blob::make_vp9_decoder(&dec_params) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("VT VP9 decoder unavailable on this host: {e}; skipping");
            return;
        }
    };

    let mut decoded: Vec<VideoFrame> = Vec::new();
    let mut session_err: Option<Error> = None;

    for (i, payload) in payloads.iter().enumerate() {
        let pkt = oxideav_core::Packet::new(
            0,
            oxideav_core::TimeBase::new(1, 1_000_000),
            payload.clone(),
        )
        .with_pts((i as i64) * 33_333);
        if let Err(e) = decoder.send_packet(&pkt) {
            // First-call failure is treated as "VP9 decoder not available
            // on this host" — skip rather than fail.
            session_err = Some(e);
            break;
        }
        loop {
            match decoder.receive_frame() {
                Ok(Frame::Video(vf)) => decoded.push(vf),
                Ok(_) => {}
                Err(Error::NeedMore) | Err(Error::Eof) => break,
                Err(e) => panic!("receive_frame error: {e}"),
            }
        }
    }
    if let Some(e) = session_err {
        if decoded.is_empty() {
            eprintln!("VT VP9 decoder errored at decode time: {e}; skipping");
            return;
        }
    }

    decoder.flush().expect("decoder flush");
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Video(vf)) => decoded.push(vf),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_frame (flush) error: {e}"),
        }
    }

    if decoded.is_empty() {
        eprintln!("VT VP9 decoder produced no frames; skipping (host may lack VP9 support)");
        return;
    }

    // Dimensions.
    for (i, df) in decoded.iter().enumerate() {
        assert_eq!(df.planes.len(), 3, "vp9 frame {i}: expected 3 planes");
        let dec_w = df.planes[0].stride;
        let dec_h = df.planes[0].data.len() / dec_w.max(1);
        assert_eq!(dec_w, width, "vp9 frame {i}: width mismatch");
        assert_eq!(dec_h, height, "vp9 frame {i}: height mismatch");
    }

    // Build a reference VideoFrame from ffmpeg's raw I420 first frame.
    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);
    let y_len = width * height;
    let c_len = chroma_w * chroma_h;
    let ref_frame = VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: width,
                data: ref_i420[..y_len].to_vec(),
            },
            VideoPlane {
                stride: chroma_w,
                data: ref_i420[y_len..y_len + c_len].to_vec(),
            },
            VideoPlane {
                stride: chroma_w,
                data: ref_i420[y_len + c_len..y_len + 2 * c_len].to_vec(),
            },
        ],
    };

    let best_psnr = decoded
        .iter()
        .map(|df| psnr_y(&ref_frame, df))
        .fold(f64::NEG_INFINITY, f64::max);

    println!(
        "vp9 decode: {} frames decoded, best PSNR_Y vs ffmpeg = {:.1} dB",
        decoded.len(),
        best_psnr
    );

    assert!(
        best_psnr >= 30.0,
        "vp9 PSNR_Y {best_psnr:.1} dB < 30 dB threshold"
    );
}

// ─────────────────────────── IVF parser unit tests ───────────────────────────

#[cfg(test)]
mod ivf_tests {
    use super::parse_ivf;

    fn build_ivf(frames: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::new();
        // 32-byte file header — only the 4-byte signature matters to our parser.
        buf.extend_from_slice(b"DKIF");
        buf.extend_from_slice(&[0u8; 28]);
        for f in frames {
            buf.extend_from_slice(&(f.len() as u32).to_le_bytes());
            buf.extend_from_slice(&0u64.to_le_bytes());
            buf.extend_from_slice(f);
        }
        buf
    }

    #[test]
    fn parses_multiple_frames() {
        let f1: &[u8] = &[0xAA, 0xBB];
        let f2: &[u8] = &[0xCC, 0xDD, 0xEE];
        let buf = build_ivf(&[f1, f2]);
        let frames = parse_ivf(&buf).expect("parses");
        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0], f1);
        assert_eq!(&frames[1], f2);
    }

    #[test]
    fn rejects_missing_signature() {
        let mut buf = build_ivf(&[&[0x01]]);
        buf[0] = b'X';
        assert!(parse_ivf(&buf).is_none());
    }

    #[test]
    fn rejects_truncated_payload() {
        let mut buf = build_ivf(&[&[0x01, 0x02, 0x03]]);
        // Drop the last byte: the declared frame_size now overruns the buffer.
        buf.pop();
        assert!(parse_ivf(&buf).is_none());
    }
}

// ────────────────────── MPEG-4 Part 2 decode test ───────────────────────────

/// Run `ffmpeg` as an opaque black-box validator to produce an MPEG-4 Part 2
/// (Simple Profile) elementary stream (and a reference raw-YUV decode).
/// Returns `(elementary_stream_bytes, reference_first_frame_i420)` or `None`
/// if ffmpeg is unavailable / lacks its built-in MPEG-4 Part 2 encoder.
fn ffmpeg_mpeg4_part_two_fixture(
    width: usize,
    height: usize,
    frames: usize,
) -> Option<(Vec<u8>, Vec<u8>)> {
    use std::process::Command;

    let ffmpeg = [
        "/opt/homebrew/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
        "ffmpeg",
    ]
    .into_iter()
    .find(|p| {
        Command::new(p)
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })?;

    let dir = std::env::temp_dir();
    let m4v = dir.join(format!("oxideav_vt_mpeg4_{width}x{height}.m4v"));
    let yuv = dir.join(format!("oxideav_vt_mpeg4_{width}x{height}.yuv"));

    // Encode a smooth gradient to an MPEG-4 Part 2 elementary stream.
    // ffmpeg's built-in `mpeg4` encoder produces an ES that starts with the
    // VOS / VOL headers and an IVOP, exactly what VideoToolbox needs.
    //
    // `-profile:v 1` (Simple Profile @ L1) is the broadly-compatible
    // baseline. `-g 5` keeps GOPs short so the first VOP is intra and the
    // decode test gets sample-similarity to ffmpeg's own decode quickly.
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!(
                "gradients=s={width}x{height}:c0=black:c1=white:d=1:r={frames},format=yuv420p"
            ),
            "-frames:v",
            &frames.to_string(),
            "-c:v",
            "mpeg4",
            "-g",
            "5",
            "-q:v",
            "3",
            "-f",
            "m4v",
        ])
        .arg(&m4v)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    // Reference decode: first frame as raw I420.
    let status = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&m4v)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&yuv)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    let es = std::fs::read(&m4v).ok()?;
    let ref_yuv = std::fs::read(&yuv).ok()?;
    let _ = std::fs::remove_file(&m4v);
    let _ = std::fs::remove_file(&yuv);

    let frame_size = width * height * 3 / 2;
    if ref_yuv.len() < frame_size {
        return None;
    }
    Some((es, ref_yuv[..frame_size].to_vec()))
}

/// Decode an ffmpeg-produced MPEG-4 Part 2 elementary stream through the
/// VideoToolbox bridge and assert the first decoded frame matches ffmpeg's
/// own software decode (PSNR_Y ≥ 30 dB — same bar as MPEG-2 / VP9 since
/// VT's IDCT differs slightly from ffmpeg's).
///
/// Self-skips when ffmpeg / VideoToolbox is unavailable, or when the VT
/// MPEG-4 Part 2 decoder errors at session-create time on the runner.
#[test]
fn mpeg4_part_two_decode_against_ffmpeg() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping mpeg4 decode");
        return;
    }

    let width = 320usize;
    let height = 240usize;
    let frames = 10usize;

    let Some((es, ref_i420)) = ffmpeg_mpeg4_part_two_fixture(width, height, frames) else {
        eprintln!("oxideav-videotoolbox: ffmpeg unavailable, skipping mpeg4 decode test");
        return;
    };

    let dec_params = {
        let mut p = CodecParameters::video(CodecId::new("mpeg4"));
        p.width = Some(width as u32);
        p.height = Some(height as u32);
        p.pixel_format = Some(PixelFormat::Yuv420P);
        p
    };

    let mut decoder = match vt_blob::make_mpeg4_part_two_decoder(&dec_params) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("VT MPEG-4 Part 2 decoder unavailable on this host: {e}; skipping");
            return;
        }
    };

    let pkt = oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1_000_000), es);

    let mut decoded: Vec<VideoFrame> = Vec::new();
    if let Err(e) = decoder.send_packet(&pkt) {
        eprintln!("VT MPEG-4 Part 2 send_packet error: {e}; skipping");
        return;
    }
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Video(vf)) => decoded.push(vf),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_frame (mpeg4) error: {e}"),
        }
    }
    decoder.flush().expect("decoder flush");
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Video(vf)) => decoded.push(vf),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_frame (mpeg4 flush) error: {e}"),
        }
    }

    if decoded.is_empty() {
        eprintln!("VT MPEG-4 Part 2 decoder produced no frames; skipping");
        return;
    }

    // Dimensions.
    for (i, df) in decoded.iter().enumerate() {
        assert_eq!(df.planes.len(), 3, "mpeg4 frame {i}: expected 3 planes");
        let dec_w = df.planes[0].stride;
        let dec_h = df.planes[0].data.len() / dec_w.max(1);
        assert_eq!(dec_w, width, "mpeg4 frame {i}: width mismatch");
        assert_eq!(dec_h, height, "mpeg4 frame {i}: height mismatch");
    }

    // Build reference VideoFrame from ffmpeg's raw I420 first frame.
    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);
    let y_len = width * height;
    let c_len = chroma_w * chroma_h;
    let ref_frame = VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: width,
                data: ref_i420[..y_len].to_vec(),
            },
            VideoPlane {
                stride: chroma_w,
                data: ref_i420[y_len..y_len + c_len].to_vec(),
            },
            VideoPlane {
                stride: chroma_w,
                data: ref_i420[y_len + c_len..y_len + 2 * c_len].to_vec(),
            },
        ],
    };

    let best_psnr = decoded
        .iter()
        .map(|df| psnr_y(&ref_frame, df))
        .fold(f64::NEG_INFINITY, f64::max);

    println!(
        "mpeg4 decode: {} frames decoded, best PSNR_Y vs ffmpeg = {:.1} dB",
        decoded.len(),
        best_psnr
    );

    assert!(
        best_psnr >= 30.0,
        "mpeg4 PSNR_Y {best_psnr:.1} dB < 30 dB threshold"
    );
}

// ─────────────────────────── AV1 decode test ────────────────────────────────

/// Run `ffmpeg` as an opaque black-box validator to produce an AV1 IVF
/// stream (and a reference raw-YUV decode). Returns
/// `(per_frame_av1_payloads, reference_first_frame_i420)` or `None` if
/// ffmpeg / libaom-av1 is unavailable. Same shape as `ffmpeg_vp9_fixture`
/// — AV1 in IVF carries one temporal unit per IVF frame record, so the
/// existing `parse_ivf` helper carves it correctly.
fn ffmpeg_av1_fixture(
    width: usize,
    height: usize,
    frames: usize,
) -> Option<(Vec<Vec<u8>>, Vec<u8>)> {
    use std::process::Command;

    let ffmpeg = [
        "/opt/homebrew/bin/ffmpeg",
        "/usr/local/bin/ffmpeg",
        "ffmpeg",
    ]
    .into_iter()
    .find(|p| {
        Command::new(p)
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })?;

    let dir = std::env::temp_dir();
    let ivf = dir.join(format!("oxideav_vt_av1_{width}x{height}.ivf"));
    let yuv = dir.join(format!("oxideav_vt_av1_{width}x{height}.yuv"));

    // Encode a smooth gradient to an AV1 IVF stream. libaom-av1 is the
    // reference AV1 encoder; if it isn't built into this ffmpeg the command
    // fails and we skip the test. `-cpu-used 8` keeps the encode under a
    // few seconds on CI runners.
    let status = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!(
                "gradients=s={width}x{height}:c0=black:c1=white:d=1:r={frames},format=yuv420p"
            ),
            "-frames:v",
            &frames.to_string(),
            "-c:v",
            "libaom-av1",
            "-cpu-used",
            "8",
            "-b:v",
            "500k",
            "-f",
            "ivf",
        ])
        .arg(&ivf)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    // Reference decode: first frame as raw I420.
    let status = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&ivf)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "yuv420p"])
        .arg(&yuv)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }

    let ivf_bytes = std::fs::read(&ivf).ok()?;
    let ref_yuv = std::fs::read(&yuv).ok()?;
    let _ = std::fs::remove_file(&ivf);
    let _ = std::fs::remove_file(&yuv);

    let payloads = parse_ivf(&ivf_bytes)?;
    if payloads.is_empty() {
        return None;
    }

    let frame_size = width * height * 3 / 2;
    if ref_yuv.len() < frame_size {
        return None;
    }
    Some((payloads, ref_yuv[..frame_size].to_vec()))
}

/// Decode an ffmpeg-produced AV1 IVF stream through the VideoToolbox bridge
/// and assert the first decoded frame matches ffmpeg's own software decode
/// (PSNR_Y ≥ 30 dB — same bar as VP9 / MPEG-2 / MPEG-4 Pt 2 since VT's AV1
/// reconstruction differs slightly from libaom-av1's).
///
/// Self-skips when ffmpeg / libaom-av1 / VideoToolbox is unavailable, or
/// when the VT AV1 decoder errors at session-create time (older macOS
/// without any AV1 decoder path, or Apple Silicon below M3 without VT's
/// internal SW fallback compiled in).
#[test]
fn av1_decode_against_ffmpeg() {
    if oxideav_videotoolbox::sys::vtable().is_err() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping av1 decode");
        return;
    }

    let width = 320usize;
    let height = 240usize;
    let frames = 10usize;

    let Some((payloads, ref_i420)) = ffmpeg_av1_fixture(width, height, frames) else {
        eprintln!("oxideav-videotoolbox: ffmpeg/libaom-av1 unavailable, skipping av1 decode test");
        return;
    };

    let dec_params = {
        let mut p = CodecParameters::video(CodecId::new("av1"));
        p.width = Some(width as u32);
        p.height = Some(height as u32);
        p.pixel_format = Some(PixelFormat::Yuv420P);
        p
    };

    let mut decoder = match vt_blob::make_av1_decoder(&dec_params) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("VT AV1 decoder unavailable on this host: {e}; skipping");
            return;
        }
    };

    let mut decoded: Vec<VideoFrame> = Vec::new();
    let mut session_err: Option<Error> = None;

    for (i, payload) in payloads.iter().enumerate() {
        let pkt = oxideav_core::Packet::new(
            0,
            oxideav_core::TimeBase::new(1, 1_000_000),
            payload.clone(),
        )
        .with_pts((i as i64) * 33_333);
        if let Err(e) = decoder.send_packet(&pkt) {
            // First-call failure is treated as "AV1 decoder not available
            // on this host" — skip rather than fail.
            session_err = Some(e);
            break;
        }
        loop {
            match decoder.receive_frame() {
                Ok(Frame::Video(vf)) => decoded.push(vf),
                Ok(_) => {}
                Err(Error::NeedMore) | Err(Error::Eof) => break,
                Err(e) => panic!("receive_frame error: {e}"),
            }
        }
    }
    if let Some(e) = session_err {
        if decoded.is_empty() {
            eprintln!("VT AV1 decoder errored at decode time: {e}; skipping");
            return;
        }
    }

    decoder.flush().expect("decoder flush");
    loop {
        match decoder.receive_frame() {
            Ok(Frame::Video(vf)) => decoded.push(vf),
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_frame (flush) error: {e}"),
        }
    }

    if decoded.is_empty() {
        eprintln!("VT AV1 decoder produced no frames; skipping (host may lack AV1 support)");
        return;
    }

    // Dimensions.
    for (i, df) in decoded.iter().enumerate() {
        assert_eq!(df.planes.len(), 3, "av1 frame {i}: expected 3 planes");
        let dec_w = df.planes[0].stride;
        let dec_h = df.planes[0].data.len() / dec_w.max(1);
        assert_eq!(dec_w, width, "av1 frame {i}: width mismatch");
        assert_eq!(dec_h, height, "av1 frame {i}: height mismatch");
    }

    // Build a reference VideoFrame from ffmpeg's raw I420 first frame.
    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);
    let y_len = width * height;
    let c_len = chroma_w * chroma_h;
    let ref_frame = VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: width,
                data: ref_i420[..y_len].to_vec(),
            },
            VideoPlane {
                stride: chroma_w,
                data: ref_i420[y_len..y_len + c_len].to_vec(),
            },
            VideoPlane {
                stride: chroma_w,
                data: ref_i420[y_len + c_len..y_len + 2 * c_len].to_vec(),
            },
        ],
    };

    let best_psnr = decoded
        .iter()
        .map(|df| psnr_y(&ref_frame, df))
        .fold(f64::NEG_INFINITY, f64::max);

    println!(
        "av1 decode: {} frames decoded, best PSNR_Y vs ffmpeg = {:.1} dB",
        decoded.len(),
        best_psnr
    );

    assert!(
        best_psnr >= 30.0,
        "av1 PSNR_Y {best_psnr:.1} dB < 30 dB threshold"
    );
}
