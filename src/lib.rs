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
//! Round 1 (this commit): scaffolding + libloading-based framework
//! loader + symbol-resolution smoke tests. No codecs wired yet.
//!
//! # Workspace policy
//!
//! Calling a system OS framework via FFI is the same shape as calling
//! `libc::malloc` — it's the platform, not a copied algorithm. The
//! workspace's clean-room rule (no embedding source from libvpx,
//! libwebp, libjxl, etc.) doesn't apply here.

pub mod sys;

/// Stable module path for the registry entry point. Round 2 will wire
/// in `VTDecompressionSession` for H.264 / HEVC / ProRes / MPEG-2 /
/// VP9 / AV1 (M3+) and `VTCompressionSession` for H.264 / HEVC /
/// ProRes / JPEG with priority 0 (preferred over the pure-Rust impls
/// at priority 100+).
#[cfg(feature = "registry")]
pub fn register(_ctx: &mut oxideav_core::RuntimeContext) {
    // Round 1: framework loads but no factories registered yet.
    let _ = sys::framework();
}
