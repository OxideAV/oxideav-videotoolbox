//! Checks for the core output-description contract (`output_params`,
//! `output_params_generation`, `Frame::StreamParams`) on spliced
//! streams: each segment is one shape, decoded on its own for the
//! reference pictures.

use oxideav_core::{CodecParameters, Decoder, Error, Frame, Packet, PixelFormat, VideoFrame};

use super::{assert_geometry, packed};

/// One segment of a spliced stream: its picture shape and count.
#[derive(Clone, Copy, Debug)]
pub struct Expect {
    pub width: u32,
    pub height: u32,
    pub label: PixelFormat,
    pub pictures: usize,
}

/// What `receive_frame` returned, with the decoder's answers at that
/// moment.
#[allow(clippy::large_enum_variant)] // test bookkeeping
pub enum Out {
    Event {
        payload: CodecParameters,
        now: Option<CodecParameters>,
        generation: u64,
    },
    Picture {
        frame: VideoFrame,
        now: Option<CodecParameters>,
        generation: u64,
    },
}

pub fn try_drain(dec: &mut dyn Decoder, into: &mut Vec<Out>) -> Result<(), Error> {
    loop {
        match dec.receive_frame() {
            Ok(Frame::StreamParams(p)) => into.push(Out::Event {
                payload: *p,
                now: dec.output_params(),
                generation: dec.output_params_generation(),
            }),
            Ok(Frame::Video(frame)) => into.push(Out::Picture {
                frame,
                now: dec.output_params(),
                generation: dec.output_params_generation(),
            }),
            Ok(_) => panic!("unexpected frame kind"),
            Err(Error::NeedMore) | Err(Error::Eof) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

pub fn drain(dec: &mut dyn Decoder, into: &mut Vec<Out>) {
    try_drain(dec, into).expect("receive_frame");
}

/// Feed `packets`, draining after every packet (or only at the end when
/// `drain_each` is false), then flush and drain. The first error is
/// returned.
pub fn try_run(
    dec: &mut dyn Decoder,
    packets: &[Packet],
    drain_each: bool,
) -> Result<Vec<Out>, Error> {
    let mut out = Vec::new();
    for p in packets {
        dec.send_packet(p)?;
        if drain_each {
            try_drain(dec, &mut out)?;
        }
    }
    dec.flush()?;
    try_drain(dec, &mut out)?;
    Ok(out)
}

pub fn run(dec: &mut dyn Decoder, packets: &[Packet], drain_each: bool) -> Vec<Out> {
    try_run(dec, packets, drain_each).expect("decode")
}

/// Field-for-field equality (`CodecParameters` has no `PartialEq`; its
/// `Debug` form lists every field).
pub fn identical(now: Option<&CodecParameters>, payload: &CodecParameters) -> bool {
    now.is_some_and(|n| format!("{n:?}") == format!("{payload:?}"))
}

pub fn same_params(p: &CodecParameters, seg: &Expect) -> bool {
    p.width == Some(seg.width) && p.height == Some(seg.height) && p.pixel_format == Some(seg.label)
}

/// Check one spliced decode against the segments and their solo decodes.
pub fn check(tag: &str, out: &[Out], segs: &[Expect], solos: &[Vec<VideoFrame>]) {
    let mut seg_idx = 0usize;
    let mut in_seg = 0usize;
    let mut events = 0usize;
    let mut last_gen = 0u64;
    let mut pending_event = false;
    for (i, o) in out.iter().enumerate() {
        match o {
            Out::Event {
                payload,
                now,
                generation,
            } => {
                assert!(!pending_event, "{tag}[{i}]: two events in a row");
                assert_eq!(
                    in_seg, segs[seg_idx].pictures,
                    "{tag}[{i}]: event before every picture of segment {seg_idx} was out"
                );
                seg_idx += 1;
                in_seg = 0;
                events += 1;
                let seg = &segs[seg_idx];
                assert!(
                    same_params(payload, seg),
                    "{tag}[{i}]: payload {:?}x{:?} {:?} is not segment {seg_idx}",
                    payload.width,
                    payload.height,
                    payload.pixel_format
                );
                assert!(
                    identical(now.as_ref(), payload),
                    "{tag}[{i}]: payload == output_params()"
                );
                assert_eq!(*generation, last_gen + 1, "{tag}[{i}]: one bump per event");
                last_gen = *generation;
                pending_event = true;
            }
            Out::Picture {
                frame,
                now,
                generation,
            } => {
                pending_event = false;
                if in_seg == segs[seg_idx].pictures {
                    panic!(
                        "{tag}[{i}]: picture {} of segment {seg_idx} has no event before it",
                        in_seg + 1
                    );
                }
                let seg = &segs[seg_idx];
                assert_eq!(
                    *generation, last_gen,
                    "{tag}[{i}]: generation moved silently"
                );
                let now = now.as_ref().expect("output_params after a frame");
                assert!(
                    same_params(now, seg),
                    "{tag}[{i}]: output_params describes the frame"
                );
                assert_geometry(frame, seg.label, seg.width, seg.height, tag);
                assert!(
                    packed(frame) == packed(&solos[seg_idx][in_seg]),
                    "{tag}[{i}]: picture {in_seg} of segment {seg_idx} differs from its solo decode"
                );
                in_seg += 1;
            }
        }
    }
    assert_eq!(seg_idx, segs.len() - 1, "{tag}: every segment reached");
    assert_eq!(
        in_seg, segs[seg_idx].pictures,
        "{tag}: last segment complete"
    );
    assert_eq!(events, segs.len() - 1, "{tag}: one event per change");
}

/// The pictures of a single-shape decode (no event allowed).
pub fn pictures(tag: &str, out: Vec<Out>) -> Vec<VideoFrame> {
    out.into_iter()
        .map(|o| match o {
            Out::Picture { frame, .. } => frame,
            Out::Event { .. } => panic!("{tag}: event in a single-shape stream"),
        })
        .collect()
}
