//! Mid-stream shape changes through the VideoToolbox H.264 / HEVC
//! decoders: the core output-description contract (`output_params`,
//! `output_params_generation`, `Frame::StreamParams`).
//!
//! Fixtures (`tests/fixtures/shape_*`, black-box encoder output, Annex
//! B): a 64×48 16-picture segment with B-frames and a second IDR / CRA
//! that repeats the parameter sets byte for byte, a 200×120 segment
//! (not a macroblock / CTB multiple → cropping window), and for HEVC a
//! 200×120 Main 10 segment. The tests splice them back to back.
//!
//! Gates: `output_params()` answers the first segment's size and label
//! before the first frame; exactly one `StreamParams` event per change
//! (none for the first shape, none for the repeated parameter sets),
//! placed immediately before the first frame of the new shape, with the
//! payload equal to `output_params()` and the generation already moved;
//! every frame's plane geometry matches the description in force; the
//! frames are the ones (in the order) each segment decodes to on its
//! own; the same placement when everything is drained at `flush`, and
//! after `reset`.
//!
//! Passes by reporting "unavailable" when the framework or the hardware
//! codec is absent.

#![cfg(all(target_os = "macos", feature = "registry"))]

mod common;

use common::shape::*;
use common::*;
use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Packet, PixelFormat, TimeBase};
use oxideav_videotoolbox::decoder::{NalCodec, NalVtDecoder, annex_b_nals, split_access_units};

struct Segment {
    file: &'static str,
    expect: Expect,
}

const A: Segment = Segment {
    file: "shape_a_64x48_16f",
    expect: Expect {
        width: 64,
        height: 48,
        label: PixelFormat::Yuv420P,
        pictures: 16,
    },
};
const B: Segment = Segment {
    file: "shape_b_200x120_8f",
    expect: Expect {
        width: 200,
        height: 120,
        label: PixelFormat::Yuv420P,
        pictures: 8,
    },
};
const C10: Segment = Segment {
    file: "shape_c_200x120_10bit_8f",
    expect: Expect {
        width: 200,
        height: 120,
        label: PixelFormat::Yuv420P10Le,
        pictures: 8,
    },
};

fn ext(codec: NalCodec) -> &'static str {
    match codec {
        NalCodec::H264 => "h264",
        NalCodec::Hevc => "hevc",
        other => unreachable!("no case for {other:?}"),
    }
}

/// Access units of a segment, each re-framed as Annex B, one packet each.
fn access_units(codec: NalCodec, seg: &Segment) -> Vec<Packet> {
    let es = std::fs::read(fixture(&format!("{}.{}", seg.file, ext(codec)))).expect("fixture");
    let nals = annex_b_nals(&es);
    split_access_units(codec, &nals)
        .into_iter()
        .map(|unit| {
            let mut out = Vec::new();
            for nal in unit {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(nal);
            }
            Packet::new(0, TimeBase::new(1, 25), out)
        })
        .collect()
}

fn make(codec: NalCodec) -> Option<Box<dyn Decoder>> {
    let id = ext(codec);
    match NalVtDecoder::make(codec, &CodecParameters::video(CodecId::new(id))) {
        Ok(d) => Some(d),
        Err(Error::Unsupported(e)) => {
            eprintln!("{id}: VideoToolbox decoder unavailable ({e})");
            None
        }
        Err(e) => panic!("{id}: construction failed: {e}"),
    }
}

/// The pictures one segment decodes to on its own (fresh decoder).
fn solo(codec: NalCodec, seg: &Segment) -> Option<Vec<oxideav_core::VideoFrame>> {
    let mut dec = make(codec)?;
    let frames = match try_run(dec.as_mut(), &access_units(codec, seg), true) {
        Ok(out) => pictures(seg.file, out),
        Err(Error::Unsupported(e)) => {
            eprintln!(
                "{}: VideoToolbox cannot decode this segment ({e})",
                seg.file
            );
            return None;
        }
        Err(e) => panic!("{}: solo decode: {e}", seg.file),
    };
    assert_eq!(
        frames.len(),
        seg.expect.pictures,
        "{}: picture count",
        seg.file
    );
    Some(frames)
}

fn splice(codec: NalCodec, segs: &[&Segment]) {
    if !vt_available() {
        eprintln!("VideoToolbox framework unavailable");
        return;
    }
    let mut solos = Vec::new();
    for seg in segs {
        match solo(codec, seg) {
            Some(f) => solos.push(f),
            None => return,
        }
    }
    let units: Vec<Packet> = segs.iter().flat_map(|s| access_units(codec, s)).collect();
    let expects: Vec<Expect> = segs.iter().map(|s| s.expect).collect();
    let tag = ext(codec);

    // Interleaved send / receive.
    let mut dec = make(codec).unwrap();
    // The first parameter sets ride in the first packet; before any frame
    // is returned the description already answers the first segment.
    dec.send_packet(&units[0]).expect("first packet");
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
    assert_eq!(dec.output_params_generation(), 0);
    let mut out = Vec::new();
    drain(dec.as_mut(), &mut out);
    out.extend(run(dec.as_mut(), &units[1..], true));
    check(&format!("{tag} interleaved"), &out, &expects, &solos);

    // Everything sent first, all output drained at flush.
    let mut dec = make(codec).unwrap();
    let out = run(dec.as_mut(), &units, false);
    check(&format!("{tag} drained at flush"), &out, &expects, &solos);
}

#[test]
fn h264_size_switch_announced_once_per_change() {
    splice(NalCodec::H264, &[&A, &B, &A]);
}

#[test]
fn hevc_size_and_bit_depth_switches_announced_once_per_change() {
    splice(NalCodec::Hevc, &[&A, &B, &C10]);
}

/// A seek into a segment of another shape: the first frame after
/// `reset` is preceded by the event; frames discarded by the reset never
/// count as returned.
fn reset_into(codec: NalCodec, second: &Segment) {
    if !vt_available() {
        eprintln!("VideoToolbox framework unavailable");
        return;
    }
    let Some(solo_second) = solo(codec, second) else {
        return;
    };
    let tag = format!("{} reset", ext(codec));
    let mut dec = make(codec).unwrap();
    let a = access_units(codec, &A);
    let mut out = Vec::new();
    for u in &a[..6] {
        dec.send_packet(u).unwrap();
        drain(dec.as_mut(), &mut out);
    }
    let returned = out.len();
    assert!(returned > 0, "{tag}: some 64x48 pictures before the seek");
    assert!(out.iter().all(|o| matches!(o, Out::Picture { .. })));
    dec.reset().expect("reset");
    // Still describes the last returned frame until the next one.
    assert!(
        same_params(&dec.output_params().unwrap(), &A.expect),
        "{tag}: kept"
    );
    assert_eq!(dec.output_params_generation(), 0);
    let mut after = run(dec.as_mut(), &access_units(codec, second), true);
    let Out::Event {
        payload,
        now,
        generation,
    } = after.remove(0)
    else {
        panic!("{tag}: first output after the seek is not the event");
    };
    assert!(same_params(&payload, &second.expect), "{tag}: payload");
    assert!(
        identical(now.as_ref(), &payload),
        "{tag}: payload == output_params()"
    );
    assert_eq!(generation, 1);
    assert_eq!(
        after.len(),
        second.expect.pictures,
        "{tag}: pictures after the seek"
    );
    for (i, o) in after.iter().enumerate() {
        let Out::Picture {
            frame, generation, ..
        } = o
        else {
            panic!("{tag}: second event");
        };
        assert_eq!(*generation, 1);
        assert!(
            packed(frame) == packed(&solo_second[i]),
            "{tag}: picture {i}"
        );
    }

    // Seeking back into the same shape announces nothing.
    dec.reset().unwrap();
    let again = run(dec.as_mut(), &access_units(codec, second), true);
    assert_eq!(
        again.len(),
        second.expect.pictures,
        "{tag}: same-shape seek"
    );
    assert!(again.iter().all(|o| matches!(o, Out::Picture { .. })));
    assert_eq!(dec.output_params_generation(), 1);
}

#[test]
fn h264_reset_into_another_shape() {
    reset_into(NalCodec::H264, &B);
}

#[test]
fn hevc_reset_into_another_shape() {
    reset_into(NalCodec::Hevc, &C10);
}
