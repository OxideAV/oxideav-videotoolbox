//! The JPEG label matrix: sampling (4:2:0 / 4:2:2 / 4:4:4 / 4:4:0 /
//! 4:1:1 / grey / CMYK) × size (even, odd, tiny) through the VideoToolbox
//! JPEG decoder.
//!
//! Measured on the M-series media engine and on VideoToolbox's own
//! software JPEG path alike: the decompressor decodes every source at
//! 4:2:0 (a 4:2:2 / 4:4:4 output request only adds a pixel-transfer
//! upsample — chroma ≈ 32 dB against the real samples and luma broken at
//! odd sizes — 4:4:0 and 4:1:1 scans fail, CMYK is rejected). So the
//! decoder claims 4:2:0 and single-component sources, delivers them in
//! the full-range label the pure-Rust decoder uses (`YuvJ420P` /
//! `Gray8`) with the exact plane geometry, and returns a typed
//! `Unsupported` for everything else so the registry falls back.
//!
//! Sources are produced at test time with the black-box `magick`
//! encoder and compared with the black-box reference decoder's planar
//! output (PSNR ≥ 45 dB per plane). Passes by reporting "unavailable"
//! when the framework or a tool is absent.

#![cfg(all(target_os = "macos", feature = "registry"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use oxideav_core::{CodecId, CodecParameters, Error, PixelFormat, TimeBase};
use oxideav_videotoolbox::blob::{jpeg_sof_label, make_jpeg_decoder};

const SIZES: [(u32, u32); 6] = [(64, 48), (57, 31), (33, 17), (16, 16), (2, 3), (1, 1)];

/// `(name, magick arguments, SOF label, reference pix_fmt)`.
const SAMPLINGS: [(&str, &[&str], Option<PixelFormat>, &str); 6] = [
    (
        "420",
        &["-sampling-factor", "2x2"],
        Some(PixelFormat::YuvJ420P),
        "yuvj420p",
    ),
    (
        "422",
        &["-sampling-factor", "2x1"],
        Some(PixelFormat::YuvJ422P),
        "yuvj422p",
    ),
    (
        "444",
        &["-sampling-factor", "1x1"],
        Some(PixelFormat::YuvJ444P),
        "yuvj444p",
    ),
    (
        "440",
        &["-sampling-factor", "1x2"],
        Some(PixelFormat::YuvJ444P),
        "yuvj440p",
    ),
    (
        "411",
        &["-sampling-factor", "4x1"],
        Some(PixelFormat::Yuv411P),
        "yuvj411p",
    ),
    (
        "gray",
        &["-colorspace", "Gray", "-type", "Grayscale"],
        Some(PixelFormat::Gray8),
        "gray",
    ),
];

fn produce(
    magick: &Path,
    dir: &Path,
    name: &str,
    args: &[&str],
    w: u32,
    h: u32,
) -> Option<PathBuf> {
    let out = dir.join(format!("{name}_{w}x{h}.jpg"));
    let ok = Command::new(magick)
        .args([
            "-size",
            &format!("{w}x{h}"),
            "plasma:fractal",
            "-seed",
            "7",
            "-quality",
            "92",
        ])
        .args(args)
        .arg(&out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(out)
}

fn decode(path: &Path, w: u32, h: u32, declared: Option<PixelFormat>) -> Result<Decoded, Error> {
    let mut p = CodecParameters::video(CodecId::new("mjpeg"));
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = declared;
    let mut dec = make_jpeg_decoder(&p)?;
    let data = std::fs::read(path).expect("jpeg");
    let got = decode_all(
        dec.as_mut(),
        &[oxideav_core::Packet::new(0, TimeBase::new(1, 1), data)],
    )?;
    if let Some(f) = got.all().first() {
        let label = dec
            .output_pixel_format()
            .expect("label after the first packet");
        assert_geometry(f, label, w, h, &path.display().to_string());
    }
    Ok(got)
}

#[test]
fn jpeg_matrix_is_truthfully_labelled_or_refused() {
    if !vt_available() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping JPEG matrix");
        return;
    }
    let Some(magick) = tool("magick") else {
        eprintln!("oxideav-videotoolbox: magick unavailable, skipping JPEG matrix");
        return;
    };
    let ffmpeg = tool("ffmpeg");
    if ffmpeg.is_none() {
        eprintln!("oxideav-videotoolbox: ffmpeg unavailable — geometry gates only, no PSNR");
    }
    let dir = scratch("jpeg-matrix");
    let mut table = Vec::new();

    for (name, args, sof_label, ref_fmt) in SAMPLINGS {
        for (w, h) in SIZES {
            let Some(path) = produce(&magick, &dir, name, args, w, h) else {
                eprintln!("{name} {w}x{h}: magick could not produce it; skipping cell");
                continue;
            };
            let data = std::fs::read(&path).unwrap();
            assert_eq!(
                jpeg_sof_label(&data),
                sof_label,
                "{name} {w}x{h}: SOF label"
            );
            let accepted = matches!(sof_label, Some(PixelFormat::YuvJ420P | PixelFormat::Gray8));

            // Undeclared label (the decoder reads the SOF) and the label the
            // pure-Rust decoder would have put on the stream.
            for declared in [None, sof_label] {
                let tag = format!("{name} {w}x{h} declared={declared:?}");
                match decode(&path, w, h, declared) {
                    Ok(got) if accepted => {
                        assert_eq!(got.all().len(), 1, "{tag}: one frame");
                        let label = sof_label.unwrap();
                        if let Some(ffmpeg) = &ffmpeg {
                            let reference =
                                reference_decode(ffmpeg, &path, ref_fmt).expect("reference");
                            let got = packed(got.all()[0]);
                            assert_eq!(got.len(), reference.len(), "{tag}: picture bytes");
                            let mut off = 0;
                            let mut worst = 99.0f64;
                            for i in 0..label.plane_count() {
                                let (pw, ph) = label.plane_dimensions(i, w, h).unwrap();
                                let n = (pw * ph) as usize;
                                worst =
                                    worst.min(psnr(&got[off..off + n], &reference[off..off + n]));
                                off += n;
                            }
                            assert!(worst >= 45.0, "{tag}: plane PSNR {worst:.1} dB < 45 dB");
                            table.push(format!("{tag}: OK {label:?} worst-plane {worst:.1} dB"));
                        } else {
                            table.push(format!("{tag}: OK {label:?} (geometry only)"));
                        }
                    }
                    Ok(got) => panic!(
                        "{tag}: VideoToolbox must refuse this scan, got {} frame(s)",
                        got.all().len()
                    ),
                    Err(Error::Unsupported(e)) if !accepted => {
                        table.push(format!("{tag}: refused ({e})"));
                    }
                    Err(e) => panic!("{tag}: {e}"),
                }
            }
        }
    }

    // CMYK: refused at construction when declared, at the SOF otherwise.
    let cmyk = dir.join("cmyk_64x48.jpg");
    let ok = Command::new(&magick)
        .args([
            "-size",
            "64x48",
            "plasma:fractal",
            "-seed",
            "7",
            "-quality",
            "92",
            "-colorspace",
            "CMYK",
        ])
        .arg(&cmyk)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        let data = std::fs::read(&cmyk).unwrap();
        assert_eq!(jpeg_sof_label(&data), None, "CMYK has no deliverable label");
        assert!(matches!(
            decode(&cmyk, 64, 48, None),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            decode(&cmyk, 64, 48, Some(PixelFormat::Cmyk)),
            Err(Error::Unsupported(_))
        ));
        table.push("cmyk 64x48: refused".to_string());
    }

    // A declared label that disagrees with the scan is refused too.
    if let Some(path) = produce(&magick, &dir, "420", &["-sampling-factor", "2x2"], 16, 16) {
        assert!(matches!(
            decode(&path, 16, 16, Some(PixelFormat::YuvJ444P)),
            Err(Error::Unsupported(_))
        ));
    }

    for line in &table {
        println!("{line}");
    }
    let ok = table.iter().filter(|l| l.contains(": OK")).count();
    let refused = table.iter().filter(|l| l.contains("refused")).count();
    println!("JPEG matrix: {ok} cells delivered, {refused} refused (fallback)");
    assert!(
        ok >= 2 * SIZES.len(),
        "4:2:0 and grey cells must all decode"
    );
}
