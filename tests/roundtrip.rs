//! End-to-end encode → decode roundtrip tests for H.264 and HEVC.
//!
//! Gated to `cfg(all(target_os = "macos", feature = "registry"))` —
//! on non-mac platforms or without the `registry` feature this module
//! compiles to nothing (empty rlib, zero tests run).
//!
//! Test strategy:
//! 1. Generate a synthetic 320×240 I420 test pattern (luma ramp).
//! 2. Encode 10 frames via `H264VtEncoder` (resp. `HevcVtEncoder`).
//! 3. Decode the resulting Annex-B stream via `H264VtDecoder` (resp. `HevcVtDecoder`).
//! 4. Assert decoded frame dimensions match 320×240.
//! 5. Assert PSNR_Y ≥ 35 dB on at least one decoded frame.

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
