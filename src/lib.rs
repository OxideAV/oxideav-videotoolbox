#![cfg(target_os = "macos")]
//! macOS VideoToolbox hardware decode/encode bridge.
//!
//! This crate is a **runtime-loaded** bridge to Apple's
//! [VideoToolbox](https://developer.apple.com/documentation/videotoolbox)
//! framework. It uses [`libloading`] to `dlopen` the framework on
//! first use, so:
//!
//! * macOS builds have **no compile-time link dependency** on
//!   VideoToolbox; if the framework can't be loaded, the registered
//!   factories return `Error::Unsupported` and the framework registry
//!   falls back to the pure-Rust codec implementation.
//! * No Objective-C / Swift involved. VideoToolbox is a C API; symbol
//!   resolution + Core Foundation refcounting is all FFI.
//!
//! The crate is gated to `cfg(target_os = "macos")` at the source
//! level: on Linux / Windows the entire crate compiles to an empty
//! rlib, and consumers (umbrella `oxideav`) gate the `register` call
//! behind the same cfg.
//!
//! # Status
//!
//! H.264 + HEVC decode + encode and JPEG + ProRes decode + encode are wired
//! via `VTDecompressionSession` / `VTCompressionSession`. Round 4 added
//! **MPEG-2 video decode** (`kCMVideoCodecType_MPEG2Video`, decode-only —
//! VideoToolbox has no MPEG-2 encoder), with an elementary-stream framer
//! that carves per-picture access units. Round 5 added **VP9 decode**
//! (`kCMVideoCodecType_VP9` = `'vp09'`, decode-only — VT has no VP9 encoder
//! either); VP9 frames are container-framed (IVF / Matroska / MP4) so each
//! demuxed `Packet` is one access unit and the existing blob
//! `FrameSplit::Whole` path applies unchanged. Round 6 added **MPEG-4
//! Part 2 video decode** (`kCMVideoCodecType_MPEG4Video` = `'mp4v'` — the
//! DivX / Xvid / ASP family, **not** H.264). VT exposes no MPEG-4 Part 2
//! compression session, so it is decode-only as well; a new
//! `FrameSplit::Mpeg4PartTwoEs` framer splits on VOP start codes
//! (`00 00 01 B6`) and attaches preceding VOS / VOL / GOV headers to the
//! first VOP. **Round 7 (this commit) closes the VOL-extradata follow-up**
//! for MPEG-4 Part 2: the decoder sniffs the configuration prefix from the
//! first packet, wraps it in an ISO/IEC 14496-1 ESDS descriptor, and feeds
//! the result to `CMVideoFormatDescriptionCreate` via
//! `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms = { "esds"
//! : CFData }`. PSNR_Y vs ffmpeg's software decode reaches ≈ 72.8 dB
//! (sample-exact within IDCT tolerance) on the integration fixture. All
//! codec ids register with `priority = 10` and `hardware_accelerated = true`.
//!
//! # Workspace policy
//!
//! Calling a system OS framework via FFI is the same shape as calling
//! `libc::malloc` — it's the platform, not a copied algorithm. The
//! workspace's clean-room rule (no embedding source from libvpx,
//! libwebp, libjxl, etc.) doesn't apply here.

pub mod sys;

#[cfg(feature = "registry")]
pub mod blob;
#[cfg(feature = "registry")]
pub mod decoder;
#[cfg(feature = "registry")]
pub mod encoder;

/// Register VideoToolbox hardware factories: H.264 / HEVC / JPEG / ProRes
/// decode + encode, plus MPEG-2 video, VP9, and MPEG-4 Part 2 decode
/// (decode-only — none of the three has a VT encoder). If the framework
/// cannot be loaded (older OS, sandboxed environment, non-macOS) the function
/// logs and returns without registering anything — the runtime falls back to
/// the pure-Rust impls.
#[cfg(feature = "registry")]
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecTag};

    // Confirm the framework loads before registering factories.
    match sys::vtable() {
        Ok(_) => {}
        Err(e) => {
            // Library not available (e.g. running under Rosetta on an old
            // OS, or in a Linux cross-build test). Graceful no-op.
            eprintln!("oxideav-videotoolbox: framework unavailable, skipping registration: {e}");
            return;
        }
    }

    // ── H.264 decoder ──────────────────────────────────────────────────────
    let h264_caps = CodecCapabilities::video("h264_videotoolbox")
        .with_lossy(true)
        .with_intra_only(false)
        .with_hardware(true)
        .with_priority(10);

    ctx.codecs.register(
        CodecInfo::new(CodecId::new("h264"))
            .capabilities(h264_caps.clone().with_decode())
            .decoder(decoder::H264VtDecoder::make)
            .tags([
                CodecTag::fourcc(b"H264"),
                CodecTag::fourcc(b"h264"),
                CodecTag::fourcc(b"AVC1"),
                CodecTag::fourcc(b"avc1"),
                CodecTag::fourcc(b"X264"),
                CodecTag::matroska("V_MPEG4/ISO/AVC"),
            ]),
    );

    // ── H.264 encoder ──────────────────────────────────────────────────────
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("h264"))
            .capabilities(
                CodecCapabilities::video("h264_videotoolbox")
                    .with_lossy(true)
                    .with_intra_only(false)
                    .with_hardware(true)
                    .with_priority(10)
                    .with_encode(),
            )
            .encoder(encoder::make_h264_encoder),
    );

    // ── HEVC decoder ───────────────────────────────────────────────────────
    let hevc_caps = CodecCapabilities::video("hevc_videotoolbox")
        .with_lossy(true)
        .with_intra_only(false)
        .with_hardware(true)
        .with_priority(10);

    ctx.codecs.register(
        CodecInfo::new(CodecId::new("hevc"))
            .capabilities(hevc_caps.clone().with_decode())
            .decoder(decoder::HevcVtDecoder::make)
            .tags([
                CodecTag::fourcc(b"hvc1"),
                CodecTag::fourcc(b"hev1"),
                CodecTag::matroska("V_MPEGH/ISO/HEVC"),
            ]),
    );

    // ── HEVC encoder ───────────────────────────────────────────────────────
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("hevc"))
            .capabilities(
                CodecCapabilities::video("hevc_videotoolbox")
                    .with_lossy(true)
                    .with_intra_only(false)
                    .with_hardware(true)
                    .with_priority(10)
                    .with_encode(),
            )
            .encoder(encoder::make_hevc_encoder),
    );

    // ── JPEG decoder ───────────────────────────────────────────────────────
    let jpeg_caps = CodecCapabilities::video("mjpeg_videotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10);

    ctx.codecs.register(
        CodecInfo::new(CodecId::new("mjpeg"))
            .capabilities(jpeg_caps.clone().with_decode())
            .decoder(blob::make_jpeg_decoder)
            .tags([
                CodecTag::fourcc(b"jpeg"),
                CodecTag::fourcc(b"JPEG"),
                CodecTag::fourcc(b"MJPG"),
                CodecTag::fourcc(b"mjpg"),
            ]),
    );

    // ── JPEG encoder ───────────────────────────────────────────────────────
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("mjpeg"))
            .capabilities(jpeg_caps.clone().with_encode())
            .encoder(blob::make_jpeg_encoder),
    );

    // ── ProRes decoder ─────────────────────────────────────────────────────
    let prores_caps = CodecCapabilities::video("prores_videotoolbox")
        .with_lossy(true)
        .with_intra_only(true)
        .with_hardware(true)
        .with_priority(10);

    ctx.codecs.register(
        CodecInfo::new(CodecId::new("prores"))
            .capabilities(prores_caps.clone().with_decode())
            .decoder(blob::make_prores_decoder)
            .tags([
                CodecTag::fourcc(b"apco"),
                CodecTag::fourcc(b"apcs"),
                CodecTag::fourcc(b"apcn"),
                CodecTag::fourcc(b"apch"),
                CodecTag::fourcc(b"ap4h"),
                CodecTag::fourcc(b"ap4x"),
            ]),
    );

    // ── ProRes encoder ─────────────────────────────────────────────────────
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("prores"))
            .capabilities(prores_caps.clone().with_encode())
            .encoder(blob::make_prores_encoder),
    );

    // ── MPEG-2 decoder (decode-only — VT has no MPEG-2 encoder) ─────────────
    let mpeg2_caps = CodecCapabilities::video("mpeg2_videotoolbox")
        .with_lossy(true)
        .with_intra_only(false)
        .with_hardware(true)
        .with_priority(10);

    ctx.codecs.register(
        CodecInfo::new(CodecId::new("mpeg2video"))
            .capabilities(mpeg2_caps.clone().with_decode())
            .decoder(blob::make_mpeg2_decoder)
            .tags([
                CodecTag::fourcc(b"mp2v"),
                CodecTag::fourcc(b"MPG2"),
                CodecTag::fourcc(b"mpg2"),
                CodecTag::fourcc(b"hdv2"),
                CodecTag::fourcc(b"m2v1"),
                CodecTag::matroska("V_MPEG2"),
            ]),
    );

    // ── VP9 decoder (decode-only — VT has no VP9 encoder) ───────────────────
    let vp9_caps = CodecCapabilities::video("vp9_videotoolbox")
        .with_lossy(true)
        .with_intra_only(false)
        .with_hardware(true)
        .with_priority(10);

    ctx.codecs.register(
        CodecInfo::new(CodecId::new("vp9"))
            .capabilities(vp9_caps.clone().with_decode())
            .decoder(blob::make_vp9_decoder)
            .tags([
                CodecTag::fourcc(b"vp09"),
                CodecTag::fourcc(b"VP90"),
                CodecTag::matroska("V_VP9"),
            ]),
    );

    // ── MPEG-4 Part 2 decoder (decode-only — VT has no MPEG-4 Pt 2 encoder) ─
    //
    // This is MPEG-4 Part 2 (Visual / ASP / SP) — the family that includes
    // DivX and Xvid — **not** MPEG-4 Part 10 (H.264). H.264 is registered
    // separately above with `kCMVideoCodecType_H264` (`'avc1'`).
    let mpeg4_caps = CodecCapabilities::video("mpeg4_videotoolbox")
        .with_lossy(true)
        .with_intra_only(false)
        .with_hardware(true)
        .with_priority(10);

    ctx.codecs.register(
        CodecInfo::new(CodecId::new("mpeg4"))
            .capabilities(mpeg4_caps.clone().with_decode())
            .decoder(blob::make_mpeg4_part_two_decoder)
            .tags([
                CodecTag::fourcc(b"mp4v"),
                CodecTag::fourcc(b"MP4V"),
                CodecTag::fourcc(b"M4S2"),
                CodecTag::fourcc(b"m4s2"),
                CodecTag::fourcc(b"DIVX"),
                CodecTag::fourcc(b"divx"),
                CodecTag::fourcc(b"DX50"),
                CodecTag::fourcc(b"XVID"),
                CodecTag::fourcc(b"xvid"),
                CodecTag::fourcc(b"FMP4"),
                CodecTag::fourcc(b"fmp4"),
                CodecTag::matroska("V_MPEG4/ISO/ASP"),
            ]),
    );

    let _ = (
        h264_caps,
        hevc_caps,
        jpeg_caps,
        prores_caps,
        mpeg2_caps,
        vp9_caps,
        mpeg4_caps,
    ); // suppress unused warnings
}

#[cfg(feature = "registry")]
oxideav_core::register!("videotoolbox", register);
