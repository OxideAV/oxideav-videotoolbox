# oxideav-videotoolbox

[![CI](https://github.com/OxideAV/oxideav-videotoolbox/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-videotoolbox/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-videotoolbox.svg)](https://crates.io/crates/oxideav-videotoolbox) [![docs.rs](https://docs.rs/oxideav-videotoolbox/badge.svg)](https://docs.rs/oxideav-videotoolbox) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Apple-platform (macOS + iOS) VideoToolbox hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

Apple's [VideoToolbox](https://developer.apple.com/documentation/videotoolbox) exposes the dedicated media engine on Apple Silicon (and the equivalent IP on Intel Macs). For codecs the chip supports natively this is **5-50× faster** than software decoding and orders of magnitude more energy-efficient.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on VideoToolbox, no Objective-C / Swift. The framework is opened via [`libloading`] on first use.

## Fallback behaviour

Two distinct failure paths fall back automatically to the pure-Rust codec:

1. **Load failure** — older OS, missing framework, sandboxed environment without VT entitlements. `register()` logs and returns without registering, so the SW codec is the only candidate at dispatch. On macOS this surfaces if `Library::new("/System/Library/Frameworks/...")` cannot map the framework; on iOS the system dyld has link-loaded the four frameworks at process start, so the only path that fires this branch is a `Library::this()` failure (effectively impossible for a running process — Apple's runtime always provides the frameworks on every supported iOS version).
2. **Init failure** — `VTDecompressionSessionCreate` / `VTCompressionSessionCreate` returns a non-zero `OSStatus` for the requested parameters. Common triggers: stream above the device's max resolution, hardware encoder slot already busy (concurrent-session cap), unsupported pixel format, codec profile the device doesn't accelerate. The factory returns `Err`; the registry's `make_decoder_with` / `make_encoder_with` retries the next-priority impl (typically the SW one). On iOS this also catches the platform-specific gaps Apple documents — e.g. AV1 encode requires A17+, ProRes encode requires iPhone 13 Pro+, some `kVTCompressionPropertyKey_*` keys are macOS-only and return `kVTPropertyNotSupportedErr` (treated as non-fatal by the property-write helper).

Pipelines that **require** hardware (e.g. real-time low-latency capture where the SW path can't keep up) can opt out of the SW fallback by setting `CodecPreferences { require_hardware: true, .. }` — the registry will then surface the `OSStatus` error instead of degrading silently.

## Platform gating

The whole crate is `#![cfg(any(target_os = "macos", target_os = "ios"))]`. On Linux / Windows it compiles to an empty rlib; the umbrella `oxideav` crate gates the `register` call behind the same cfg.

Symbol-loading shape differs by platform:

* **macOS** — `libloading::Library::new("/System/Library/Frameworks/<Name>.framework/<Name>")` opens each of VideoToolbox / CoreVideo / CoreMedia / CoreFoundation via `dlopen` at first use. **No compile-time link dependency.**
* **iOS** — the four Apple system frameworks are link-loaded at process start by the system dyld via `build.rs` (`cargo:rustc-link-lib=framework=VideoToolbox` etc.). At runtime `libloading::os::unix::Library::this()` returns the host process's `RTLD_DEFAULT` handle and every framework symbol resolves via `dlsym` against the dyld shared cache.

The vtable assembly code and every call site are identical across the two branches — the only difference is the `sys::open()` helper.

## iOS targets

* `aarch64-apple-ios` — iOS device builds (iPhone, iPad)
* `aarch64-apple-ios-sim` — Apple Silicon iOS simulator
* `x86_64-apple-ios-sim` — Intel-Mac iOS simulator (legacy host)

The crate's `cargo check` is exercised against all three in CI. Live `cargo test` on iOS would need an iOS simulator runner; the current integration suite drives a black-box validator binary, which isn't shipped in the iOS sim — live iOS testing is a follow-up. macOS tests cover the shared crate body, which is symmetric on the two platforms above the symbol-loading layer.

## Priority

Hardware factories register with `CodecCapabilities::with_priority(10)` — **lower numbers win at resolution time**, so on macOS hardware paths are preferred over the pure-Rust impls (which sit at priority 100+).

## Opt-out

Users who want to force the pure-Rust path globally can pass `--no-hwaccel` to the `oxideav` CLI; this sets `CodecPreferences { no_hardware: true }`, which the pipeline forwards to `make_decoder_with` / `make_encoder_with` so HW factories are skipped at dispatch time. The runtime context still registers VT — `oxideav list` shows the `*_videotoolbox` rows regardless of the flag — only resolution is biased.

## Coverage roadmap

| Codec        | Decode (M-series) | Encode (M-series) | Status                  |
|--------------|-------------------|-------------------|-------------------------|
| H.264        | hardware          | hardware          | wired (≈ 51 dB PSNR_Y)  |
| HEVC         | hardware          | hardware          | wired (≈ 54 dB PSNR_Y)  |
| ProRes       | hardware          | hardware          | wired (≈ 52 dB PSNR_Y)  |
| JPEG (MJPEG) | hardware          | hardware          | wired (≈ 36 dB PSNR_Y)  |
| MPEG-2       | hardware          | — (no VT encoder) | wired (≈ 61 dB PSNR_Y, decode-only) |
| VP9          | hardware (M1+)    | — (no VT encoder) | wired (decode-only)     |
| MPEG-4 Pt 2  | hardware          | — (no VT encoder) | wired (decode-only, VOL→ESDS extension atoms, ≈ 72 dB PSNR_Y) |
| AV1          | hardware (M3+) / VT-internal SW elsewhere | hardware (M3+) | decode wired (av1C extension-atom); encode roadmap |
| VVC (H.266)  | hardware (M3+, macOS 26+) / VT-internal SW elsewhere | — (no VT VVC compression session yet) | decode wired (Annex-B splitter + vvcC extension-atom) |

## Encoder knobs

Encoders accept four optional knobs threaded through `CodecParameters`:

| Knob | Source | Applies to | VT property |
|------|--------|------------|-------------|
| Target bit-rate (bps) | `params.bit_rate: Option<u64>` | H.264, HEVC, MJPEG | `kVTCompressionPropertyKey_AverageBitRate` (CFNumber-i32, saturating-clamped to `i32::MAX`) |
| Quality (0.0..1.0) | `params.options["quality"]` | MJPEG (primary), H.264 / HEVC / ProRes (hint) | `kVTCompressionPropertyKey_Quality` (CFNumber-Float32) |
| Profile / level | `params.options["profile"]` | H.264 (auto-level: `baseline` / `main` / `high` / `extended` / `constrained_baseline` / `constrained_high`; explicit level: `baseline_{1_3,3_0..5_2}` / `main_{3_0..5_2}` / `high_{3_0..5_2}` / `extended_5_0`; canonical `H264_*` pass-through); HEVC (`main` / `main10` / `main4_2_2_10` / `main42210`; canonical `HEVC_*` pass-through) | `kVTCompressionPropertyKey_ProfileLevel` (CFString) |
| ProRes flavour | `params.tag = Some(CodecTag::fourcc(b"..."))` | ProRes encoder | `kCMVideoCodecType_AppleProRes*` (one of `apco` / `apcs` / `apcn` / `apch` / `ap4h` / `ap4x`) |
| Max keyframe interval (frames) | `params.options["keyframe_interval"]` (non-negative integer; 0 = no forced cadence) | H.264, HEVC (intra-only MJPEG / ProRes silently ignore) | `kVTCompressionPropertyKey_MaxKeyFrameInterval` (CFNumber-i32, clamped to `i32::MAX`) |
| Max keyframe interval (seconds) | `params.options["keyframe_interval_duration"]` (finite non-negative; 0 = no forced cadence) | H.264, HEVC (intra-only MJPEG / ProRes silently ignore) | `kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration` (CFNumber-Float64) |
| Expected frame rate (fps) | `params.options["expected_frame_rate"]` (finite-positive Float64); fall-back: `params.frame_rate` reduced via `Rational::as_f64()` | H.264, HEVC, MJPEG (ProRes ignores; fixed-CBR) | `kVTCompressionPropertyKey_ExpectedFrameRate` (CFNumber-Float64) |
| Data-rate hard cap | `params.options["data_rate_limits"]` (`bytes:seconds[,bytes:seconds]`, 1–2 segments) | H.264, HEVC, MJPEG (ProRes ignores; fixed-CBR) | `kVTCompressionPropertyKey_DataRateLimits` (CFArray\<CFNumber\>, alternating `[bytes, seconds, ...]`) |
| Constant bit-rate (bps, macOS 13+) | `params.options["constant_bit_rate"]` (non-negative integer; clamped to `i32::MAX`) | H.264, HEVC, MJPEG (ProRes ignores; fixed-CBR). Mutually exclusive with `bit_rate` (`AverageBitRate`) + `data_rate_limits` per SDK header. | `kVTCompressionPropertyKey_ConstantBitRate` (CFNumber bits/second) |

All knobs are optional; absent / out-of-range values keep the previous defaults (H.264 Baseline_AutoLevel, HEVC Main_AutoLevel, ProRes 422, no explicit bit-rate or quality, VT-default keyframe cadence, no data rate limits, default rate-control mode). VT silently ignores properties it doesn't support for a given codec, so `bit_rate` on ProRes is a no-op (ProRes is fixed-CBR per profile), `MaxKeyFrameInterval*` on MJPEG / ProRes is a no-op (both are intra-only), and `constant_bit_rate` on pre-macOS-13 hosts or encoders without CBR support returns `kVTPropertyNotSupportedErr` (treated as non-fatal) — round-trip still succeeds in every case.

The three rate-control knobs (`AverageBitRate` soft long-term target,
`DataRateLimits` hard short-window cap composable up to two segments,
and the legacy fixed-CBR `ConstantBitRate` mode), the cadence knobs
(`MaxKeyFrameInterval` / `MaxKeyFrameIntervalDuration` /
`ExpectedFrameRate`), the quality knob, and the full H.264 / HEVC
profile-level alias map are wired through both encoder paths (`encoder.rs`
H.264 / HEVC and `blob.rs` MJPEG / ProRes), with shared option-parser
helpers so both paths have identical input semantics. Property keys are
those declared in the macOS SDK header
`VideoToolbox/VTCompressionProperties.h`.

### Decoder framing details

Codecs without parameter sets (MJPEG `'jpeg'`, ProRes `'apcn'`) share
one `blob.rs` `BlobDecoder` / `BlobEncoder` driver built on
`CMVideoFormatDescriptionCreate(width, height, codecType)`. A
pixel-format-adaptive decode callback inspects
`CVPixelBufferGetPixelFormatType` and dispatches to one of four
converters (NV12 `'420v'`/`'420f'`, packed UYVY `'2vuy'`, packed YUY2
`'yuvs'`, biplanar 16-bit 4:2:2 `'sv22'`), so ProRes's 16-bit 4:2:2
output is handled alongside the H.264 / HEVC NV12 path. ProRes flavour
is selected from `CodecParameters::tag` (one of `apco` / `apcs` /
`apcn` / `apch` / `ap4h` / `ap4x`), defaulting to ProRes 422.

Codecs whose elementary stream is not pre-framed get an in-codec
`FrameSplit` framer that carves access units from the bitstream:

* `FrameSplit::Mpeg2Es` — splits MPEG-2 on the picture start code,
  attaching leading sequence / GOP / extension headers to the first
  picture.
* `FrameSplit::Mpeg4PartTwoEs` — splits MPEG-4 Part 2 on VOP start
  codes (`00 00 01 B6`), attaching preceding VOS / VO / VOL / GOV /
  user-data headers to the first VOP.
* `FrameSplit::VvcEs` — splits VVC (H.266) per H.266 Annex B, opening
  a new access unit on an AUD / PH repeat or a VCL NAL.

Container-framed codecs (VP9, AV1) use `FrameSplit::Whole` — one
demuxed `Packet` is one access unit.

For decoders whose VT path requires the configuration record
out-of-band, the bridge harvests the leading non-VCL configuration
prefix from the first packet and supplies it through
`kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms`: an
ISO/IEC 14496-1 ESDS blob keyed `"esds"` for MPEG-4 Part 2, an
`av1C` record for AV1, and a `vvcC` record for VVC. The public `blob`
module exposes the NAL / OBU walkers and config-record builders these
paths use.

## Workspace policy

Calling a system OS framework via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule (no embedding third-party codec source) does not apply to bridging a system OS framework.

## License

MIT.
