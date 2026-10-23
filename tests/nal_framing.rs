//! H.264 / HEVC through the VideoToolbox decoder in **both framings**:
//! Annex B (per access unit and the whole stream in one packet) and
//! ISO-MP4 style (`avcC` / `hvcC` extradata + length-prefixed samples).
//!
//! Fixtures (`tests/fixtures/`, ≤ 30 KiB each): a 48×32 (not a
//! macroblock multiple → SPS cropping window) 12-picture stream with
//! B-frames from the black-box H.264 / HEVC encoders, plus the black-box
//! reference decoder's planar 4:2:0 output of each.
//!
//! Gates: frame count == pictures, cropping applied (48×32 planes, not
//! 48×48 / 64×64), output label `Yuv420P` with matching plane geometry,
//! display order (PSNR ≥ 45 dB against the in-order reference, picture
//! by picture), the re-ordered tail delivered at `flush`, and packet
//! timestamps carried through.
//!
//! Passes by reporting "unavailable" when the framework or the hardware
//! codec is absent.

#![cfg(all(target_os = "macos", feature = "registry"))]

mod common;

use common::*;
use oxideav_core::{CodecId, CodecParameters, Decoder, Error, PixelFormat, TimeBase};
use oxideav_videotoolbox::decoder::{
    NalCodec, NalVtDecoder, ParamSets, annex_b_nals, split_access_units,
};

const W: u32 = 48;
const H: u32 = 32;
const PICTURES: usize = 12;

struct Stream {
    codec: NalCodec,
    annex_b: Vec<u8>,
    reference: Vec<u8>,
}

fn stream(codec: NalCodec) -> Stream {
    let (es, yuv) = match codec {
        NalCodec::H264 => ("x264_48x32_12f.h264", "x264_48x32_12f.yuv"),
        NalCodec::Hevc => ("x265_48x32_12f.hevc", "x265_48x32_12f.yuv"),
    };
    let annex_b = std::fs::read(fixture(es)).expect("fixture");
    let reference = std::fs::read(fixture(yuv)).expect("reference");
    assert_eq!(
        reference.len(),
        picture_bytes(PixelFormat::Yuv420P, W, H) * PICTURES,
        "reference holds {PICTURES} cropped pictures"
    );
    Stream {
        codec,
        annex_b,
        reference,
    }
}

/// Access units of the Annex-B stream, each re-framed as Annex B.
fn access_units(s: &Stream) -> Vec<Vec<u8>> {
    let nals = annex_b_nals(&s.annex_b);
    split_access_units(s.codec, &nals)
        .into_iter()
        .map(|unit| {
            let mut buf = Vec::new();
            for nal in unit {
                buf.extend_from_slice(&[0, 0, 0, 1]);
                buf.extend_from_slice(nal);
            }
            buf
        })
        .collect()
}

/// Parameter sets of the stream and its access units with the parameter
/// sets stripped and 4-byte length prefixes (what a container hands over).
fn length_prefixed(s: &Stream) -> (ParamSets, Vec<Vec<u8>>) {
    let ps = ParamSets::from_extradata(s.codec, &s.annex_b).expect("in-band parameter sets");
    let nals = annex_b_nals(&s.annex_b);
    let units = split_access_units(s.codec, &nals)
        .into_iter()
        .map(|unit| {
            let mut buf = Vec::new();
            for nal in unit {
                let t = s.codec.nal_type(nal).unwrap();
                let is_ps = match s.codec {
                    NalCodec::H264 => matches!(t, 7 | 8),
                    NalCodec::Hevc => matches!(t, 32..=34),
                };
                if is_ps {
                    continue;
                }
                buf.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                buf.extend_from_slice(nal);
            }
            buf
        })
        .filter(|u| !u.is_empty())
        .collect();
    (ps, units)
}

/// `avcC` (ISO/IEC 14496-15 §5.3.3.1.2) from the stream's SPS / PPS.
fn build_avcc(ps: &ParamSets) -> Vec<u8> {
    let sps = &ps.sets[1][0];
    let mut v = vec![
        1,
        sps[1],
        sps[2],
        sps[3],
        0xFF,
        0xE0 | ps.sets[1].len() as u8,
    ];
    for s in &ps.sets[1] {
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s);
    }
    v.push(ps.sets[2].len() as u8);
    for p in &ps.sets[2] {
        v.extend_from_slice(&(p.len() as u16).to_be_bytes());
        v.extend_from_slice(p);
    }
    v
}

/// `hvcC` (§8.3.3.1.2) from the stream's VPS / SPS / PPS; the 22-byte
/// header is zero apart from `configurationVersion` and
/// `lengthSizeMinusOne` (the decoder reads only the arrays).
fn build_hvcc(ps: &ParamSets) -> Vec<u8> {
    let mut v = vec![0u8; 22];
    v[0] = 1;
    v[21] = 0x0F; // reserved bits set, lengthSizeMinusOne = 3
    v.push(3);
    for (t, slot) in [(32u8, 0usize), (33, 1), (34, 2)] {
        v.push(0x80 | t);
        v.extend_from_slice(&(ps.sets[slot].len() as u16).to_be_bytes());
        for n in &ps.sets[slot] {
            v.extend_from_slice(&(n.len() as u16).to_be_bytes());
            v.extend_from_slice(n);
        }
    }
    v
}

fn params(codec: NalCodec, extradata: Vec<u8>, declare_size: bool) -> CodecParameters {
    let mut p = CodecParameters::video(CodecId::new(match codec {
        NalCodec::H264 => "h264",
        NalCodec::Hevc => "hevc",
    }));
    if declare_size {
        p.width = Some(W);
        p.height = Some(H);
    }
    p.extradata = extradata;
    p
}

fn make(codec: NalCodec, p: &CodecParameters) -> Option<Box<dyn Decoder>> {
    match NalVtDecoder::make(codec, p) {
        Ok(d) => Some(d),
        Err(Error::Unsupported(e)) => {
            eprintln!("{codec:?}: VideoToolbox decoder unavailable on this host ({e}); skipping");
            None
        }
        Err(e) => panic!("{codec:?}: decoder construction: {e}"),
    }
}

/// The gates every framing must pass.
fn check(s: &Stream, got: &Decoded, label: Option<PixelFormat>, tag: &str) {
    assert_eq!(label, Some(PixelFormat::Yuv420P), "{tag}: output label");
    let all = got.all();
    assert_eq!(all.len(), PICTURES, "{tag}: frame count");
    assert!(
        !got.from_flush.is_empty(),
        "{tag}: a B-frame stream must release its re-ordered tail at flush"
    );
    let bytes = picture_bytes(PixelFormat::Yuv420P, W, H);
    for (i, f) in all.iter().enumerate() {
        assert_geometry(f, PixelFormat::Yuv420P, W, H, &format!("{tag}: frame {i}"));
        let want = &s.reference[i * bytes..(i + 1) * bytes];
        let got = packed(f);
        let p = psnr(&got, want);
        assert!(
            p >= 45.0,
            "{tag}: frame {i} PSNR {p:.1} dB < 45 dB — wrong picture or wrong order"
        );
    }
}

fn run(codec: NalCodec) {
    if !vt_available() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping {codec:?} framing");
        return;
    }
    let s = stream(codec);
    let tb = TimeBase::new(1, 25);

    // Annex B, one access unit per packet, no timestamps (a raw byte
    // stream demuxer's shape).
    let Some(mut dec) = make(codec, &params(codec, Vec::new(), false)) else {
        return;
    };
    assert_eq!(
        dec.output_pixel_format(),
        None,
        "no label before the first parameter set"
    );
    let units = access_units(&s);
    assert_eq!(units.len(), PICTURES, "access-unit split");
    let got = decode_all(dec.as_mut(), &packets(units.clone(), None, tb)).expect("annex b per AU");
    check(&s, &got, dec.output_pixel_format(), "annex-b/per-AU");
    assert!(
        got.all().iter().all(|f| f.pts.is_none()),
        "packets without pts yield frames without pts"
    );

    // Annex B, the whole stream in one packet.
    let mut dec = make(codec, &params(codec, Vec::new(), false)).unwrap();
    let got = decode_all(dec.as_mut(), &packets(vec![s.annex_b.clone()], None, tb))
        .expect("annex b whole");
    check(&s, &got, dec.output_pixel_format(), "annex-b/whole");

    // Annex B parameter sets as extradata, in-band pictures.
    let mut dec = make(codec, &params(codec, s.annex_b.clone(), true)).unwrap();
    assert_eq!(
        dec.output_pixel_format(),
        Some(PixelFormat::Yuv420P),
        "label known from extradata before the first packet"
    );
    let got = decode_all(dec.as_mut(), &packets(units, None, tb)).expect("annex b extradata");
    check(&s, &got, dec.output_pixel_format(), "annex-b/extradata");

    // Configuration record + length-prefixed samples, with per-packet
    // stamps (decode-order indices: the stamp must ride on its own
    // picture whatever the output order).
    let (ps, samples) = length_prefixed(&s);
    assert_eq!(samples.len(), PICTURES);
    let record = match codec {
        NalCodec::H264 => build_avcc(&ps),
        NalCodec::Hevc => build_hvcc(&ps),
    };
    let parsed = ParamSets::from_extradata(codec, &record).expect("record parses");
    assert_eq!(parsed.sets, ps.sets);
    assert_eq!(parsed.length_size, Some(4));
    let mut dec = make(codec, &params(codec, record, true)).unwrap();
    assert_eq!(dec.output_pixel_format(), Some(PixelFormat::Yuv420P));
    let stamps: Vec<i64> = (0..PICTURES as i64).map(|i| i * 40).collect();
    let got =
        decode_all(dec.as_mut(), &packets(samples, Some(&stamps), tb)).expect("length-prefixed");
    check(
        &s,
        &got,
        dec.output_pixel_format(),
        "avcC-hvcC/length-prefixed",
    );
    let mut seen: Vec<i64> = got
        .all()
        .iter()
        .map(|f| f.pts.expect("pts carried"))
        .collect();
    seen.sort_unstable();
    assert_eq!(
        seen, stamps,
        "every packet stamp comes back on exactly one frame"
    );
}

#[test]
fn h264_both_framings_decode_every_picture_in_display_order() {
    run(NalCodec::H264);
}

#[test]
fn hevc_both_framings_decode_every_picture_in_display_order() {
    run(NalCodec::Hevc);
}

/// A declared layout VideoToolbox cannot deliver is refused at
/// construction (typed `Unsupported`, so the registry falls back to the
/// pure-Rust decoder) — never silently re-labelled.
#[test]
fn undeliverable_declared_layout_is_refused_at_construction() {
    if !vt_available() {
        eprintln!("oxideav-videotoolbox: framework unavailable, skipping");
        return;
    }
    for pf in [
        PixelFormat::Yuv420P12Le,
        PixelFormat::Cmyk,
        PixelFormat::Yuva420P,
    ] {
        let mut p = params(NalCodec::H264, Vec::new(), true);
        p.pixel_format = Some(pf);
        match NalVtDecoder::make(NalCodec::H264, &p) {
            Err(Error::Unsupported(_)) => {}
            other => panic!("{pf:?}: expected Unsupported, got {:?}", other.map(|_| ())),
        }
    }
}
