//! Mid-stream shape changes through the VideoToolbox "blob" decoders
//! (MJPEG, ProRes, MPEG-2, MPEG-4 Part 2, VP9, AV1): the core
//! output-description contract (`output_params`,
//! `output_params_generation`, `Frame::StreamParams`) on streams spliced
//! from segments of different picture sizes.
//!
//! Segments come from the black-box encoders (ffmpeg) at run time, or
//! from this crate's own VideoToolbox encoders (ProRes); the decoder is
//! constructed with the *first* segment's container size, as a demuxer
//! would. Gates: the first segment's description before the first
//! frame, one event per change at the boundary with payload ==
//! `output_params()`, matching plane geometry, and pictures identical
//! to each segment's solo decode, in order.
//!
//! Passes by reporting "unavailable" when the framework, a hardware
//! codec or a producer is absent.

#![cfg(all(target_os = "macos", feature = "registry"))]

mod common;

use std::path::Path;
use std::process::Command;

use common::shape::*;
use common::*;
use oxideav_core::{
    CodecId, CodecParameters, Decoder, Error, Frame, Packet, PixelFormat, TimeBase, VideoFrame,
    VideoPlane,
};
use oxideav_videotoolbox::blob;

type Factory = fn(&CodecParameters) -> Result<Box<dyn Decoder>, Error>;

const SIZES: [(u32, u32); 3] = [(64, 64), (128, 96), (64, 64)];
const FRAMES: usize = 6;

/// One segment at `w × h` from a black-box encoder, as packets.
fn produce(
    ffmpeg: &Path,
    encoder: &str,
    extra: &[&str],
    (w, h): (u32, u32),
    ext: &str,
) -> Option<Vec<Packet>> {
    let dir = scratch("stream-params");
    let out = dir.join(format!("{encoder}-{w}x{h}.{ext}"));
    let muxer: &[&str] = match ext {
        "m4v" => &["-f", "m4v"],
        "m2v" => &["-f", "mpeg2video"],
        "mjpeg" => &["-f", "mjpeg"],
        _ => &[],
    };
    let ok = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={w}x{h}:rate=25"))
        .args(["-frames:v", &FRAMES.to_string(), "-c:v", encoder])
        .args(extra)
        .args(muxer)
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    let bytes = std::fs::read(&out).ok()?;
    let tb = TimeBase::new(1, 25);
    Some(match ext {
        "ivf" => parse_ivf(&bytes)?
            .into_iter()
            .map(|(_, data)| Packet::new(0, tb, data))
            .collect(),
        "mjpeg" => split_jpegs(&bytes)
            .into_iter()
            .map(|data| Packet::new(0, tb, data))
            .collect(),
        // Elementary streams: one packet per segment (the decoder's own
        // framer carves the pictures).
        _ => vec![Packet::new(0, tb, bytes)],
    })
}

/// Split concatenated JPEG images on their SOI markers.
fn split_jpegs(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut starts: Vec<usize> = (0..buf.len().saturating_sub(2))
        .filter(|&i| buf[i] == 0xFF && buf[i + 1] == 0xD8 && buf[i + 2] == 0xFF)
        .collect();
    starts.push(buf.len());
    starts
        .windows(2)
        .map(|w| buf[w[0]..w[1]].to_vec())
        .collect()
}

fn params(id: &str, (w, h): (u32, u32), tag: Option<&[u8; 4]>) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(id));
    p.width = Some(w);
    p.height = Some(h);
    p.tag = tag.map(oxideav_core::CodecTag::fourcc);
    p
}

fn make(factory: Factory, p: &CodecParameters, tag: &str) -> Option<Box<dyn Decoder>> {
    match factory(p) {
        Ok(d) => Some(d),
        Err(Error::Unsupported(e)) => {
            eprintln!("{tag}: VideoToolbox decoder unavailable ({e})");
            None
        }
        Err(e) => panic!("{tag}: construction failed: {e}"),
    }
}

/// What a spliced decode must do at the size switch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AtSwitch {
    /// Re-configure and announce it (the contract).
    Announce,
    /// Refuse with an error, never a mislabelled frame: the bridge has
    /// no parse of the new picture size for this codec yet (MPEG-4 Part
    /// 2 VOL, ProRes frame header). Every picture of the first segment
    /// still comes out first.
    Refuse,
}

/// One single-shape segment of a spliced stream.
struct Seg {
    size: (u32, u32),
    label: PixelFormat,
    packets: Vec<Packet>,
}

/// Decode `segments` back to back with a decoder built for the first
/// segment's size, and each segment alone, then gate the contract.
fn splice(
    tag: &str,
    id: &str,
    fourcc: Option<&[u8; 4]>,
    factory: Factory,
    segments: &[Seg],
    at_switch: AtSwitch,
) {
    let mut solos = Vec::new();
    let mut expects = Vec::new();
    for Seg {
        size,
        label,
        packets: pkts,
    } in segments
    {
        let Some(mut dec) = make(factory, &params(id, *size, fourcc), tag) else {
            return;
        };
        let frames = match try_run(dec.as_mut(), pkts, true) {
            Ok(out) => pictures(tag, out),
            Err(Error::Unsupported(e)) => {
                eprintln!("{tag}: VideoToolbox decoder unavailable at {size:?} ({e})");
                return;
            }
            Err(e) => panic!("{tag}: solo decode at {size:?}: {e}"),
        };
        assert!(!frames.is_empty(), "{tag}: {size:?} decodes");
        expects.push(Expect {
            width: size.0,
            height: size.1,
            label: *label,
            pictures: frames.len(),
        });
        solos.push(frames);
    }
    let all: Vec<Packet> = segments.iter().flat_map(|s| s.packets.clone()).collect();

    let first = params(id, segments[0].size, fourcc);
    if at_switch == AtSwitch::Refuse {
        let mut dec = make(factory, &first, tag).unwrap();
        let mut out = Vec::new();
        let mut refused = false;
        for p in &all {
            if dec.send_packet(p).is_err() || try_drain(dec.as_mut(), &mut out).is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused, "{tag}: the size switch is refused");
        let got = pictures(tag, out);
        assert_eq!(got.len(), expects[0].pictures, "{tag}: first segment out");
        for (i, f) in got.iter().enumerate() {
            assert!(
                packed(f) == packed(&solos[0][i]),
                "{tag}: picture {i} of the first segment"
            );
        }
        assert_eq!(dec.output_params_generation(), 0);
        return;
    }
    let mut dec = make(factory, &first, tag).unwrap();
    // Before any packet: the container's size (the label too, unless
    // the first picture's header decides it).
    let p = dec
        .output_params()
        .expect("output_params before the first packet");
    assert_eq!(
        (p.width, p.height),
        (Some(expects[0].width), Some(expects[0].height)),
        "{tag}: container size stated up front"
    );
    // After the first packet, before any frame is returned: complete.
    dec.send_packet(&all[0]).expect("first packet");
    let p = dec
        .output_params()
        .expect("output_params before the first frame");
    assert!(
        same_params(&p, &expects[0]),
        "{tag}: predicted {:?}x{:?} {:?}",
        p.width,
        p.height,
        p.pixel_format
    );
    let mut out = Vec::new();
    drain(dec.as_mut(), &mut out);
    out.extend(run(dec.as_mut(), &all[1..], true));
    check(&format!("{tag} interleaved"), &out, &expects, &solos);

    let mut dec = make(factory, &first, tag).unwrap();
    let out = run(dec.as_mut(), &all, false);
    check(&format!("{tag} drained at flush"), &out, &expects, &solos);

    // An elementary stream handed over in one packet: the framer carves
    // the pictures and the sequence headers between them.
    if all.len() == segments.len() && segments.iter().all(|s| s.packets.len() == 1) {
        let one: Vec<u8> = all.iter().flat_map(|p| p.data.clone()).collect();
        let mut dec = make(factory, &first, tag).unwrap();
        let out = run(
            dec.as_mut(),
            &[Packet::new(0, TimeBase::new(1, 25), one)],
            true,
        );
        check(&format!("{tag} one packet"), &out, &expects, &solos);
    }
}

fn ffmpeg_splice(
    encoder: &str,
    extra: &[&str],
    ext: &str,
    id: &str,
    factory: Factory,
    label: PixelFormat,
    at_switch: AtSwitch,
) {
    if !vt_available() {
        eprintln!("VideoToolbox framework unavailable");
        return;
    }
    let Some(ffmpeg) = tool("ffmpeg") else {
        eprintln!("{encoder}: ffmpeg unavailable");
        return;
    };
    if !ffmpeg_has_encoder(&ffmpeg, encoder) {
        eprintln!("{encoder}: producer unavailable");
        return;
    }
    let mut segments = Vec::new();
    for size in SIZES {
        let Some(p) = produce(&ffmpeg, encoder, extra, size, ext) else {
            eprintln!("{encoder}: producer failed at {size:?}");
            return;
        };
        segments.push(Seg {
            size,
            label,
            packets: p,
        });
    }
    splice(encoder, id, None, factory, &segments, at_switch);
}

#[test]
fn mjpeg_size_switch() {
    ffmpeg_splice(
        "mjpeg",
        &["-pix_fmt", "yuvj420p"],
        "mjpeg",
        "mjpeg",
        blob::make_jpeg_decoder,
        PixelFormat::YuvJ420P,
        AtSwitch::Announce,
    );
}

#[test]
fn mpeg2_size_switch() {
    ffmpeg_splice(
        "mpeg2video",
        &["-pix_fmt", "yuv420p", "-bf", "0", "-g", "3"],
        "m2v",
        "mpeg2video",
        blob::make_mpeg2_decoder,
        PixelFormat::Yuv420P,
        AtSwitch::Announce,
    );
}

/// MPEG-4 Part 2: a new VOL re-builds the `esds` record, but the session
/// also needs the VOL's picture size (not parsed here yet), so the
/// switch is refused rather than mislabelled.
#[test]
fn mpeg4_part2_size_switch_refused_without_vol_size() {
    ffmpeg_splice(
        "mpeg4",
        &["-pix_fmt", "yuv420p", "-bf", "0", "-g", "3"],
        "m4v",
        "mpeg4",
        blob::make_mpeg4_part_two_decoder,
        PixelFormat::Yuv420P,
        AtSwitch::Refuse,
    );
}

#[test]
fn vp9_size_switch() {
    ffmpeg_splice(
        "libvpx-vp9",
        &["-pix_fmt", "yuv420p", "-g", "3"],
        "ivf",
        "vp9",
        blob::make_vp9_decoder,
        PixelFormat::Yuv420P,
        AtSwitch::Announce,
    );
}

#[test]
fn av1_size_switch() {
    ffmpeg_splice(
        "libsvtav1",
        &["-pix_fmt", "yuv420p", "-g", "3", "-preset", "10"],
        "ivf",
        "av1",
        blob::make_av1_decoder,
        PixelFormat::Yuv420P,
        AtSwitch::Announce,
    );
}

/// ProRes 422 segments from the crate's own VideoToolbox encoder. The
/// session's picture size comes from the container; a stream switching
/// size needs the frame header's size (not parsed here yet), so the
/// switch is refused rather than mislabelled.
#[test]
fn prores_size_switch_refused_without_frame_header_size() {
    if !vt_available() {
        eprintln!("VideoToolbox framework unavailable");
        return;
    }
    let mut segments = Vec::new();
    for (w, h) in SIZES {
        let mut p = params("prores", (w, h), Some(b"apcn"));
        p.pixel_format = Some(PixelFormat::Yuv420P);
        let Ok(mut enc) = blob::make_prores_encoder(&p) else {
            eprintln!("prores: encoder unavailable");
            return;
        };
        let (wu, hu) = (w as usize, h as usize);
        let mut pkts = Vec::new();
        for i in 0..FRAMES {
            let y: Vec<u8> = (0..wu * hu)
                .map(|p| ((p % wu) * 200 / wu + i * 5) as u8)
                .collect();
            let f = VideoFrame {
                pts: Some(i as i64),
                planes: vec![
                    VideoPlane {
                        stride: wu,
                        data: y,
                    },
                    VideoPlane {
                        stride: wu / 2,
                        data: vec![110; wu * hu / 4],
                    },
                    VideoPlane {
                        stride: wu / 2,
                        data: vec![140; wu * hu / 4],
                    },
                ],
            };
            enc.send_frame(&Frame::Video(f)).expect("send_frame");
            while let Ok(p) = enc.receive_packet() {
                pkts.push(p);
            }
        }
        enc.flush().expect("flush");
        while let Ok(p) = enc.receive_packet() {
            pkts.push(p);
        }
        segments.push(Seg {
            size: (w, h),
            label: PixelFormat::Yuv422P10Le,
            packets: pkts,
        });
    }
    splice(
        "prores",
        "prores",
        Some(b"apcn"),
        blob::make_prores_decoder,
        &segments,
        AtSwitch::Refuse,
    );
}

/// MJPEG switching between colour 4:2:0 and greyscale scans (and size):
/// each switch re-negotiates the label and is announced.
#[test]
fn mjpeg_layout_switch() {
    if !vt_available() {
        eprintln!("VideoToolbox framework unavailable");
        return;
    }
    let Some(magick) = tool("magick") else {
        eprintln!("mjpeg layouts: magick unavailable");
        return;
    };
    let dir = scratch("stream-params");
    let cells: [((u32, u32), PixelFormat, &[&str]); 4] = [
        (
            (64, 48),
            PixelFormat::YuvJ420P,
            &["-sampling-factor", "2x2"],
        ),
        (
            (64, 48),
            PixelFormat::Gray8,
            &["-colorspace", "Gray", "-type", "Grayscale"],
        ),
        (
            (96, 64),
            PixelFormat::Gray8,
            &["-colorspace", "Gray", "-type", "Grayscale"],
        ),
        (
            (64, 48),
            PixelFormat::YuvJ420P,
            &["-sampling-factor", "2x2"],
        ),
    ];
    let mut segments = Vec::new();
    for (i, ((w, h), label, args)) in cells.into_iter().enumerate() {
        let out = dir.join(format!("layout-{i}-{w}x{h}.jpg"));
        let ok = Command::new(&magick)
            .args(["-size", &format!("{w}x{h}"), "plasma:fractal", "-seed", "7"])
            .args(args)
            .arg(&out)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let Some(data) = ok.then(|| std::fs::read(&out).ok()).flatten() else {
            eprintln!("mjpeg layouts: magick could not produce {w}x{h}");
            return;
        };
        assert_eq!(blob::jpeg_sof_label(&data), Some(label));
        let packets = (0..3)
            .map(|_| Packet::new(0, TimeBase::new(1, 25), data.clone()))
            .collect();
        segments.push(Seg {
            size: (w, h),
            label,
            packets,
        });
    }
    splice(
        "mjpeg layouts",
        "mjpeg",
        None,
        blob::make_jpeg_decoder,
        &segments,
        AtSwitch::Announce,
    );
}
