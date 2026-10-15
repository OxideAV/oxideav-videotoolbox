//! Every codec the VideoToolbox decoder claims, gated the same way
//! against a black-box producer: frame count == pictures, every frame
//! has the plane geometry of `output_pixel_format()`, the re-ordered tail
//! arrives at `flush`, timestamps come back monotone in display order,
//! and the pictures match the black-box reference decode (PSNR ≥ 40 dB
//! in order — the lossy codecs' own IDCT / loop-filter differences keep
//! this a touch below the lossless-layout bar).
//!
//! Sources are produced at test time (H.264 / HEVC / MPEG-2 / MPEG-4
//! Part 2 / VP9 / AV1 by the black-box encoders; MJPEG and ProRes by the
//! crate's own VideoToolbox encoders). A codec whose producer or hardware
//! is absent reports "unavailable" and passes; VVC has no local producer
//! and is reported as such.

#![cfg(all(target_os = "macos", feature = "registry"))]

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use oxideav_core::{
    CodecId, CodecParameters, Decoder, Encoder, Error, Frame, PixelFormat, TimeBase, VideoFrame,
    VideoPlane,
};
use oxideav_videotoolbox::{blob, decoder};

const W: u32 = 96;
const H: u32 = 72;
const FRAMES: usize = 12;

/// Encode the test pattern with a black-box encoder into `ext` (an
/// explicit raw-stream muxer when the extension alone would pick a
/// container: `.m4v` is an MP4 brand, not an MPEG-4 elementary stream).
fn produce(ffmpeg: &Path, encoder: &str, extra: &[&str], ext: &str) -> Option<Vec<u8>> {
    let out = scratch("sweep").join(format!("{encoder}.{ext}"));
    let muxer: &[&str] = match ext {
        "m4v" => &["-f", "m4v"],
        "m2v" => &["-f", "mpeg2video"],
        _ => &[],
    };
    let ok = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!("testsrc2=size={W}x{H}:rate=25"))
        .args([
            "-frames:v",
            &FRAMES.to_string(),
            "-c:v",
            encoder,
            "-pix_fmt",
            "yuv420p",
        ])
        .args(extra)
        .args(muxer)
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    std::fs::read(&out).ok()
}

fn reference(ffmpeg: &Path, encoder: &str, ext: &str) -> Vec<u8> {
    let path = scratch("sweep").join(format!("{encoder}.{ext}"));
    reference_decode(ffmpeg, &path, "yuv420p").expect("reference decode")
}

fn gate(
    tag: &str,
    dec: &mut dyn Decoder,
    pkts: &[oxideav_core::Packet],
    reference: Option<&[u8]>,
    expect_reorder_tail: bool,
) {
    let got = match decode_all(dec, pkts) {
        Ok(g) => g,
        Err(Error::Unsupported(e)) => {
            eprintln!("{tag}: unavailable on this host ({e})");
            return;
        }
        Err(e) => panic!("{tag}: {e}"),
    };
    let label = dec
        .output_pixel_format()
        .unwrap_or_else(|| panic!("{tag}: label"));
    let all = got.all();
    assert_eq!(all.len(), FRAMES, "{tag}: frame count");
    if expect_reorder_tail {
        assert!(
            !got.from_flush.is_empty(),
            "{tag}: re-ordered tail at flush"
        );
    }
    for (i, f) in all.iter().enumerate() {
        assert_geometry(f, label, W, H, &format!("{tag}: frame {i}"));
    }
    let stamps: Vec<i64> = all.iter().filter_map(|f| f.pts).collect();
    if !stamps.is_empty() {
        assert_eq!(stamps.len(), FRAMES, "{tag}: every frame stamped");
        assert!(
            stamps.windows(2).all(|w| w[0] < w[1]),
            "{tag}: stamps not strictly increasing in display order: {stamps:?}"
        );
    }
    if let Some(reference) = reference {
        let bytes = picture_bytes(label, W, H);
        assert_eq!(reference.len(), bytes * FRAMES, "{tag}: reference size");
        for (i, f) in all.iter().enumerate() {
            let p = psnr(&packed(f), &reference[i * bytes..(i + 1) * bytes]);
            assert!(p >= 40.0, "{tag}: frame {i} PSNR {p:.1} dB < 40 dB");
        }
    }
    println!(
        "{tag}: {FRAMES} frames as {label:?}, {} released at flush",
        got.from_flush.len()
    );
}

fn params(id: &str, label: Option<PixelFormat>) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(id));
    p.width = Some(W);
    p.height = Some(H);
    p.pixel_format = label;
    p
}

fn skip_or(result: Result<Box<dyn Decoder>, Error>, tag: &str) -> Option<Box<dyn Decoder>> {
    match result {
        Ok(d) => Some(d),
        Err(Error::Unsupported(e)) => {
            eprintln!("{tag}: decoder unavailable on this host ({e})");
            None
        }
        Err(e) => panic!("{tag}: {e}"),
    }
}

/// Annex-B H.264 / HEVC: one access unit per packet, no stamps (raw
/// byte-stream shape), then with display stamps derived from the
/// reference order is not possible here, so stamps are checked through
/// the IVF codecs below.
fn nal_codec(ffmpeg: &Path, codec: decoder::NalCodec, encoder: &str, ext: &str, extra: &[&str]) {
    let tag = encoder.to_string();
    if !ffmpeg_has_encoder(ffmpeg, encoder) {
        eprintln!("{tag}: producer unavailable");
        return;
    }
    let Some(es) = produce(ffmpeg, encoder, extra, ext) else {
        eprintln!("{tag}: producer failed");
        return;
    };
    let reference = reference(ffmpeg, encoder, ext);
    let id = match codec {
        decoder::NalCodec::H264 => "h264",
        decoder::NalCodec::Hevc => "hevc",
    };
    let Some(mut dec) = skip_or(decoder::NalVtDecoder::make(codec, &params(id, None)), &tag) else {
        return;
    };
    let nals = decoder::annex_b_nals(&es);
    let units: Vec<Vec<u8>> = decoder::split_access_units(codec, &nals)
        .into_iter()
        .map(|u| {
            let mut b = Vec::new();
            for n in u {
                b.extend_from_slice(&[0, 0, 0, 1]);
                b.extend_from_slice(n);
            }
            b
        })
        .collect();
    assert_eq!(units.len(), FRAMES, "{tag}: access units");
    gate(
        &tag,
        dec.as_mut(),
        &packets(units, None, TimeBase::new(1, 25)),
        Some(&reference),
        true,
    );
}

#[test]
fn h264_and_hevc_sweep() {
    if !vt_available() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping sweep");
        return;
    }
    let Some(ffmpeg) = tool("ffmpeg") else {
        eprintln!("oxideav-videotoolbox: ffmpeg unavailable, skipping sweep");
        return;
    };
    nal_codec(
        &ffmpeg,
        decoder::NalCodec::H264,
        "libx264",
        "h264",
        &["-bf", "3", "-g", "6"],
    );
    nal_codec(
        &ffmpeg,
        decoder::NalCodec::Hevc,
        "libx265",
        "hevc",
        &[
            "-x265-params",
            "log-level=error:bframes=3:keyint=6:repeat-headers=1",
        ],
    );
}

/// Elementary-stream codecs with an in-crate framer: the whole stream in
/// one packet (the framer carves the pictures).
#[test]
fn mpeg2_and_mpeg4_part2_sweep() {
    if !vt_available() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping sweep");
        return;
    }
    let Some(ffmpeg) = tool("ffmpeg") else {
        eprintln!("oxideav-videotoolbox: ffmpeg unavailable, skipping sweep");
        return;
    };
    for (encoder, ext, id, factory) in [
        (
            "mpeg2video",
            "m2v",
            "mpeg2video",
            blob::make_mpeg2_decoder as fn(&CodecParameters) -> Result<Box<dyn Decoder>, Error>,
        ),
        ("mpeg4", "m4v", "mpeg4", blob::make_mpeg4_part_two_decoder),
    ] {
        if !ffmpeg_has_encoder(&ffmpeg, encoder) {
            eprintln!("{encoder}: producer unavailable");
            continue;
        }
        // Intra + P only: the elementary-stream path emits in decode order.
        let Some(es) = produce(&ffmpeg, encoder, &["-bf", "0", "-g", "6"], ext) else {
            eprintln!("{encoder}: producer failed");
            continue;
        };
        let reference = reference(&ffmpeg, encoder, ext);
        let Some(mut dec) = skip_or(factory(&params(id, None)), encoder) else {
            continue;
        };
        gate(
            encoder,
            dec.as_mut(),
            &packets(vec![es], None, TimeBase::new(1, 25)),
            Some(&reference),
            false,
        );
    }
}

/// Container-framed codecs: one IVF frame per packet with its timestamp.
#[test]
fn vp9_and_av1_sweep() {
    if !vt_available() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping sweep");
        return;
    }
    let Some(ffmpeg) = tool("ffmpeg") else {
        eprintln!("oxideav-videotoolbox: ffmpeg unavailable, skipping sweep");
        return;
    };
    for (encoder, id, factory, extra) in [
        (
            "libvpx-vp9",
            "vp9",
            blob::make_vp9_decoder as fn(&CodecParameters) -> Result<Box<dyn Decoder>, Error>,
            &["-g", "6"][..],
        ),
        (
            "libsvtav1",
            "av1",
            blob::make_av1_decoder,
            &["-g", "6", "-preset", "10"][..],
        ),
        (
            "libaom-av1",
            "av1",
            blob::make_av1_decoder,
            &["-g", "6", "-cpu-used", "8"][..],
        ),
    ] {
        if !ffmpeg_has_encoder(&ffmpeg, encoder) {
            eprintln!("{encoder}: producer unavailable");
            continue;
        }
        let Some(ivf) = produce(&ffmpeg, encoder, extra, "ivf") else {
            eprintln!("{encoder}: producer failed");
            continue;
        };
        let reference = reference(&ffmpeg, encoder, "ivf");
        let frames = parse_ivf(&ivf).expect("ivf");
        assert_eq!(frames.len(), FRAMES, "{encoder}: IVF frames");
        let (stamps, payloads): (Vec<i64>, Vec<Vec<u8>>) = frames.into_iter().unzip();
        let Some(mut dec) = skip_or(factory(&params(id, None)), encoder) else {
            continue;
        };
        gate(
            encoder,
            dec.as_mut(),
            &packets(payloads, Some(&stamps), TimeBase::new(1, 25)),
            Some(&reference),
            false,
        );
    }
}

/// MJPEG and ProRes through the crate's own encoders, checking the
/// decoder's default labels: MJPEG `YuvJ420P` (the SOF decides), ProRes
/// 422 `Yuv422P10Le` and ProRes 4444 `Yuv444P16Le` when the container
/// declares nothing, and the declared label when it does.
#[test]
fn mjpeg_and_prores_sweep() {
    if !vt_available() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping sweep");
        return;
    }
    let source: Vec<VideoFrame> = (0..FRAMES)
        .map(|i| {
            let w = W as usize;
            let h = H as usize;
            let y: Vec<u8> = (0..w * h)
                .map(|p| (((p % w) * 200 / w) + ((p / w) * 40 / h) + i * 3) as u8)
                .collect();
            VideoFrame {
                pts: Some(i as i64 * 40_000),
                planes: vec![
                    VideoPlane { stride: w, data: y },
                    VideoPlane {
                        stride: w / 2,
                        data: vec![(100 + i * 4) as u8; w * h / 4],
                    },
                    VideoPlane {
                        stride: w / 2,
                        data: vec![(150 - i * 4) as u8; w * h / 4],
                    },
                ],
            }
        })
        .collect();
    let encode = |enc: &mut dyn Encoder| -> Vec<oxideav_core::Packet> {
        let mut out = Vec::new();
        for f in &source {
            enc.send_frame(&Frame::Video(f.clone()))
                .expect("send_frame");
            while let Ok(p) = enc.receive_packet() {
                out.push(p);
            }
        }
        enc.flush().expect("flush");
        while let Ok(p) = enc.receive_packet() {
            out.push(p);
        }
        out
    };

    // MJPEG: the encoder emits 4:2:0 scans; undeclared → `YuvJ420P`.
    let enc_params = params("mjpeg", Some(PixelFormat::Yuv420P));
    if let Ok(mut enc) = blob::make_jpeg_encoder(&enc_params) {
        let pkts = encode(enc.as_mut());
        assert_eq!(pkts.len(), FRAMES);
        let sof = blob::jpeg_sof_label(&pkts[0].data);
        assert_eq!(
            sof,
            Some(PixelFormat::YuvJ420P),
            "VT JPEG encoder writes 4:2:0 scans"
        );
        for declared in [
            None,
            Some(PixelFormat::YuvJ420P),
            Some(PixelFormat::Yuv420P),
        ] {
            let tag = format!("mjpeg declared={declared:?}");
            if let Some(mut dec) =
                skip_or(blob::make_jpeg_decoder(&params("mjpeg", declared)), &tag)
            {
                gate(&tag, dec.as_mut(), &pkts, None, false);
                assert_eq!(
                    dec.output_pixel_format(),
                    Some(declared.unwrap_or(PixelFormat::YuvJ420P))
                );
            }
        }
    }

    // ProRes 422 (default flavour) and 4444.
    for (fourcc, default_label) in [
        (b"apcn", PixelFormat::Yuv422P10Le),
        (b"ap4h", PixelFormat::Yuv444P16Le),
    ] {
        let mut enc_params = params("prores", Some(PixelFormat::Yuv420P));
        enc_params.tag = Some(oxideav_core::CodecTag::fourcc(fourcc));
        let Some(mut enc) = blob::make_prores_encoder(&enc_params).ok() else {
            eprintln!(
                "prores {}: encoder unavailable",
                String::from_utf8_lossy(fourcc)
            );
            continue;
        };
        let pkts = encode(enc.as_mut());
        assert_eq!(pkts.len(), FRAMES);
        for declared in [
            None,
            Some(PixelFormat::Yuv420P),
            Some(PixelFormat::Yuv422P),
            Some(default_label),
        ] {
            let tag = format!(
                "prores {} declared={declared:?}",
                String::from_utf8_lossy(fourcc)
            );
            let mut p = params("prores", declared);
            p.tag = Some(oxideav_core::CodecTag::fourcc(fourcc));
            if let Some(mut dec) = skip_or(blob::make_prores_decoder(&p), &tag) {
                gate(&tag, dec.as_mut(), &pkts, None, false);
                assert_eq!(
                    dec.output_pixel_format(),
                    Some(declared.unwrap_or(default_label)),
                    "{tag}"
                );
            }
        }
    }
}

#[test]
fn vvc_has_no_local_producer() {
    // VideoToolbox's VVC decode is claimed (M3+, macOS 26+) but no black-box
    // VVC encoder is available on the hosts this suite runs on; the claim
    // stays unmeasured here and is reported, not asserted.
    if !vt_available() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping");
        return;
    }
    match tool("ffmpeg") {
        Some(f) if ffmpeg_has_encoder(&f, "libvvenc") => {
            eprintln!("vvc: a producer exists on this host — extend the sweep");
        }
        _ => eprintln!("vvc: no local producer; decode claim unmeasured"),
    }
}
