//! VTCompressionSession-backed H.264 and HEVC encoders.
//!
//! # Design
//!
//! Each encoder holds a `VTCompressionSessionRef` built once at
//! construction time. `send_frame` wraps the I420 `VideoFrame` into a
//! biplanar NV12 `CVPixelBuffer` (VideoToolbox's preferred format) and
//! submits it. The compression callback is called synchronously (we call
//! `VTCompressionSessionCompleteFrames` after each encode) and stores the
//! resulting Annex-B packet in a queue that `receive_packet` drains.
//!
//! Output packets contain SPS/PPS (keyframes) or just slice data
//! exactly as VT produces them, which is already Annex-B compatible
//! once we re-prefix start codes.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, PixelFormat, Result, TimeBase};

use crate::sys::{self, CMTime, K_OS_STATUS_NO_ERROR};

// kCMVideoCodecType_H264 = 'avc1' = 0x61766331
const K_CM_VIDEO_CODEC_TYPE_H264: u32 = 0x61766331;
// kCMVideoCodecType_HEVC = 'hvc1' = 0x68766331
const K_CM_VIDEO_CODEC_TYPE_HEVC: u32 = 0x68766331;

// kVTCompressionPropertyKey_RealTime
const K_VT_REAL_TIME: &str = "RealTime";
// kVTCompressionPropertyKey_AverageBitRate (CFNumber, target bits-per-second)
const K_VT_AVERAGE_BIT_RATE: &str = "AverageBitRate";
// kVTCompressionPropertyKey_AllowFrameReordering
const K_VT_ALLOW_FRAME_REORDER: &str = "AllowFrameReordering";
// kVTCompressionPropertyKey_ProfileLevel (H.264 / HEVC)
const K_VT_PROFILE_LEVEL: &str = "ProfileLevel";
// kVTCompressionPropertyKey_Quality (CFNumber Float, 0.0..1.0)
const K_VT_QUALITY: &str = "Quality";
// kVTCompressionPropertyKey_MaxKeyFrameInterval (CFNumber<int>, frames)
const K_VT_MAX_KEY_FRAME_INTERVAL: &str = "MaxKeyFrameInterval";
// kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration (CFNumber<seconds>)
const K_VT_MAX_KEY_FRAME_INTERVAL_DURATION: &str = "MaxKeyFrameIntervalDuration";
// kVTCompressionPropertyKey_ExpectedFrameRate (CFNumber, fps)
const K_VT_EXPECTED_FRAME_RATE: &str = "ExpectedFrameRate";
// kVTCompressionPropertyKey_DataRateLimits (CFArray<CFNumber>, [bytes, seconds, ...])
const K_VT_DATA_RATE_LIMITS: &str = "DataRateLimits";
// kVTCompressionPropertyKey_ConstantBitRate (CFNumber bits per second, macOS 13.0+)
const K_VT_CONSTANT_BIT_RATE: &str = "ConstantBitRate";
// kVTProfileLevel_H264_Baseline_AutoLevel
const K_VT_H264_BASELINE: &str = "H264_Baseline_AutoLevel";
// kVTProfileLevel_HEVC_Main_AutoLevel
const K_VT_HEVC_MAIN: &str = "HEVC_Main_AutoLevel";

/// Parse `options["keyframe_interval"]` as a non-negative integer frame
/// count for `kVTCompressionPropertyKey_MaxKeyFrameInterval`. Per
/// `VTCompressionProperties.h`, the property is CFNumber<int> and
/// "0 means keyframes can be inserted on demand"; we accept 0 (caller
/// explicitly opts out of a forced cadence) up to `i32::MAX` and clamp
/// anything beyond. Returns `None` for unparseable / negative input so
/// the caller falls back to VT's built-in default (no upper bound).
pub(crate) fn parse_keyframe_interval(opt: &str) -> Option<i32> {
    let trimmed = opt.trim();
    let parsed: i64 = trimmed.parse().ok()?;
    if parsed < 0 {
        return None;
    }
    if parsed > i32::MAX as i64 {
        return Some(i32::MAX);
    }
    Some(parsed as i32)
}

/// Parse `options["keyframe_interval_duration"]` as a non-negative
/// Float64 seconds value for
/// `kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration`. Per the SDK
/// header the property is `CFNumber<seconds>` and a value of 0 disables
/// the duration-based cadence cap. Rejects NaN / negative / non-finite
/// input so the caller falls back to VT's default.
pub(crate) fn parse_keyframe_interval_duration(opt: &str) -> Option<f64> {
    let v: f64 = opt.trim().parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some(v)
}

/// Resolve `kVTCompressionPropertyKey_ExpectedFrameRate` (CFNumber, fps).
///
/// Precedence:
///   1. `params.options["expected_frame_rate"]` if it parses as a finite
///      strictly-positive Float64.
///   2. `params.frame_rate` (`Rational`) if non-zero and the resulting
///      `as_f64()` is finite and strictly positive.
///
/// The SDK header documents the property as a hint the encoder uses to
/// optimise rate-control and energy budgeting; the value need not match
/// the actual presentation rate, but Apple recommends setting it to the
/// real-time frame rate when the stream targets a known cadence.
pub(crate) fn resolve_expected_frame_rate(params: &CodecParameters) -> Option<f64> {
    if let Some(raw) = params.options.get("expected_frame_rate") {
        if let Ok(v) = raw.trim().parse::<f64>() {
            if v.is_finite() && v > 0.0 {
                return Some(v);
            }
        }
    }
    if let Some(r) = params.frame_rate {
        if r.den != 0 {
            let v = r.as_f64();
            if v.is_finite() && v > 0.0 {
                return Some(v);
            }
        }
    }
    None
}

/// Map a non-zero `OSStatus` from a VideoToolbox / CoreMedia / CoreVideo
/// call into a typed `oxideav_core::Error`, with the symbolic header
/// name (per `sys::describe_os_status`) in the message.
///
/// Classification:
///   * **Unsupported** — the "no such codec / not on this host / not
///     right now" family (`kVTCouldNotFindVideo{De,En}coderErr`,
///     `kVTVideo{De,En}coderNotAvailableNowErr`,
///     `kVTVideoDecoderUnsupportedDataFormatErr`,
///     `kVTPropertyNotSupportedErr`, `kVTPixelTransferNotSupportedErr`,
///     `kVTFormatDescriptionChangeNotSupportedErr`, and the two
///     needs-Rosetta statuses). The registry's software-fallback retry
///     keys off this: hardware "can't" is not hardware "broke".
///   * **Invalid** — caller/data-shaped failures (`kVTParameterErr`,
///     `kVTVideoDecoderBadDataErr`).
///   * **Other** — everything else (malfunctions, allocation failures,
///     invalidated sessions, unknown codes).
pub(crate) fn vt_error(context: &str, status: sys::OSStatus) -> Error {
    let msg = format!("{context}: OSStatus {}", sys::describe_os_status(status));
    match status {
        sys::K_VT_PROPERTY_NOT_SUPPORTED_ERR
        | sys::K_VT_PIXEL_TRANSFER_NOT_SUPPORTED_ERR
        | sys::K_VT_COULD_NOT_FIND_VIDEO_DECODER_ERR
        | sys::K_VT_COULD_NOT_FIND_VIDEO_ENCODER_ERR
        | sys::K_VT_VIDEO_DECODER_UNSUPPORTED_DATA_FORMAT_ERR
        | sys::K_VT_VIDEO_DECODER_NOT_AVAILABLE_NOW_ERR
        | sys::K_VT_VIDEO_ENCODER_NOT_AVAILABLE_NOW_ERR
        | sys::K_VT_FORMAT_DESCRIPTION_CHANGE_NOT_SUPPORTED_ERR
        | sys::K_VT_VIDEO_DECODER_NEEDS_ROSETTA_ERR
        | sys::K_VT_VIDEO_ENCODER_NEEDS_ROSETTA_ERR => Error::unsupported(msg),
        sys::K_VT_PARAMETER_ERR | sys::K_VT_VIDEO_DECODER_BAD_DATA_ERR => Error::invalid(msg),
        _ => Error::other(msg),
    }
}

/// Per-frame duration in microseconds (the encoder output packets'
/// `TimeBase::new(1, 1_000_000)`), derived from the same frame-rate
/// resolution as `resolve_expected_frame_rate`. `None` when the caller
/// supplied no usable cadence.
pub(crate) fn frame_duration_us(params: &CodecParameters) -> Option<i64> {
    let fps = resolve_expected_frame_rate(params)?;
    let dur = (1_000_000.0 / fps).round();
    if dur.is_finite() && dur >= 1.0 && dur <= i64::MAX as f64 {
        Some(dur as i64)
    } else {
        None
    }
}

/// One hard-cap segment of `kVTCompressionPropertyKey_DataRateLimits`.
/// Per `VideoToolbox/VTCompressionProperties.h`, "each hard limit is
/// described by a data size in bytes and a duration in seconds, and
/// requires that the total size of compressed data for any contiguous
/// segment of that duration (in decode time) must not exceed the data
/// size". The CFArray Apple expects is a flat alternating
/// `[bytes, seconds, bytes, seconds, ...]` of "an even number of
/// CFNumbers" — Apple documents up to two segments ("zero, one or two
/// hard limits").
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct DataRateLimit {
    pub bytes: i32,
    pub seconds: f64,
}

/// Parse `options["data_rate_limits"]` as a comma-separated list of
/// `bytes:seconds` pairs (one or two segments). Whitespace surrounding
/// each token is tolerated.
///
/// Examples (per the SDK header's hard-cap shape):
///   * `"100000:1"` — at most 100 000 bytes in any one-second window.
///   * `"100000:1, 500000:5"` — composable caps over a 1 s and 5 s window.
///
/// Rejects:
///   * Anything outside the 1–2-segment range Apple documents.
///   * Negative bytes / negative or non-positive seconds (the SDK rejects
///     these at `VTSessionSetProperty` time).
///   * Non-integer or `> i32::MAX` byte counts (the property uses
///     `CFNumber<SInt32>` per Apple's array-element typing convention;
///     callers that need a larger window per segment should split into
///     two segments).
///   * NaN / infinite seconds.
///   * Empty / unparseable strings (the encoder keeps VT's default of
///     "no data rate limits").
pub(crate) fn parse_data_rate_limits(opt: &str) -> Option<Vec<DataRateLimit>> {
    let trimmed = opt.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(2);
    for seg in trimmed.split(',') {
        let seg = seg.trim();
        if seg.is_empty() {
            return None;
        }
        let (bytes_str, seconds_str) = seg.split_once(':')?;
        let bytes: i64 = bytes_str.trim().parse().ok()?;
        if !(0..=i32::MAX as i64).contains(&bytes) {
            return None;
        }
        let seconds: f64 = seconds_str.trim().parse().ok()?;
        if !seconds.is_finite() || seconds <= 0.0 {
            return None;
        }
        out.push(DataRateLimit {
            bytes: bytes as i32,
            seconds,
        });
    }
    if out.is_empty() || out.len() > 2 {
        return None;
    }
    Some(out)
}

/// Parse `options["constant_bit_rate"]` as a non-negative integer
/// bits-per-second value for `kVTCompressionPropertyKey_ConstantBitRate`
/// (macOS 13.0+, per `VideoToolbox/VTCompressionProperties.h`).
///
/// The SDK header documents the property as `CFNumber bits per second`
/// and notes:
///   * CBR is intended for legacy CDN interop, not general streaming.
///   * Not compatible with `kVTCompressionPropertyKey_DataRateLimits` or
///     `kVTCompressionPropertyKey_AverageBitRate`.
///   * `kVTCompressionPropertyKey_ExpectedFrameRate` should be set
///     alongside CBR for effective rate control.
///   * Not all encoders or modes support CBR; setting the property on an
///     unsupported encoder returns `kVTPropertyNotSupportedErr`. The
///     bridge treats that as non-fatal (the prior behaviour stays in
///     effect), matching the round 9 / 13 pattern.
///
/// Returns `None` for negative / unparseable / non-numeric input so the
/// caller falls back to VT's default rate-control mode.
pub(crate) fn parse_constant_bit_rate(opt: &str) -> Option<i32> {
    let trimmed = opt.trim();
    let parsed: i64 = trimmed.parse().ok()?;
    if parsed < 0 {
        return None;
    }
    if parsed > i32::MAX as i64 {
        return Some(i32::MAX);
    }
    Some(parsed as i32)
}

/// Translate a free-form `options["profile"]` string to the canonical
/// `kVTProfileLevel_*` string Apple's VideoToolbox understands. The mapping
/// covers the public set declared in the macOS SDK header
/// `VideoToolbox/VTCompressionProperties.h` (the very same header the rest of
/// this crate already pins for `kVTCompressionPropertyKey_*` / property-key
/// strings).
///
/// Returns `None` for empty / unrecognised input so the caller falls back to
/// its built-in default. Inputs that already match the canonical
/// `H264_*` form (`"H264_High_AutoLevel"`, `"H264_Baseline_3_1"`, etc.) are
/// preserved via a pass-through — the prior round-9 implementation
/// deliberately accepted literal Apple strings but its `_ => None` fall-back
/// silently swallowed them, so the documented behaviour didn't actually
/// happen. Round 12 closes that gap.
///
/// Round 12 also expands the alias table from the four `_AutoLevel` short
/// names to the full set Apple declares:
///
/// * `baseline` / `baseline_1_3` / `baseline_3_0` … `baseline_5_2` —
///   `kVTProfileLevel_H264_Baseline_*` (macOS 10.8 / 10.9+, per SDK header).
/// * `main` / `main_3_0` … `main_5_2` — `kVTProfileLevel_H264_Main_*`.
/// * `high` / `high_3_0` … `high_5_2` — `kVTProfileLevel_H264_High_*`.
/// * `extended` / `extended_5_0` — `kVTProfileLevel_H264_Extended_*`.
/// * `constrained_baseline` / `constrained_high` —
///   `kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel` /
///   `kVTProfileLevel_H264_ConstrainedHigh_AutoLevel` (macOS 12.0+).
fn h264_profile_string(opt: &str) -> Option<&'static str> {
    let lower = opt.to_ascii_lowercase();
    if let Some(s) = h264_profile_named_level(&lower) {
        return Some(s);
    }
    // Pass-through for the canonical Apple form (`"H264_High_AutoLevel"`,
    // `"H264_Baseline_3_1"`, …). Only accept strings that exactly match a
    // value the SDK header documents, so an attacker can't drive arbitrary
    // junk through into a CFString property.
    if h264_canonical_value(opt) {
        return h264_static_canonical(opt);
    }
    None
}

/// Subset that accepts the short aliases (no level digits) and the level-suffix
/// aliases (`baseline_3_1`, `main_4_2`, `high_5_2`, etc.). Always lower-case
/// input.
fn h264_profile_named_level(lower: &str) -> Option<&'static str> {
    match lower {
        // Auto-level short names + tolerant variants.
        "baseline" | "baseline_auto" | "baseline_autolevel" => Some("H264_Baseline_AutoLevel"),
        "main" | "main_auto" | "main_autolevel" => Some("H264_Main_AutoLevel"),
        "high" | "high_auto" | "high_autolevel" => Some("H264_High_AutoLevel"),
        "extended" | "extended_auto" | "extended_autolevel" => Some("H264_Extended_AutoLevel"),
        "constrained_baseline"
        | "constrainedbaseline"
        | "constrained_baseline_auto"
        | "constrained_baseline_autolevel" => Some("H264_ConstrainedBaseline_AutoLevel"),
        "constrained_high"
        | "constrainedhigh"
        | "constrained_high_auto"
        | "constrained_high_autolevel" => Some("H264_ConstrainedHigh_AutoLevel"),
        // Baseline named-level variants (per SDK header).
        "baseline_1_3" => Some("H264_Baseline_1_3"),
        "baseline_3_0" => Some("H264_Baseline_3_0"),
        "baseline_3_1" => Some("H264_Baseline_3_1"),
        "baseline_3_2" => Some("H264_Baseline_3_2"),
        "baseline_4_0" => Some("H264_Baseline_4_0"),
        "baseline_4_1" => Some("H264_Baseline_4_1"),
        "baseline_4_2" => Some("H264_Baseline_4_2"),
        "baseline_5_0" => Some("H264_Baseline_5_0"),
        "baseline_5_1" => Some("H264_Baseline_5_1"),
        "baseline_5_2" => Some("H264_Baseline_5_2"),
        // Main named-level variants.
        "main_3_0" => Some("H264_Main_3_0"),
        "main_3_1" => Some("H264_Main_3_1"),
        "main_3_2" => Some("H264_Main_3_2"),
        "main_4_0" => Some("H264_Main_4_0"),
        "main_4_1" => Some("H264_Main_4_1"),
        "main_4_2" => Some("H264_Main_4_2"),
        "main_5_0" => Some("H264_Main_5_0"),
        "main_5_1" => Some("H264_Main_5_1"),
        "main_5_2" => Some("H264_Main_5_2"),
        // High named-level variants.
        "high_3_0" => Some("H264_High_3_0"),
        "high_3_1" => Some("H264_High_3_1"),
        "high_3_2" => Some("H264_High_3_2"),
        "high_4_0" => Some("H264_High_4_0"),
        "high_4_1" => Some("H264_High_4_1"),
        "high_4_2" => Some("H264_High_4_2"),
        "high_5_0" => Some("H264_High_5_0"),
        "high_5_1" => Some("H264_High_5_1"),
        "high_5_2" => Some("H264_High_5_2"),
        // Extended named-level variant.
        "extended_5_0" => Some("H264_Extended_5_0"),
        _ => None,
    }
}

/// Case-sensitive set of canonical `H264_*` strings that VT documents in
/// `VTCompressionProperties.h`. Used by the pass-through arm so the caller
/// can supply the literal SDK string instead of one of our short aliases.
fn h264_canonical_value(s: &str) -> bool {
    matches!(
        s,
        "H264_Baseline_AutoLevel"
            | "H264_Main_AutoLevel"
            | "H264_High_AutoLevel"
            | "H264_Extended_AutoLevel"
            | "H264_ConstrainedBaseline_AutoLevel"
            | "H264_ConstrainedHigh_AutoLevel"
            | "H264_Baseline_1_3"
            | "H264_Baseline_3_0"
            | "H264_Baseline_3_1"
            | "H264_Baseline_3_2"
            | "H264_Baseline_4_0"
            | "H264_Baseline_4_1"
            | "H264_Baseline_4_2"
            | "H264_Baseline_5_0"
            | "H264_Baseline_5_1"
            | "H264_Baseline_5_2"
            | "H264_Main_3_0"
            | "H264_Main_3_1"
            | "H264_Main_3_2"
            | "H264_Main_4_0"
            | "H264_Main_4_1"
            | "H264_Main_4_2"
            | "H264_Main_5_0"
            | "H264_Main_5_1"
            | "H264_Main_5_2"
            | "H264_High_3_0"
            | "H264_High_3_1"
            | "H264_High_3_2"
            | "H264_High_4_0"
            | "H264_High_4_1"
            | "H264_High_4_2"
            | "H264_High_5_0"
            | "H264_High_5_1"
            | "H264_High_5_2"
            | "H264_Extended_5_0"
    )
}

/// Return the `'static` form of a validated canonical H.264 string. The Apple
/// values are themselves `'static`; we just need to translate the runtime
/// `&str` parameter into a known compile-time literal so the rest of the API
/// can keep its `Option<&'static str>` shape.
fn h264_static_canonical(s: &str) -> Option<&'static str> {
    Some(match s {
        "H264_Baseline_AutoLevel" => "H264_Baseline_AutoLevel",
        "H264_Main_AutoLevel" => "H264_Main_AutoLevel",
        "H264_High_AutoLevel" => "H264_High_AutoLevel",
        "H264_Extended_AutoLevel" => "H264_Extended_AutoLevel",
        "H264_ConstrainedBaseline_AutoLevel" => "H264_ConstrainedBaseline_AutoLevel",
        "H264_ConstrainedHigh_AutoLevel" => "H264_ConstrainedHigh_AutoLevel",
        "H264_Baseline_1_3" => "H264_Baseline_1_3",
        "H264_Baseline_3_0" => "H264_Baseline_3_0",
        "H264_Baseline_3_1" => "H264_Baseline_3_1",
        "H264_Baseline_3_2" => "H264_Baseline_3_2",
        "H264_Baseline_4_0" => "H264_Baseline_4_0",
        "H264_Baseline_4_1" => "H264_Baseline_4_1",
        "H264_Baseline_4_2" => "H264_Baseline_4_2",
        "H264_Baseline_5_0" => "H264_Baseline_5_0",
        "H264_Baseline_5_1" => "H264_Baseline_5_1",
        "H264_Baseline_5_2" => "H264_Baseline_5_2",
        "H264_Main_3_0" => "H264_Main_3_0",
        "H264_Main_3_1" => "H264_Main_3_1",
        "H264_Main_3_2" => "H264_Main_3_2",
        "H264_Main_4_0" => "H264_Main_4_0",
        "H264_Main_4_1" => "H264_Main_4_1",
        "H264_Main_4_2" => "H264_Main_4_2",
        "H264_Main_5_0" => "H264_Main_5_0",
        "H264_Main_5_1" => "H264_Main_5_1",
        "H264_Main_5_2" => "H264_Main_5_2",
        "H264_High_3_0" => "H264_High_3_0",
        "H264_High_3_1" => "H264_High_3_1",
        "H264_High_3_2" => "H264_High_3_2",
        "H264_High_4_0" => "H264_High_4_0",
        "H264_High_4_1" => "H264_High_4_1",
        "H264_High_4_2" => "H264_High_4_2",
        "H264_High_5_0" => "H264_High_5_0",
        "H264_High_5_1" => "H264_High_5_1",
        "H264_High_5_2" => "H264_High_5_2",
        "H264_Extended_5_0" => "H264_Extended_5_0",
        _ => return None,
    })
}

/// Same shape as `h264_profile_string` for the HEVC encoder. Apple's SDK
/// header `VideoToolbox/VTCompressionProperties.h` declares (as of macOS
/// 14.2 SDK):
///
/// * `kVTProfileLevel_HEVC_Main_AutoLevel` (macOS 10.13+).
/// * `kVTProfileLevel_HEVC_Main10_AutoLevel` (macOS 10.13+).
/// * `kVTProfileLevel_HEVC_Main42210_AutoLevel` (macOS 12.3+).
///
/// Note: the **runtime CFString value** of the third one is
/// `"HEVC_Main42210_AutoLevel"` — i.e. five contiguous digits, *not*
/// `"HEVC_Main4_2_2_10_AutoLevel"` (which the round-9 alias map emitted; VT
/// would have refused to recognise that string and silently kept the default
/// Main profile). Round 12 fixes the alias's output to the actual SDK value
/// while keeping the documented input alias `main4_2_2_10` working.
fn hevc_profile_string(opt: &str) -> Option<&'static str> {
    let lower = opt.to_ascii_lowercase();
    match lower.as_str() {
        "main" | "main_auto" | "main_autolevel" => return Some("HEVC_Main_AutoLevel"),
        "main10" | "main_10" | "main10_auto" | "main10_autolevel" => {
            return Some("HEVC_Main10_AutoLevel")
        }
        // Round 9 input aliases — kept verbatim.
        "main4_2_2_10" | "main422_10" => return Some("HEVC_Main42210_AutoLevel"),
        // New input aliases — the canonical SDK-symbol form and the
        // value-form (no underscores).
        "main42210" | "main_42210" | "main_4_2_2_10_autolevel" => {
            return Some("HEVC_Main42210_AutoLevel")
        }
        "" => return None,
        _ => {}
    }
    // Canonical-pass-through: accept the literal Apple CFString value.
    match opt {
        "HEVC_Main_AutoLevel" => Some("HEVC_Main_AutoLevel"),
        "HEVC_Main10_AutoLevel" => Some("HEVC_Main10_AutoLevel"),
        "HEVC_Main42210_AutoLevel" => Some("HEVC_Main42210_AutoLevel"),
        _ => None,
    }
}

// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange = '420v'
const K_CV_PIXEL_FORMAT_NV12: u32 = 0x34323076;

// ─────────────────────────── Callback state ───────────────────────────────────

struct EncCallbackState {
    /// `(annex_b_data, is_keyframe)` per encoded access unit. The
    /// keyframe bit comes from the NAL-type scan in `extract_annex_b`
    /// (IDR for H.264; IDR_W_RADL / IDR_N_LP / CRA for HEVC).
    packets: VecDeque<(Vec<u8>, bool)>,
    error: Option<String>,
    is_hevc: bool,
}

impl EncCallbackState {
    fn new(is_hevc: bool) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            packets: VecDeque::new(),
            error: None,
            is_hevc,
        }))
    }
}

/// Extract H.264 parameter sets (SPS + PPS) from a CMVideoFormatDescription
/// and return them as Annex-B bytes (start code + raw NAL).
unsafe fn extract_h264_param_sets(
    vt: &sys::Vtable,
    fmt_desc: sys::CMVideoFormatDescriptionRef,
) -> Vec<u8> {
    let mut out = Vec::new();
    // Query count first.
    let mut count: usize = 0;
    let mut _ptr: *const u8 = std::ptr::null();
    let mut _size: usize = 0;
    let mut _nal_len: i32 = 0;
    unsafe {
        (vt.cm_fmt_h264_param_at_idx)(
            fmt_desc,
            0,
            &mut _ptr,
            &mut _size,
            &mut count,
            &mut _nal_len,
        );
    }
    for i in 0..count {
        let mut ptr: *const u8 = std::ptr::null();
        let mut size: usize = 0;
        let st = unsafe {
            (vt.cm_fmt_h264_param_at_idx)(
                fmt_desc,
                i,
                &mut ptr,
                &mut size,
                &mut std::mem::zeroed(),
                &mut std::mem::zeroed(),
            )
        };
        if st == 0 && !ptr.is_null() && size > 0 {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
        }
    }
    out
}

/// Extract HEVC parameter sets (VPS + SPS + PPS) from a CMVideoFormatDescription.
unsafe fn extract_hevc_param_sets(
    vt: &sys::Vtable,
    fmt_desc: sys::CMVideoFormatDescriptionRef,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut count: usize = 0;
    let mut _ptr: *const u8 = std::ptr::null();
    let mut _size: usize = 0;
    let mut _nal_len: i32 = 0;
    unsafe {
        (vt.cm_fmt_hevc_param_at_idx)(
            fmt_desc,
            0,
            &mut _ptr,
            &mut _size,
            &mut count,
            &mut _nal_len,
        );
    }
    for i in 0..count {
        let mut ptr: *const u8 = std::ptr::null();
        let mut size: usize = 0;
        let st = unsafe {
            (vt.cm_fmt_hevc_param_at_idx)(
                fmt_desc,
                i,
                &mut ptr,
                &mut size,
                &mut std::mem::zeroed(),
                &mut std::mem::zeroed(),
            )
        };
        if st == 0 && !ptr.is_null() && size > 0 {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
        }
    }
    out
}

/// Extract Annex-B data from a CMSampleBuffer produced by VT.
///
/// VT encodes output as AVCC (length-prefixed NAL units). We convert
/// to Annex-B (start-code prefixed) for portability.
/// If `is_hevc` and the format description contains parameter sets, they are
/// prepended on every keyframe (IDR / CRA) packet.
///
/// Returns `(annex_b_data, is_keyframe)` — the keyframe bit is derived
/// from the VCL NAL types in the access unit (IDR for H.264;
/// IDR_W_RADL / IDR_N_LP / CRA for HEVC) and flows into the output
/// `Packet`'s `flags.keyframe`.
unsafe fn extract_annex_b(
    vt: &sys::Vtable,
    sample_buffer: sys::CMSampleBufferRef,
    is_hevc: bool,
) -> Result<(Vec<u8>, bool)> {
    let block_buf = unsafe { (vt.cm_sample_get_data_buffer)(sample_buffer) };
    if block_buf.is_null() {
        return Err(Error::other("CMSampleBufferGetDataBuffer returned null"));
    }

    let total_len = unsafe { (vt.cm_block_get_data_length)(block_buf) };
    let mut avcc_data = vec![0u8; total_len];

    let status = unsafe {
        (vt.cm_block_copy_data)(
            block_buf,
            0,
            total_len,
            avcc_data.as_mut_ptr() as *mut c_void,
        )
    };
    if status != K_OS_STATUS_NO_ERROR {
        return Err(vt_error("CMBlockBufferCopyDataBytes", status));
    }

    // Convert AVCC → Annex-B: replace each 4-byte big-endian length
    // with a 4-byte start code (00 00 00 01).
    let mut out = Vec::with_capacity(total_len + 256);
    let mut pos = 0usize;
    let mut is_keyframe = false;

    while pos + 4 <= avcc_data.len() {
        let nal_len = u32::from_be_bytes([
            avcc_data[pos],
            avcc_data[pos + 1],
            avcc_data[pos + 2],
            avcc_data[pos + 3],
        ]) as usize;
        pos += 4;
        if pos + nal_len > avcc_data.len() {
            break;
        }
        let nal = &avcc_data[pos..pos + nal_len];
        // Detect keyframe NAL types.
        if !nal.is_empty() {
            if is_hevc {
                let nal_type = (nal[0] >> 1) & 0x3F;
                // IDR_W_RADL=19, IDR_N_LP=20, CRA=21
                if nal_type == 19 || nal_type == 20 || nal_type == 21 {
                    is_keyframe = true;
                }
            } else {
                let nal_type = nal[0] & 0x1F;
                // IDR=5
                if nal_type == 5 {
                    is_keyframe = true;
                }
            }
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(nal);
        pos += nal_len;
    }

    // Prepend parameter sets on keyframes (VT doesn't include them in the
    // AVCC data payload — they live in the CMVideoFormatDescription).
    if is_keyframe {
        let fmt_desc = unsafe { (vt.cm_sample_get_format_desc)(sample_buffer) };
        if !fmt_desc.is_null() {
            let params = if is_hevc {
                unsafe { extract_hevc_param_sets(vt, fmt_desc) }
            } else {
                unsafe { extract_h264_param_sets(vt, fmt_desc) }
            };
            if !params.is_empty() {
                let mut combined = params;
                combined.extend_from_slice(&out);
                return Ok((combined, is_keyframe));
            }
        }
    }

    Ok((out, is_keyframe))
}

/// Extern-C callback — called by VT with each encoded sample buffer.
unsafe extern "C" fn comp_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: u32,
    sample_buffer: sys::CMSampleBufferRef,
) {
    let state_ptr = output_callback_ref_con as *const Mutex<EncCallbackState>;
    let state = unsafe { &*state_ptr };
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    if status != K_OS_STATUS_NO_ERROR {
        guard.error = Some(format!(
            "VT encode callback: OSStatus {}",
            sys::describe_os_status(status)
        ));
        return;
    }

    if sample_buffer.is_null() {
        return;
    }

    let vt = match sys::vtable() {
        Ok(v) => v,
        Err(e) => {
            guard.error = Some(format!("vtable unavailable in encode callback: {e}"));
            return;
        }
    };

    let is_hevc = guard.is_hevc;
    match unsafe { extract_annex_b(vt, sample_buffer, is_hevc) } {
        Ok(entry) => guard.packets.push_back(entry),
        Err(e) => guard.error = Some(e.to_string()),
    }
}

// ─────────────────────────── VtEncoder ────────────────────────────────────────

pub struct VtEncoder {
    codec_id: CodecId,
    session: sys::VTCompressionSessionRef,
    state: Arc<Mutex<EncCallbackState>>,
    output_queue: VecDeque<Packet>,
    output_params: CodecParameters,
    pts_counter: i64,
    width: usize,
    height: usize,
    /// Per-frame duration in the output time base (µs), derived from the
    /// caller's frame rate (`options["expected_frame_rate"]` or
    /// `params.frame_rate`). `None` when no cadence is known — output
    /// packets then carry `duration: None` and the CMTime duration falls
    /// back to a nominal 1/30 s.
    frame_duration_us: Option<i64>,
}

// SAFETY: VTCompressionSessionRef is thread-safe per Apple docs.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new_h264(params: &CodecParameters) -> Result<Box<dyn oxideav_core::Encoder>> {
        Self::create(
            "h264",
            K_CM_VIDEO_CODEC_TYPE_H264,
            K_VT_H264_BASELINE,
            false,
            params,
        )
    }

    pub fn new_hevc(params: &CodecParameters) -> Result<Box<dyn oxideav_core::Encoder>> {
        Self::create(
            "hevc",
            K_CM_VIDEO_CODEC_TYPE_HEVC,
            K_VT_HEVC_MAIN,
            true,
            params,
        )
    }

    fn create(
        codec_id_str: &str,
        codec_type: u32,
        profile_level: &str,
        is_hevc: bool,
        params: &CodecParameters,
    ) -> Result<Box<dyn oxideav_core::Encoder>> {
        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;

        let width = params.width.unwrap_or(320) as usize;
        let height = params.height.unwrap_or(240) as usize;

        let state = EncCallbackState::new(is_hevc);
        let state_raw = Arc::into_raw(Arc::clone(&state)) as *mut c_void;

        let mut session: sys::VTCompressionSessionRef = std::ptr::null_mut();
        let status = unsafe {
            (vt.vt_comp_create)(
                std::ptr::null_mut(),
                width as i32,
                height as i32,
                codec_type,
                std::ptr::null_mut(), // encoder specification
                std::ptr::null_mut(), // source image buffer attributes
                std::ptr::null_mut(), // compressed data allocator
                comp_callback,
                state_raw,
                &mut session,
            )
        };

        if status != K_OS_STATUS_NO_ERROR {
            // Reclaim the leaked Arc.
            let _ = unsafe { Arc::from_raw(state_raw as *const Mutex<EncCallbackState>) };
            return Err(vt_error(
                &format!("VTCompressionSessionCreate (codec_type 0x{codec_type:08x})"),
                status,
            ));
        }

        // Configure properties.
        // Allow frame reordering = false (reduces latency, simpler test).
        let bool_false = unsafe { sys::cf_number_i32(vt, 0) };
        let reorder_key = unsafe { sys::cf_string(vt, K_VT_ALLOW_FRAME_REORDER) };
        unsafe {
            (vt.vt_session_set_property)(session, reorder_key, bool_false);
            (vt.cf_release)(reorder_key);
            (vt.cf_release)(bool_false);
        }

        // Profile level — `options["profile"]` (case-insensitive) overrides
        // the codec's built-in default. Unknown / empty falls back to the
        // default the caller passed (`profile_level`).
        let resolved_profile = params
            .options
            .get("profile")
            .and_then(|p| {
                if is_hevc {
                    hevc_profile_string(p)
                } else {
                    h264_profile_string(p)
                }
            })
            .unwrap_or(profile_level);
        let profile_cf = unsafe { sys::cf_string(vt, resolved_profile) };
        let profile_key = unsafe { sys::cf_string(vt, K_VT_PROFILE_LEVEL) };
        unsafe {
            (vt.vt_session_set_property)(session, profile_key, profile_cf);
            (vt.cf_release)(profile_key);
            (vt.cf_release)(profile_cf);
        }

        // AverageBitRate — when the caller sets `CodecParameters::bit_rate`,
        // forward it to `kVTCompressionPropertyKey_AverageBitRate` as a
        // CFNumber-i32 (bits per second). Saturating cast keeps values
        // > 2^31 clamped to `i32::MAX`. Apple's H.264 / HEVC encoders
        // accept the property; failure is non-fatal.
        if let Some(bps) = params.bit_rate {
            let clamped = bps.min(i32::MAX as u64) as i32;
            let br_val = unsafe { sys::cf_number_i32(vt, clamped) };
            let br_key = unsafe { sys::cf_string(vt, K_VT_AVERAGE_BIT_RATE) };
            unsafe {
                (vt.vt_session_set_property)(session, br_key, br_val);
                (vt.cf_release)(br_key);
                (vt.cf_release)(br_val);
            }
        }

        // Real-time = true (kCFBooleanTrue = a special CF singleton, but VT
        // also accepts a CFNumber 1).
        let bool_true = unsafe { sys::cf_number_i32(vt, 1) };
        let rt_key = unsafe { sys::cf_string(vt, K_VT_REAL_TIME) };
        unsafe {
            (vt.vt_session_set_property)(session, rt_key, bool_true);
            (vt.cf_release)(rt_key);
            (vt.cf_release)(bool_true);
        }

        // Quality knob — `options["quality"]` parsed as a Float32 in
        // `[0.0, 1.0]`. Out-of-range / unparseable values are ignored.
        // Apple's H.264 / HEVC encoders document this as a hint that
        // interacts with the rate-control mode (it is the primary knob
        // in constant-quality mode and biases the encoder otherwise).
        if let Some(q_raw) = params.options.get("quality") {
            if let Ok(q) = q_raw.parse::<f32>() {
                if q.is_finite() && (0.0..=1.0).contains(&q) {
                    let q_val = unsafe { sys::cf_number_f32(vt, q) };
                    let q_key = unsafe { sys::cf_string(vt, K_VT_QUALITY) };
                    unsafe {
                        (vt.vt_session_set_property)(session, q_key, q_val);
                        (vt.cf_release)(q_key);
                        (vt.cf_release)(q_val);
                    }
                }
            }
        }

        // MaxKeyFrameInterval — `options["keyframe_interval"]` is the
        // maximum frame count between keyframes (CFNumber<int> per
        // `VTCompressionProperties.h`). Per the SDK doc, 0 means "no
        // forced cadence cap; keyframes are inserted on demand". The
        // parser clamps anything beyond `i32::MAX` to the SDK's natural
        // upper bound; unparseable / negative input is ignored so the
        // encoder keeps VT's built-in default (also "no cap" on Apple
        // hardware encoders unless the caller asks for one).
        if let Some(kfi_raw) = params.options.get("keyframe_interval") {
            if let Some(kfi) = parse_keyframe_interval(kfi_raw) {
                let kfi_val = unsafe { sys::cf_number_i32(vt, kfi) };
                let kfi_key = unsafe { sys::cf_string(vt, K_VT_MAX_KEY_FRAME_INTERVAL) };
                unsafe {
                    (vt.vt_session_set_property)(session, kfi_key, kfi_val);
                    (vt.cf_release)(kfi_key);
                    (vt.cf_release)(kfi_val);
                }
            }
        }

        // MaxKeyFrameIntervalDuration — `options["keyframe_interval_duration"]`
        // is the maximum wall-clock seconds between keyframes
        // (CFNumber<seconds> per the SDK header). Apple documents both
        // duration- and frame-count-based caps as composable: VT picks
        // whichever forces a keyframe first. 0 disables the duration cap.
        if let Some(kfd_raw) = params.options.get("keyframe_interval_duration") {
            if let Some(kfd) = parse_keyframe_interval_duration(kfd_raw) {
                let kfd_val = unsafe { sys::cf_number_f64(vt, kfd) };
                let kfd_key = unsafe { sys::cf_string(vt, K_VT_MAX_KEY_FRAME_INTERVAL_DURATION) };
                unsafe {
                    (vt.vt_session_set_property)(session, kfd_key, kfd_val);
                    (vt.cf_release)(kfd_key);
                    (vt.cf_release)(kfd_val);
                }
            }
        }

        // ExpectedFrameRate — caller-supplied via `options["expected_frame_rate"]`
        // (Float64 fps) or, when absent, derived from `params.frame_rate`
        // (the stream's container-level cadence). Per the SDK header this
        // is a hint the encoder uses for rate-control and energy
        // optimisation; setting it incorrectly does not break output but
        // mis-tunes the bit-rate envelope on streams with a stable cadence.
        if let Some(efr) = resolve_expected_frame_rate(params) {
            let efr_val = unsafe { sys::cf_number_f64(vt, efr) };
            let efr_key = unsafe { sys::cf_string(vt, K_VT_EXPECTED_FRAME_RATE) };
            unsafe {
                (vt.vt_session_set_property)(session, efr_key, efr_val);
                (vt.cf_release)(efr_key);
                (vt.cf_release)(efr_val);
            }
        }

        // DataRateLimits — `options["data_rate_limits"]` parsed as a
        // comma-separated list of `bytes:seconds` pairs. Per the SDK
        // header the property is `CFArray[CFNumber]` of alternating
        // bytes-and-seconds entries, with "zero, one or two hard
        // limits". The bridge clamps unsupported input to the parser's
        // documented rejection set so VT receives a well-formed array
        // or no array at all (the encoder keeps its default rate
        // control).
        if let Some(drl_raw) = params.options.get("data_rate_limits") {
            if let Some(segments) = parse_data_rate_limits(drl_raw) {
                // Build a CFArray of CFNumbers alternating bytes-i32,
                // seconds-Float64. CoreFoundation copies each value at
                // CFNumberCreate time; we release every element after the
                // array retains it via `kCFTypeArrayCallBacks`.
                let mut elements: Vec<sys::CFTypeRef> = Vec::with_capacity(segments.len() * 2);
                for seg in &segments {
                    elements.push(unsafe { sys::cf_number_i32(vt, seg.bytes) });
                    elements.push(unsafe { sys::cf_number_f64(vt, seg.seconds) });
                }
                let arr = unsafe { sys::cf_array(vt, &elements) };
                let drl_key = unsafe { sys::cf_string(vt, K_VT_DATA_RATE_LIMITS) };
                unsafe {
                    (vt.vt_session_set_property)(session, drl_key, arr);
                    (vt.cf_release)(drl_key);
                    (vt.cf_release)(arr);
                    for e in elements {
                        (vt.cf_release)(e);
                    }
                }
            }
        }

        // ConstantBitRate — `options["constant_bit_rate"]` parsed as a
        // non-negative i32 bits-per-second value (CFNumber, macOS
        // 13.0+). The SDK header notes CBR is for legacy-CDN interop
        // and is mutually exclusive with `AverageBitRate` and
        // `DataRateLimits`. Older macOS (pre-13) or encoders that lack
        // CBR support return `kVTPropertyNotSupportedErr` at
        // `VTSessionSetProperty`; we treat that as non-fatal so the
        // encoder keeps its default mode, matching every prior
        // round-9 / round-13 knob's failure semantics.
        if let Some(cbr_raw) = params.options.get("constant_bit_rate") {
            if let Some(cbr) = parse_constant_bit_rate(cbr_raw) {
                let cbr_val = unsafe { sys::cf_number_i32(vt, cbr) };
                let cbr_key = unsafe { sys::cf_string(vt, K_VT_CONSTANT_BIT_RATE) };
                unsafe {
                    (vt.vt_session_set_property)(session, cbr_key, cbr_val);
                    (vt.cf_release)(cbr_key);
                    (vt.cf_release)(cbr_val);
                }
            }
        }

        // Prepare.
        let prep_status = unsafe { (vt.vt_comp_prepare)(session) };
        if prep_status != K_OS_STATUS_NO_ERROR {
            // Non-fatal on older macOS; ignore.
        }

        let mut output_params = CodecParameters::video(CodecId::new(codec_id_str));
        output_params.width = Some(width as u32);
        output_params.height = Some(height as u32);
        output_params.pixel_format = Some(PixelFormat::Yuv420P);
        output_params.frame_rate = params.frame_rate;
        output_params.bit_rate = params.bit_rate;

        Ok(Box::new(VtEncoder {
            codec_id: CodecId::new(codec_id_str),
            session,
            state,
            output_queue: VecDeque::new(),
            output_params,
            pts_counter: 0,
            width,
            height,
            frame_duration_us: frame_duration_us(params),
        }))
    }

    /// Convert an I420 `VideoFrame` → biplanar NV12 `CVPixelBuffer`.
    fn frame_to_pixel_buffer(
        &self,
        vt: &sys::Vtable,
        frame: &oxideav_core::VideoFrame,
    ) -> Result<sys::CVPixelBufferRef> {
        i420_to_nv12_pixel_buffer(vt, frame, self.width, self.height)
    }
}

/// Number of NV12 plane-copy allocations currently owned by outstanding
/// `CVPixelBuffer`s (created below, freed by the release callback).
/// Every submitted frame must eventually return to zero — the
/// `plane_boxes_released` test pins that the release callback actually
/// fires and frees.
pub(crate) static LIVE_PLANE_BOXES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Heap-owned NV12 plane copies backing a `CVPixelBuffer`; freed by
/// `release_planes` when CoreVideo destroys the buffer.
struct PlaneBoxes {
    _y: Box<[u8]>,
    _uv: Box<[u8]>,
}

impl PlaneBoxes {
    fn new(y: Box<[u8]>, uv: Box<[u8]>) -> Self {
        LIVE_PLANE_BOXES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self { _y: y, _uv: uv }
    }
}

impl Drop for PlaneBoxes {
    fn drop(&mut self) {
        LIVE_PLANE_BOXES.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// `CVPixelBufferReleasePlanarBytesCallback` — invoked by CoreVideo when
/// the pixel buffer is destroyed. Reclaims the `PlaneBoxes` allocation
/// passed as `release_ref_con`. The signature matches the five-parameter
/// planar-bytes callback prototype in `CVPixelBuffer.h` (an earlier
/// revision used the two-parameter non-planar shape *and* never freed
/// the refcon, leaking the full NV12 frame copy on every encoded frame).
unsafe extern "C" fn release_planes(
    release_ref_con: *mut c_void,
    _data_ptr: *const c_void,
    _data_size: usize,
    _number_of_planes: usize,
    _plane_addresses: *const *const c_void,
) {
    if !release_ref_con.is_null() {
        // SAFETY: `release_ref_con` is the `Box::into_raw(PlaneBoxes)`
        // this module handed to `CVPixelBufferCreateWithPlanarBytes`;
        // CoreVideo invokes the release callback exactly once.
        let _ = unsafe { Box::from_raw(release_ref_con as *mut PlaneBoxes) };
    }
}

/// Convert an I420 `VideoFrame` → biplanar NV12 `CVPixelBuffer`
/// (`'420v'`), the layout VT's hardware encoders prefer. Shared by the
/// H.264 / HEVC path here and the MJPEG / ProRes path in `blob.rs`.
///
/// The NV12 copy is heap-owned (`PlaneBoxes`) and handed to CoreVideo
/// with a release callback; it is freed when the last reference to the
/// pixel buffer goes away.
pub(crate) fn i420_to_nv12_pixel_buffer(
    vt: &sys::Vtable,
    frame: &oxideav_core::VideoFrame,
    width: usize,
    height: usize,
) -> Result<sys::CVPixelBufferRef> {
    if frame.planes.len() < 3 {
        return Err(Error::invalid("expected I420 frame with 3 planes"));
    }

    let y_plane = &frame.planes[0];
    let u_plane = &frame.planes[1];
    let v_plane = &frame.planes[2];

    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);

    // Build NV12: Y plane + interleaved UV.
    let y_len = width * height;
    let uv_len = chroma_w * 2 * chroma_h;

    let mut y_data: Vec<u8> = vec![0u8; y_len];
    let mut uv_data: Vec<u8> = vec![0u8; uv_len];

    // Copy Y (possibly re-stride to `width`).
    let y_rows = y_plane
        .data
        .len()
        .checked_div(y_plane.stride)
        .map(|r| height.min(r))
        .unwrap_or(0);
    for row in 0..y_rows {
        let src_start = row * y_plane.stride;
        let dst_start = row * width;
        let copy_len = width.min(y_plane.stride);
        if src_start + copy_len <= y_plane.data.len() && dst_start + copy_len <= y_len {
            y_data[dst_start..dst_start + copy_len]
                .copy_from_slice(&y_plane.data[src_start..src_start + copy_len]);
        }
    }

    // Interleave U + V → UV.
    for row in 0..chroma_h {
        let u_src = row * u_plane.stride;
        let v_src = row * v_plane.stride;
        let uv_dst = row * chroma_w * 2;
        for col in 0..chroma_w {
            let u_val = if u_src + col < u_plane.data.len() {
                u_plane.data[u_src + col]
            } else {
                128
            };
            let v_val = if v_src + col < v_plane.data.len() {
                v_plane.data[v_src + col]
            } else {
                128
            };
            uv_data[uv_dst + col * 2] = u_val;
            uv_data[uv_dst + col * 2 + 1] = v_val;
        }
    }

    let mut y_boxed = y_data.into_boxed_slice();
    let mut uv_boxed = uv_data.into_boxed_slice();

    let mut plane_ptrs: [*mut c_void; 2] = [
        y_boxed.as_mut_ptr() as *mut c_void,
        uv_boxed.as_mut_ptr() as *mut c_void,
    ];
    let plane_widths: [usize; 2] = [width, chroma_w];
    let plane_heights: [usize; 2] = [height, chroma_h];
    let plane_bpr: [usize; 2] = [width, chroma_w * 2];

    let boxes_raw = Box::into_raw(Box::new(PlaneBoxes::new(y_boxed, uv_boxed))) as *mut c_void;

    let mut pixel_buf: sys::CVPixelBufferRef = std::ptr::null_mut();
    let ret = unsafe {
        (vt.cv_pb_create_planar)(
            std::ptr::null_mut(),
            width,
            height,
            K_CV_PIXEL_FORMAT_NV12,
            std::ptr::null_mut(), // dataPtr (base of all planes combined, can be NULL)
            0,                    // dataSize
            2,
            plane_ptrs.as_mut_ptr(),
            plane_widths.as_ptr(),
            plane_heights.as_ptr(),
            plane_bpr.as_ptr(),
            Some(release_planes),
            boxes_raw,
            std::ptr::null_mut(),
            &mut pixel_buf,
        )
    };

    if ret != 0 {
        // Free the boxes ourselves since the callback won't be called.
        let _ = unsafe { Box::from_raw(boxes_raw as *mut PlaneBoxes) };
        return Err(vt_error("CVPixelBufferCreateWithPlanarBytes", ret));
    }

    Ok(pixel_buf)
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        if self.session.is_null() {
            return;
        }
        if let Ok(vt) = sys::vtable() {
            // Per VTCompressionSession.h: invalidate to tear the session
            // down, then CFRelease the object reference — sessions are CF
            // objects and invalidating alone leaks them.
            unsafe {
                (vt.vt_comp_invalidate)(self.session);
                (vt.cf_release)(self.session);
            }
            // Balance the `Arc::into_raw(Arc::clone(&state))` handed to
            // `VTCompressionSessionCreate` as the callback refcon.
            // Invalidate guarantees no further callback invocations, so
            // the raw pointer (== `Arc::as_ptr(&self.state)`) can be
            // reclaimed; `self.state` still holds its own reference, so
            // the allocation stays alive until the struct finishes
            // dropping.
            let _ = unsafe { Arc::from_raw(Arc::as_ptr(&self.state)) };
        }
    }
}

impl oxideav_core::Encoder for VtEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.output_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            _ => return Err(Error::invalid("expected Video frame")),
        };

        let pts = vf.pts.unwrap_or(self.pts_counter);
        self.pts_counter += 1;

        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;

        let pixel_buf = self.frame_to_pixel_buffer(vt, vf)?;

        let pts_time = CMTime::make(pts, 1_000_000);
        // Frame duration from the caller's cadence; nominal 1/30 s when
        // no frame rate is known (the value is a rate-control hint, not
        // part of the bitstream).
        let dur_time = match self.frame_duration_us {
            Some(us) => CMTime::make(us, 1_000_000),
            None => CMTime::make(1, 30),
        };

        let status = unsafe {
            (vt.vt_comp_encode)(
                self.session,
                pixel_buf,
                pts_time,
                dur_time,
                std::ptr::null_mut(), // frame properties
                std::ptr::null_mut(), // source frame ref con
                std::ptr::null_mut(), // info flags out
            )
        };

        // Release our reference — VT retains it internally.
        unsafe { (vt.cf_release)(pixel_buf) };

        if status != K_OS_STATUS_NO_ERROR {
            return Err(vt_error("VTCompressionSessionEncodeFrame", status));
        }

        // Force synchronous completion.
        let complete_status =
            unsafe { (vt.vt_comp_complete)(self.session, CMTime::make(i64::MAX, 1)) };
        if complete_status != K_OS_STATUS_NO_ERROR {
            return Err(vt_error(
                "VTCompressionSessionCompleteFrames",
                complete_status,
            ));
        }

        // Drain newly produced packets. Frame reordering is disabled at
        // session-create time, so decode order equals presentation order
        // and DTS mirrors PTS.
        let mut guard = self
            .state
            .lock()
            .map_err(|_| Error::other("lock poisoned"))?;
        if let Some(ref e) = guard.error {
            return Err(Error::other(e.clone()));
        }
        while let Some((data, keyframe)) = guard.packets.pop_front() {
            let mut pkt = Packet::new(0, TimeBase::new(1, 1_000_000), data)
                .with_pts(pts)
                .with_dts(pts)
                .with_keyframe(keyframe);
            if let Some(dur) = self.frame_duration_us {
                pkt = pkt.with_duration(dur);
            }
            self.output_queue.push_back(pkt);
        }

        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(pkt) = self.output_queue.pop_front() {
            return Ok(pkt);
        }
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        if self.session.is_null() {
            return Ok(());
        }
        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;
        let status = unsafe { (vt.vt_comp_complete)(self.session, CMTime::make(i64::MAX, 1)) };
        if status != K_OS_STATUS_NO_ERROR {
            return Err(vt_error(
                "VTCompressionSessionCompleteFrames (flush)",
                status,
            ));
        }
        let mut guard = self
            .state
            .lock()
            .map_err(|_| Error::other("lock poisoned"))?;
        while let Some((data, keyframe)) = guard.packets.pop_front() {
            let mut pkt = Packet::new(0, TimeBase::new(1, 1_000_000), data).with_keyframe(keyframe);
            if let Some(dur) = self.frame_duration_us {
                pkt = pkt.with_duration(dur);
            }
            self.output_queue.push_back(pkt);
        }
        Ok(())
    }
}

// ─────────────────────────── Public factory functions ─────────────────────────

pub fn make_h264_encoder(params: &CodecParameters) -> Result<Box<dyn oxideav_core::Encoder>> {
    VtEncoder::new_h264(params)
}

pub fn make_hevc_encoder(params: &CodecParameters) -> Result<Box<dyn oxideav_core::Encoder>> {
    VtEncoder::new_hevc(params)
}

#[cfg(test)]
mod tests {
    use super::{
        frame_duration_us, h264_profile_string, hevc_profile_string, parse_constant_bit_rate,
        parse_data_rate_limits, parse_keyframe_interval, parse_keyframe_interval_duration,
        resolve_expected_frame_rate, DataRateLimit,
    };
    use oxideav_core::{CodecId, CodecParameters, Rational};

    /// The NV12 plane copies handed to `CVPixelBufferCreateWithPlanarBytes`
    /// are freed when CoreVideo destroys the pixel buffer — i.e. the
    /// five-parameter planar release callback actually fires and reclaims
    /// the `PlaneBoxes` allocation. Before this test's commit the callback
    /// was bound with the wrong (two-parameter) arity and freed nothing,
    /// leaking the full NV12 frame copy on every encoded frame.
    #[test]
    fn plane_boxes_released() {
        use super::LIVE_PLANE_BOXES;
        use oxideav_core::{Frame, PixelFormat, VideoFrame, VideoPlane};
        use std::sync::atomic::Ordering;

        if crate::sys::vtable().is_err() {
            eprintln!("oxideav-videotoolbox: framework unavailable, skipping plane-box test");
            return;
        }

        let width = 64usize;
        let height = 64usize;
        let mut p = CodecParameters::video(CodecId::new("h264"));
        p.width = Some(width as u32);
        p.height = Some(height as u32);
        p.pixel_format = Some(PixelFormat::Yuv420P);

        let before = LIVE_PLANE_BOXES.load(Ordering::SeqCst);

        let mut encoder = super::make_h264_encoder(&p).expect("encoder construction");
        for i in 0..10i64 {
            let chroma = width.div_ceil(2) * height.div_ceil(2);
            let frame = VideoFrame {
                pts: Some(i),
                planes: vec![
                    VideoPlane {
                        stride: width,
                        data: vec![(16 + i * 5) as u8; width * height],
                    },
                    VideoPlane {
                        stride: width.div_ceil(2),
                        data: vec![128u8; chroma],
                    },
                    VideoPlane {
                        stride: width.div_ceil(2),
                        data: vec![128u8; chroma],
                    },
                ],
            };
            encoder.send_frame(&Frame::Video(frame)).expect("send");
            while encoder.receive_packet().is_ok() {}
        }
        encoder.flush().expect("flush");
        drop(encoder);

        // Every pixel buffer VT held has been released by session
        // teardown; the release callback must have freed every plane box.
        let after = LIVE_PLANE_BOXES.load(Ordering::SeqCst);
        assert_eq!(
            after, before,
            "NV12 plane copies leaked: {before} live before, {after} after"
        );
    }

    /// `vt_error` classifies the "not available on this host" family as
    /// `Unsupported` (so the registry's software fallback retries), the
    /// caller/data-shaped family as `InvalidData`, and everything else
    /// as `Other` — always embedding the symbolic header name.
    #[test]
    fn vt_error_classification() {
        use super::vt_error;
        use oxideav_core::Error;

        // Unsupported family.
        for status in [
            -12900, -12905, -12906, -12908, -12910, -12913, -12915, -12916, -17692, -17693,
        ] {
            match vt_error("ctx", status) {
                Error::Unsupported(msg) => {
                    assert!(msg.contains("ctx"), "missing context in {msg}");
                    assert!(msg.contains("kVT"), "missing symbolic name in {msg}");
                }
                other => panic!("status {status}: expected Unsupported, got {other:?}"),
            }
        }
        // Invalid-data family.
        for status in [-12902, -12909] {
            match vt_error("ctx", status) {
                Error::InvalidData(msg) => {
                    assert!(msg.contains("kVT"), "missing symbolic name in {msg}");
                }
                other => panic!("status {status}: expected InvalidData, got {other:?}"),
            }
        }
        // Everything else — including unknown codes — is Other, and the
        // message keeps the numeric status.
        match vt_error("VTDecompressionSessionCreate", -12911) {
            Error::Other(msg) => {
                assert!(msg.contains("kVTVideoDecoderMalfunctionErr"), "{msg}");
                assert!(msg.contains("-12911"), "{msg}");
            }
            other => panic!("expected Other, got {other:?}"),
        }
        match vt_error("ctx", -424242) {
            Error::Other(msg) => assert!(msg.contains("-424242"), "{msg}"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    /// `frame_duration_us` derives the per-frame duration (µs) from the
    /// same cadence resolution as `resolve_expected_frame_rate` and
    /// returns `None` when no usable frame rate exists.
    #[test]
    fn frame_duration_from_cadence() {
        let mut p = CodecParameters::video(CodecId::new("h264"));
        assert_eq!(frame_duration_us(&p), None);

        p.frame_rate = Some(Rational::new(30, 1));
        assert_eq!(frame_duration_us(&p), Some(33_333));

        p.frame_rate = Some(Rational::new(30000, 1001));
        assert_eq!(frame_duration_us(&p), Some(33_367)); // 1e6*1001/30000 rounded

        // Explicit option overrides the Rational.
        p.options
            .insert("expected_frame_rate".to_string(), "50".to_string());
        assert_eq!(frame_duration_us(&p), Some(20_000));

        // Degenerate cadences yield None (no duration claim).
        p.options = oxideav_core::CodecOptions::new();
        p.frame_rate = Some(Rational::new(0, 1));
        assert_eq!(frame_duration_us(&p), None);
        p.frame_rate = Some(Rational::new(30, 0));
        assert_eq!(frame_duration_us(&p), None);
    }

    /// `h264_profile_string` accepts the documented short aliases
    /// case-insensitively and maps each to the canonical
    /// `kVTProfileLevel_H264_*_AutoLevel` string.
    #[test]
    fn h264_profile_aliases() {
        assert_eq!(
            h264_profile_string("Baseline"),
            Some("H264_Baseline_AutoLevel")
        );
        assert_eq!(h264_profile_string("MAIN"), Some("H264_Main_AutoLevel"));
        assert_eq!(h264_profile_string("high"), Some("H264_High_AutoLevel"));
        assert_eq!(
            h264_profile_string("extended"),
            Some("H264_Extended_AutoLevel")
        );
        assert_eq!(h264_profile_string(""), None);
        assert_eq!(h264_profile_string("not-a-profile"), None);
    }

    /// Constrained Baseline / Constrained High aliases land on the
    /// `kVTProfileLevel_H264_Constrained{Baseline,High}_AutoLevel` symbols
    /// declared in `VTCompressionProperties.h` (macOS 12.0+).
    #[test]
    fn h264_constrained_aliases() {
        assert_eq!(
            h264_profile_string("constrained_baseline"),
            Some("H264_ConstrainedBaseline_AutoLevel")
        );
        assert_eq!(
            h264_profile_string("ConstrainedBaseline"),
            Some("H264_ConstrainedBaseline_AutoLevel")
        );
        assert_eq!(
            h264_profile_string("CONSTRAINED_HIGH"),
            Some("H264_ConstrainedHigh_AutoLevel")
        );
        assert_eq!(
            h264_profile_string("constrainedhigh"),
            Some("H264_ConstrainedHigh_AutoLevel")
        );
    }

    /// Named-level H.264 aliases land on the explicit `H264_<Profile>_<L>_<l>`
    /// SDK symbols. Coverage: every Baseline / Main / High level the SDK
    /// header declares, plus `Extended_5_0` (the only Extended-with-level
    /// constant Apple exports).
    #[test]
    fn h264_named_level_aliases() {
        // Baseline.
        assert_eq!(
            h264_profile_string("baseline_1_3"),
            Some("H264_Baseline_1_3")
        );
        assert_eq!(
            h264_profile_string("baseline_3_0"),
            Some("H264_Baseline_3_0")
        );
        assert_eq!(
            h264_profile_string("Baseline_3_1"),
            Some("H264_Baseline_3_1")
        );
        assert_eq!(
            h264_profile_string("baseline_3_2"),
            Some("H264_Baseline_3_2")
        );
        assert_eq!(
            h264_profile_string("BASELINE_4_0"),
            Some("H264_Baseline_4_0")
        );
        assert_eq!(
            h264_profile_string("baseline_4_1"),
            Some("H264_Baseline_4_1")
        );
        assert_eq!(
            h264_profile_string("baseline_4_2"),
            Some("H264_Baseline_4_2")
        );
        assert_eq!(
            h264_profile_string("baseline_5_0"),
            Some("H264_Baseline_5_0")
        );
        assert_eq!(
            h264_profile_string("baseline_5_1"),
            Some("H264_Baseline_5_1")
        );
        assert_eq!(
            h264_profile_string("baseline_5_2"),
            Some("H264_Baseline_5_2")
        );
        // Main.
        assert_eq!(h264_profile_string("main_3_0"), Some("H264_Main_3_0"));
        assert_eq!(h264_profile_string("main_3_1"), Some("H264_Main_3_1"));
        assert_eq!(h264_profile_string("main_3_2"), Some("H264_Main_3_2"));
        assert_eq!(h264_profile_string("main_4_0"), Some("H264_Main_4_0"));
        assert_eq!(h264_profile_string("main_4_1"), Some("H264_Main_4_1"));
        assert_eq!(h264_profile_string("Main_4_2"), Some("H264_Main_4_2"));
        assert_eq!(h264_profile_string("main_5_0"), Some("H264_Main_5_0"));
        assert_eq!(h264_profile_string("main_5_1"), Some("H264_Main_5_1"));
        assert_eq!(h264_profile_string("main_5_2"), Some("H264_Main_5_2"));
        // High.
        assert_eq!(h264_profile_string("high_3_0"), Some("H264_High_3_0"));
        assert_eq!(h264_profile_string("high_3_1"), Some("H264_High_3_1"));
        assert_eq!(h264_profile_string("high_3_2"), Some("H264_High_3_2"));
        assert_eq!(h264_profile_string("high_4_0"), Some("H264_High_4_0"));
        assert_eq!(h264_profile_string("HIGH_4_1"), Some("H264_High_4_1"));
        assert_eq!(h264_profile_string("high_4_2"), Some("H264_High_4_2"));
        assert_eq!(h264_profile_string("high_5_0"), Some("H264_High_5_0"));
        assert_eq!(h264_profile_string("high_5_1"), Some("H264_High_5_1"));
        assert_eq!(h264_profile_string("high_5_2"), Some("H264_High_5_2"));
        // Extended (only one Apple-declared level).
        assert_eq!(
            h264_profile_string("extended_5_0"),
            Some("H264_Extended_5_0")
        );
    }

    /// The canonical `H264_*` strings from the SDK header pass straight
    /// through. The round-9 implementation documented this behaviour but the
    /// `_ => None` arm silently swallowed it; round 12 fixes that.
    #[test]
    fn h264_canonical_passthrough() {
        // Each canonical value is preserved verbatim (case-sensitive — they
        // are SDK-declared symbol strings).
        for s in [
            "H264_Baseline_AutoLevel",
            "H264_Main_AutoLevel",
            "H264_High_AutoLevel",
            "H264_Extended_AutoLevel",
            "H264_ConstrainedBaseline_AutoLevel",
            "H264_ConstrainedHigh_AutoLevel",
            "H264_Baseline_1_3",
            "H264_Baseline_5_2",
            "H264_Main_5_2",
            "H264_High_5_2",
            "H264_Extended_5_0",
        ] {
            assert_eq!(h264_profile_string(s), Some(s));
        }
        // Junk SDK-style strings are NOT passed through — only the
        // documented set is accepted.
        assert_eq!(h264_profile_string("H264_DoesNotExist"), None);
        assert_eq!(h264_profile_string("H264_High_99_9"), None);
    }

    /// `hevc_profile_string` accepts the documented short aliases
    /// case-insensitively and maps each to the canonical
    /// `kVTProfileLevel_HEVC_*_AutoLevel` string.
    #[test]
    fn hevc_profile_aliases() {
        assert_eq!(hevc_profile_string("Main"), Some("HEVC_Main_AutoLevel"));
        assert_eq!(hevc_profile_string("MAIN10"), Some("HEVC_Main10_AutoLevel"));
        assert_eq!(
            hevc_profile_string("main_10"),
            Some("HEVC_Main10_AutoLevel")
        );
        assert_eq!(hevc_profile_string(""), None);
        assert_eq!(hevc_profile_string("bogus"), None);
    }

    /// HEVC 4:2:2 10-bit alias lands on the **actual** Apple CFString value
    /// `"HEVC_Main42210_AutoLevel"` (five contiguous digits) — not the
    /// `"HEVC_Main4_2_2_10_AutoLevel"` form the round-9 implementation
    /// emitted (which VT would have rejected, silently falling back to
    /// Main). The round-9 input alias `main4_2_2_10` keeps working; round 12
    /// just corrects the output value and adds the canonical-form aliases.
    #[test]
    fn hevc_main42210_emits_actual_sdk_value() {
        // Round-9 input aliases — preserved.
        assert_eq!(
            hevc_profile_string("main4_2_2_10"),
            Some("HEVC_Main42210_AutoLevel")
        );
        assert_eq!(
            hevc_profile_string("main422_10"),
            Some("HEVC_Main42210_AutoLevel")
        );
        // Round-12 new input aliases.
        assert_eq!(
            hevc_profile_string("main42210"),
            Some("HEVC_Main42210_AutoLevel")
        );
        assert_eq!(
            hevc_profile_string("main_42210"),
            Some("HEVC_Main42210_AutoLevel")
        );
        // Canonical pass-through.
        assert_eq!(
            hevc_profile_string("HEVC_Main42210_AutoLevel"),
            Some("HEVC_Main42210_AutoLevel")
        );
    }

    /// HEVC canonical-pass-through accepts each documented value verbatim
    /// and refuses everything else (no `HEVC_DoesNotExist`).
    #[test]
    fn hevc_canonical_passthrough() {
        for s in [
            "HEVC_Main_AutoLevel",
            "HEVC_Main10_AutoLevel",
            "HEVC_Main42210_AutoLevel",
        ] {
            assert_eq!(hevc_profile_string(s), Some(s));
        }
        assert_eq!(hevc_profile_string("HEVC_DoesNotExist"), None);
        // The bug-form string is not accepted.
        assert_eq!(hevc_profile_string("HEVC_Main4_2_2_10_AutoLevel"), None);
    }

    /// `parse_keyframe_interval` accepts non-negative integers (including 0,
    /// the SDK's "no forced cadence" sentinel) and clamps anything above
    /// `i32::MAX` to that ceiling. Whitespace surrounding the value is
    /// tolerated; negative, empty, and non-numeric input all return `None`.
    #[test]
    fn keyframe_interval_parser() {
        assert_eq!(parse_keyframe_interval("0"), Some(0));
        assert_eq!(parse_keyframe_interval("1"), Some(1));
        assert_eq!(parse_keyframe_interval("60"), Some(60));
        assert_eq!(parse_keyframe_interval(" 250 "), Some(250));
        // Clamp at i32::MAX rather than overflow.
        assert_eq!(parse_keyframe_interval("99999999999"), Some(i32::MAX));
        // Reject negatives, empty, and non-numeric strings.
        assert_eq!(parse_keyframe_interval("-1"), None);
        assert_eq!(parse_keyframe_interval(""), None);
        assert_eq!(parse_keyframe_interval("not-a-number"), None);
        assert_eq!(parse_keyframe_interval("3.14"), None);
    }

    /// `parse_keyframe_interval_duration` accepts non-negative finite
    /// Float64 values (including 0, the SDK's "no forced cadence" sentinel).
    /// Negatives, NaN, ±infinity, and unparseable input return `None`.
    #[test]
    fn keyframe_interval_duration_parser() {
        assert_eq!(parse_keyframe_interval_duration("0"), Some(0.0));
        assert_eq!(parse_keyframe_interval_duration("0.5"), Some(0.5));
        assert_eq!(parse_keyframe_interval_duration("2"), Some(2.0));
        assert_eq!(parse_keyframe_interval_duration(" 1.25 "), Some(1.25));
        // Reject negatives, NaN, infinity, empty.
        assert_eq!(parse_keyframe_interval_duration("-0.1"), None);
        assert_eq!(parse_keyframe_interval_duration("nan"), None);
        assert_eq!(parse_keyframe_interval_duration("inf"), None);
        assert_eq!(parse_keyframe_interval_duration("-inf"), None);
        assert_eq!(parse_keyframe_interval_duration(""), None);
        assert_eq!(parse_keyframe_interval_duration("not-a-number"), None);
    }

    /// `resolve_expected_frame_rate` reads `options["expected_frame_rate"]`
    /// when present and finite-positive, falling back to `params.frame_rate`
    /// (`Rational`) otherwise. Returns `None` when both sources are absent or
    /// invalid so the encoder skips setting the property entirely.
    #[test]
    fn expected_frame_rate_resolver() {
        // Neither source set → None (encoder keeps VT's default cadence hint).
        let mut p = CodecParameters::video(CodecId::new("h264"));
        assert_eq!(resolve_expected_frame_rate(&p), None);

        // params.frame_rate alone — derived from the Rational's `as_f64`.
        p.frame_rate = Some(Rational::new(30000, 1001));
        let v = resolve_expected_frame_rate(&p).expect("derived from Rational");
        assert!((v - (30000.0 / 1001.0)).abs() < 1e-9, "got {v}");

        // Zero-denominator Rational is rejected (division-by-zero would
        // produce a non-finite value).
        p.frame_rate = Some(Rational::new(30, 0));
        assert_eq!(resolve_expected_frame_rate(&p), None);

        // Negative / zero rate rejected.
        p.frame_rate = Some(Rational::new(-30, 1));
        assert_eq!(resolve_expected_frame_rate(&p), None);
        p.frame_rate = Some(Rational::new(0, 1));
        assert_eq!(resolve_expected_frame_rate(&p), None);

        // Explicit options value overrides params.frame_rate.
        p.frame_rate = Some(Rational::new(30, 1));
        p.options
            .insert("expected_frame_rate".to_string(), "59.94".to_string());
        let v = resolve_expected_frame_rate(&p).expect("override");
        assert!((v - 59.94).abs() < 1e-9, "got {v}");

        // Junk / non-finite override falls back to params.frame_rate.
        p.options
            .insert("expected_frame_rate".to_string(), "nan".to_string());
        let v = resolve_expected_frame_rate(&p).expect("fallback");
        assert!((v - 30.0).abs() < 1e-9, "got {v}");

        // Negative override falls back to params.frame_rate.
        p.options
            .insert("expected_frame_rate".to_string(), "-10".to_string());
        let v = resolve_expected_frame_rate(&p).expect("fallback");
        assert!((v - 30.0).abs() < 1e-9, "got {v}");

        // Zero override falls back to params.frame_rate.
        p.options
            .insert("expected_frame_rate".to_string(), "0".to_string());
        let v = resolve_expected_frame_rate(&p).expect("fallback");
        assert!((v - 30.0).abs() < 1e-9, "got {v}");
    }

    /// `parse_data_rate_limits` accepts 1–2 `bytes:seconds` segments
    /// (whitespace-tolerated; comma-separated). Bytes is an i32 (clamped
    /// to the SDK's CFNumber<SInt32> array element); seconds is a
    /// strictly-positive finite Float64 per the header's "duration in
    /// seconds" wording.
    #[test]
    fn data_rate_limits_parser_single_segment() {
        let segs = parse_data_rate_limits("100000:1").expect("single segment");
        assert_eq!(
            segs,
            vec![DataRateLimit {
                bytes: 100_000,
                seconds: 1.0
            }]
        );
    }

    #[test]
    fn data_rate_limits_parser_two_segments() {
        let segs = parse_data_rate_limits("100000:1, 500000:5").expect("two segments");
        assert_eq!(
            segs,
            vec![
                DataRateLimit {
                    bytes: 100_000,
                    seconds: 1.0,
                },
                DataRateLimit {
                    bytes: 500_000,
                    seconds: 5.0,
                },
            ]
        );
    }

    #[test]
    fn data_rate_limits_parser_whitespace_tolerated() {
        let segs = parse_data_rate_limits("  200000 : 2.5  ").expect("whitespace");
        assert_eq!(
            segs,
            vec![DataRateLimit {
                bytes: 200_000,
                seconds: 2.5,
            }]
        );
    }

    #[test]
    fn data_rate_limits_parser_rejects_more_than_two_segments() {
        // Apple's header documents "zero, one or two hard limits"; we
        // reject 3+ segments since VT would refuse a `CFArray` with more
        // than 4 elements anyway.
        assert_eq!(parse_data_rate_limits("1:1, 2:2, 3:3"), None);
    }

    #[test]
    fn data_rate_limits_parser_rejects_zero_or_negative_seconds() {
        assert_eq!(parse_data_rate_limits("100000:0"), None);
        assert_eq!(parse_data_rate_limits("100000:-1"), None);
        assert_eq!(parse_data_rate_limits("100000:nan"), None);
        assert_eq!(parse_data_rate_limits("100000:inf"), None);
    }

    #[test]
    fn data_rate_limits_parser_rejects_negative_or_oversize_bytes() {
        assert_eq!(parse_data_rate_limits("-1:1"), None);
        // i32::MAX + 1 — out of CFNumber<SInt32> range.
        assert_eq!(parse_data_rate_limits("2147483648:1"), None);
        // i32::MAX exactly is accepted (boundary).
        let segs = parse_data_rate_limits("2147483647:1").expect("i32::MAX boundary");
        assert_eq!(segs[0].bytes, i32::MAX);
    }

    #[test]
    fn data_rate_limits_parser_rejects_malformed_input() {
        // Missing colon.
        assert_eq!(parse_data_rate_limits("100000"), None);
        // Empty / whitespace-only.
        assert_eq!(parse_data_rate_limits(""), None);
        assert_eq!(parse_data_rate_limits("   "), None);
        // Empty segment after comma.
        assert_eq!(parse_data_rate_limits("100000:1,"), None);
        // Non-numeric.
        assert_eq!(parse_data_rate_limits("abc:1"), None);
        assert_eq!(parse_data_rate_limits("100000:abc"), None);
    }

    /// `parse_constant_bit_rate` accepts non-negative integers
    /// (CFNumber bits-per-second); clamps overflow at `i32::MAX`; rejects
    /// negatives / floats / empty / non-numeric.
    #[test]
    fn constant_bit_rate_parser() {
        assert_eq!(parse_constant_bit_rate("0"), Some(0));
        assert_eq!(parse_constant_bit_rate("2000000"), Some(2_000_000));
        assert_eq!(parse_constant_bit_rate(" 5000000 "), Some(5_000_000));
        // Clamp at i32::MAX.
        assert_eq!(parse_constant_bit_rate("99999999999"), Some(i32::MAX));
        // Reject.
        assert_eq!(parse_constant_bit_rate("-1"), None);
        assert_eq!(parse_constant_bit_rate(""), None);
        assert_eq!(parse_constant_bit_rate("2.5"), None);
        assert_eq!(parse_constant_bit_rate("not-a-number"), None);
    }
}
