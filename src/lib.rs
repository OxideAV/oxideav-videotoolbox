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
//! Round 2 (this commit): real H.264 + HEVC decode + encode factories
//! via `VTDecompressionSession` / `VTCompressionSession`. Both codec ids
//! register with `priority = 10` and `hardware_accelerated = true`.
//!
//! # Workspace policy
//!
//! Calling a system OS framework via FFI is the same shape as calling
//! `libc::malloc` — it's the platform, not a copied algorithm. The
//! workspace's clean-room rule (no embedding source from libvpx,
//! libwebp, libjxl, etc.) doesn't apply here.

pub mod sys;

#[cfg(feature = "registry")]
pub mod decoder;
#[cfg(feature = "registry")]
pub mod encoder;

/// Register H.264 and HEVC hardware decode + encode factories via
/// VideoToolbox. If the framework cannot be loaded (older OS, sandboxed
/// environment, non-macOS) the function logs and returns without
/// registering anything — the runtime falls back to the pure-Rust impls.
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

    let _ = (h264_caps, hevc_caps); // suppress unused warnings
}

#[cfg(feature = "registry")]
oxideav_core::register!("videotoolbox", register);
