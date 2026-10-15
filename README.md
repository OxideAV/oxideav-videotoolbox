# oxideav-videotoolbox

[![CI](https://github.com/OxideAV/oxideav-videotoolbox/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-videotoolbox/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-videotoolbox.svg)](https://crates.io/crates/oxideav-videotoolbox) [![docs.rs](https://docs.rs/oxideav-videotoolbox/badge.svg)](https://docs.rs/oxideav-videotoolbox) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Apple-platform (macOS + iOS) VideoToolbox hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

Apple's [VideoToolbox](https://developer.apple.com/documentation/videotoolbox) exposes the dedicated media engine on Apple Silicon (and the equivalent IP on Intel Macs). For codecs the chip supports natively this is **5-50× faster** than software decoding and orders of magnitude more energy-efficient.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on VideoToolbox, no Objective-C / Swift. The framework is opened via [`libloading`] on first use.

## Fallback behaviour

Two distinct failure paths fall back automatically to the pure-Rust codec:

1. **Load failure** — older OS, missing framework, sandboxed environment without VT entitlements. `register()` logs and returns without registering, so the SW codec is the only candidate at dispatch. On macOS this surfaces if `Library::new("/System/Library/Frameworks/...")` cannot map the framework; on iOS the system dyld has link-loaded the four frameworks at process start, so the only path that fires this branch is a `Library::this()` failure (effectively impossible for a running process — Apple's runtime always provides the frameworks on every supported iOS version).
2. **Init failure** — `VTDecompressionSessionCreate` / `VTCompressionSessionCreate` returns a non-zero `OSStatus` for the requested parameters. Common triggers: stream above the device's max resolution, hardware encoder slot already busy (concurrent-session cap), unsupported pixel format, codec profile the device doesn't accelerate. The factory returns `Err`; `oxideav_pipeline::make_decoder_with` / `make_encoder_with` (the selection layer the pipeline and CLI use) retries the next-priority impl (typically the SW one). On iOS this also catches the platform-specific gaps Apple documents — e.g. AV1 encode requires A17+, ProRes encode requires iPhone 13 Pro+, some `kVTCompressionPropertyKey_*` keys are macOS-only and return `kVTPropertyNotSupportedErr` (treated as non-fatal by the property-write helper).

## Error taxonomy

Every non-zero `OSStatus` is rendered with its symbolic header name —
e.g. `VTDecompressionSessionCreate: OSStatus -12906
(kVTCouldNotFindVideoDecoderErr)` — via `sys::describe_os_status`, which
covers the error enums of `VideoToolbox/VTErrors.h`, CoreMedia's
`CMBlockBuffer.h` / `CMFormatDescription.h` / `CMSampleBuffer.h`, and
CoreVideo's `CVReturn.h`. Statuses are classified onto the typed core
error surface:

* `Error::Unsupported` — the "not available on this host" family
  (`kVTCouldNotFindVideo{De,En}coderErr`,
  `kVTVideo{De,En}coderNotAvailableNowErr`,
  `kVTVideoDecoderUnsupportedDataFormatErr`,
  `kVTPropertyNotSupportedErr`, `kVTPixelTransferNotSupportedErr`,
  needs-Rosetta, …) — the registry retries the pure-Rust impl.
* `Error::InvalidData` — `kVTParameterErr`, `kVTVideoDecoderBadDataErr`.
* `Error::Other` — malfunctions, allocation failures, unknown codes.

Errors reported by VT's output callbacks surface once — at the end of
the `send_packet` / `send_frame` that caused them, at `flush`, or from
`receive_frame` — and are then cleared: one corrupt access unit does
not latch the session into a permanent error state, and a single still
VideoToolbox cannot decode is an error rather than zero frames.

## Timestamps & packet metadata

* **Decoded frames carry their PTS.** Each submitted access unit rides
  a sequence stamp through the by-value `CMTime` of the
  decompression-output callback (seven-parameter prototype in
  `VTDecompressionSession.h`); the bridge matches the decoded picture
  back to its packet, so `VideoFrame::pts` is the caller's own
  timestamp (`None` for packets that carried none) and frames come out
  in **display order** (see "Output order" below).
* **Encoder packets carry stream metadata**: `flags.keyframe` (from the
  access unit's VCL NAL types for H.264/HEVC — IDR / IDR_W_RADL /
  IDR_N_LP / CRA — and unconditionally for the intra-only MJPEG /
  ProRes), `dts` mirroring `pts` (frame reordering is disabled at
  session-create time), and `duration` derived from
  `options["expected_frame_rate"]` / `params.frame_rate`.

## Hardware policy knob

`options["hardware"]` builds the session-specification dictionary for
**all four encoders and all nine decoders**:

| Value | Specification | Behaviour |
|-------|---------------|-----------|
| `require` / `required` | `kVT*Specification_RequireHardwareAccelerated*` = true | Hardware only; session creation fails when the media engine is absent / the format isn't hardware-supported / the hardware slot is busy — no silent software fallback. The header documents `kVTCouldNotFindVideo{De,En}coderErr` (→ typed `Error::Unsupported`); virtualized hosts have been observed returning `kVTInvalidSessionErr` (→ `Error::Other`). |
| `enable` / `allow` | `kVT*Specification_EnableHardwareAccelerated*` = true | Prefer hardware, allow VT fallback (VT's documented default, stated explicitly). |
| `disable` / `software` / `sw` | the Enable key = false | Force VT's internal software codec (distinct from the registry's pure-Rust fallback). |

Absent / unrecognised values keep VT's default policy (no specification
dictionary).

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

Users who want to force the pure-Rust path globally can pass `--no-hwaccel` to the `oxideav` CLI; this sets `CodecPreferences { no_hardware: true }`, which the pipeline forwards to `oxideav_pipeline::make_decoder_with` / `make_encoder_with` so HW factories are skipped at dispatch time. The runtime context still registers VT — `oxideav list` shows the `*_videotoolbox` rows regardless of the flag — only resolution is biased.

## Coverage roadmap

| Codec        | Decode (M-series) | Encode (M-series) | Status                  |
|--------------|-------------------|-------------------|-------------------------|
| H.264        | hardware          | hardware          | wired (≈ 51 dB PSNR_Y)  |
| HEVC         | hardware          | hardware          | wired (≈ 54 dB PSNR_Y)  |
| ProRes       | hardware          | hardware          | wired (≈ 52 dB PSNR_Y)  |
| JPEG (MJPEG) | hardware (4:2:0 + grey scans; others fall back) | hardware | wired (58–73 dB vs the pure-Rust decode) |
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
| Hardware policy | `params.options["hardware"]` (`require` / `enable` / `disable`) | every encoder **and** decoder (session-specification, not a property) | `kVT{VideoEncoder,VideoDecoder}Specification_{Require,Enable}HardwareAccelerated*` (CFBoolean) |

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

### Output labels

A decoder's frames — and `Decoder::output_pixel_format()` — are
labelled with the layout it **actually produces**. The label is
negotiated once: the container's declared `CodecParameters::pixel_format`
when VideoToolbox can emit it, otherwise the codec's own layout (H.264 /
HEVC: the SPS's chroma format × bit depth; ProRes 422 flavours:
`Yuv422P10Le`; ProRes 4444 flavours: `Yuv444P16Le`; MPEG-2 / MPEG-4 /
VP9 / AV1 / VVC: `Yuv420P`; JPEG: read from the SOF). VideoToolbox is
asked for exactly that layout through `destinationImageBufferAttributes`
(the `pixbuf` module maps `PixelFormat` ⇄ `kCVPixelFormatType_*`: 8-bit
4:2:0 / 4:2:2 / 4:4:4 in both ranges, grey, 10-bit 4:2:0 / 4:2:2 /
4:4:4, 16-bit 4:2:2 / 4:4:4, BGRA; 4:1:1 and 4:4:0 are derived from a
4:4:4 request by a software chroma re-sampling) and the buffer is
converted into planar frames of that label. A declared layout
VideoToolbox has no pixel format for (12-bit planar, CMYK, RGB, alpha
planes, …) is refused at construction with a typed `Unsupported`, so the
registry falls back to the pure-Rust decoder instead of getting
mislabelled frames.

**JPEG** is the measured exception: the VideoToolbox JPEG decompressor
(hardware and its software path alike) decodes every source at 4:2:0 —
a 4:2:2 / 4:4:4 output request only adds a pixel-transfer upsample
(chroma ≈ 32 dB against the real samples, luma broken at odd sizes),
4:4:0 / 4:1:1 scans fail outright and CMYK is rejected. The decoder
therefore claims 4:2:0 and single-component scans only (`YuvJ420P` /
`Gray8`, 58–73 dB against the pure-Rust decode at every size down to
1×1) and returns `Unsupported` for everything else, which the pure-Rust
decoder decodes exactly.

### Output order

VideoToolbox returns pictures in decode order on the M-series media
engine (`kVTDecodeFrame_EnableTemporalProcessing` and the asynchronous
flag only defer the callbacks). The H.264 / HEVC decoders derive every
picture's order count from its slice header (`nalparse`: H.264 §8.2.1
for all three `pic_order_cnt_type`s, H.265 §8.3.1) and hold pictures in
a buffer sorted by order count, releasing them when the stream's reorder
depth is exceeded (H.264 VUI `max_num_reorder_frames`, else the level's
`MaxDpbFrames`; H.265 `sps_max_num_reorder_pics`), when the container's
decode timeline (`dts < pts` seen) proves them due, at a new coded video
sequence, and in full at `flush`. Known limit: a
`memory_management_control_operation` equal to 5 is not detected (the
order-count history resets at IDR pictures only). The intra-only and
container-framed codecs (JPEG, ProRes, MPEG-2 / MPEG-4 Part 2 as framed
here, VP9, AV1) are delivered as decoded.

### Decoder framing details

H.264 / HEVC accept both framings: the `avcC` / `hvcC` configuration
record (or Annex-B parameter sets) in `CodecParameters::extradata`
builds the session at construction, in-band parameter sets re-create it
when they change, and each packet is read as length-prefixed (the
record's `lengthSizeMinusOne + 1`, 4 without a record) or Annex B —
a length prefix that spells a start code (`00 00 01 xx`) is resolved by
the record's length size. Packets holding several coded pictures are
split into access units on the H.264 §7.4.1.2.3 / H.265 §7.4.2.4.4
boundaries.

Codecs without parameter sets (MJPEG `'jpeg'`, ProRes) share one
`blob.rs` `BlobDecoder` / `BlobEncoder` driver built on
`CMVideoFormatDescriptionCreate(width, height, codecType)`. ProRes
flavour is selected from `CodecParameters::tag` (one of `apco` / `apcs`
/ `apcn` / `apch` / `ap4h` / `ap4x`) on both the encode and the decode
side, defaulting to ProRes 422.

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
