# oxideav-videotoolbox

macOS VideoToolbox hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

Apple's [VideoToolbox](https://developer.apple.com/documentation/videotoolbox) exposes the dedicated media engine on Apple Silicon (and the equivalent IP on Intel Macs). For codecs the chip supports natively this is **5-50× faster** than software decoding and orders of magnitude more energy-efficient.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on VideoToolbox, no Objective-C / Swift. The framework is opened via [`libloading`] on first use.

## Fallback behaviour

Two distinct failure paths fall back automatically to the pure-Rust codec:

1. **Load failure** — older macOS, missing framework, sandboxed environment without VT entitlements. `register()` logs and returns without registering, so the SW codec is the only candidate at dispatch.
2. **Init failure** — `VTDecompressionSessionCreate` / `VTCompressionSessionCreate` returns a non-zero `OSStatus` for the requested parameters. Common triggers: stream above the device's max resolution, hardware encoder slot already busy (concurrent-session cap), unsupported pixel format, codec profile the device doesn't accelerate. The factory returns `Err`; the registry's `make_decoder_with` / `make_encoder_with` retries the next-priority impl (typically the SW one).

Pipelines that **require** hardware (e.g. real-time low-latency capture where the SW path can't keep up) can opt out of the SW fallback by setting `CodecPreferences { require_hardware: true, .. }` — the registry will then surface the `OSStatus` error instead of degrading silently.

## Platform gating

The whole crate is `#![cfg(target_os = "macos")]`. On Linux / Windows it compiles to an empty rlib; the umbrella `oxideav` crate gates the `register` call behind the same cfg.

## Priority

Hardware factories register with `CodecCapabilities::with_priority(10)` — **lower numbers win at resolution time**, so on macOS hardware paths are preferred over the pure-Rust impls (which sit at priority 100+).

## Opt-out

Users who want to force the pure-Rust path globally can pass `--no-hwaccel` to the `oxideav` CLI; this sets `CodecPreferences { no_hardware: true }`, which the pipeline forwards to `make_decoder_with` / `make_encoder_with` so HW factories are skipped at dispatch time. The runtime context still registers VT — `oxideav list` shows the `*_videotoolbox` rows regardless of the flag — only resolution is biased.

## Coverage roadmap

| Codec        | Decode (M-series) | Encode (M-series) |
|--------------|-------------------|-------------------|
| H.264        | hardware          | hardware          |
| HEVC         | hardware          | hardware          |
| ProRes       | hardware          | hardware          |
| MPEG-2       | hardware          | —                 |
| MPEG-4 Pt 2  | hardware          | —                 |
| VP9          | hardware (M1+)    | —                 |
| AV1          | hardware (M3+)    | hardware (M3+)    |
| JPEG         | hardware          | hardware          |

Round 1: scaffolding. Round 2 (this commit): H.264 + HEVC decode + encode — both wired, roundtrip PSNR_Y H.264 ≈ 46 dB, HEVC ≈ 50 dB. Round 3: ProRes + JPEG. Round 4: VP9 / AV1 / MPEG-2.

## Workspace policy

Calling a system OS framework via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule (no embedding source from libvpx, libwebp, libjxl, etc.) does not apply to this crate.

## License

MIT.
