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

| Codec        | Decode (M-series) | Encode (M-series) | Status               |
|--------------|-------------------|-------------------|----------------------|
| H.264        | hardware          | hardware          | wired (≈ 51 dB PSNR_Y) |
| HEVC         | hardware          | hardware          | wired (≈ 54 dB PSNR_Y) |
| ProRes       | hardware          | hardware          | wired (≈ 52 dB PSNR_Y) |
| JPEG (MJPEG) | hardware          | hardware          | wired (≈ 36 dB PSNR_Y) |
| MPEG-2       | hardware          | —                 | roadmap              |
| MPEG-4 Pt 2  | hardware          | —                 | roadmap              |
| VP9          | hardware (M1+)    | —                 | roadmap              |
| AV1          | hardware (M3+)    | hardware (M3+)    | roadmap              |

Round 1: scaffolding. Round 2: H.264 + HEVC decode + encode. **Round 3 (this commit): JPEG (MJPEG) + ProRes decode + encode via a shared blob-codec module (`blob.rs`)** — single-blob frames built on `CMVideoFormatDescriptionCreate(width, height, codecType)` rather than the parameter-set extraction H.264/HEVC need. Round 4: VP9 / AV1 / MPEG-2 / MPEG-4 Pt 2.

### Round 3 implementation notes

* **Blob codec module (`src/blob.rs`)** — `BlobDecoder` and `BlobEncoder` share one VTDecompression/VTCompression driver for every codec whose format description is `(width, height, codecType)` with no parameter sets. Currently used by MJPEG (`'jpeg'`) and ProRes (`'apcn'`).
* **Pixel-format adaptive callback** — VT decoders return different `CVPixelBuffer` formats depending on the codec: H.264/HEVC honour the NV12 destination-attribute request (`'420v'`), but ProRes returns 16-bit biplanar 4:2:2 (`'sv22'`) regardless. The blob decoder callback inspects `CVPixelBufferGetPixelFormatType` and dispatches to one of four converters: NV12 (`'420v'`/`'420f'`), packed UYVY (`'2vuy'`), packed YUY2 (`'yuvs'`), or biplanar 16-bit 4:2:2 (`'sv22'`).
* **ProRes profile selection** — defaults to ProRes 422 (`'apcn'`) for both encode and decode. The decoder format description carries the codec-type, and VT internally dispatches to the right ProRes flavour when it sees the frame header (`'icpf'` magic at offset 4). Explicit profile selection via `CodecParameters::tag` is a future-round item.
* **Roundtrip tests use a smooth diagonal gradient** — the previous test pattern `(col + row/2 + frame*10) % 255` had a modulo-wraparound discontinuity that JPEG's DCT could not represent without ~10 dB of error. The new gradient (clipped to video-range `[16, 235]`) reaches ≥ 36 dB on every codec.

## Workspace policy

Calling a system OS framework via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule (no embedding source from libvpx, libwebp, libjxl, etc.) does not apply to this crate.

## License

MIT.
