# oxideav-videotoolbox

macOS VideoToolbox hardware decode/encode bridge for the [oxideav](https://github.com/OxideAV/oxideav) framework.

## Why a bridge crate?

Apple's [VideoToolbox](https://developer.apple.com/documentation/videotoolbox) exposes the dedicated media engine on Apple Silicon (and the equivalent IP on Intel Macs). For codecs the chip supports natively this is **5-50× faster** than software decoding and orders of magnitude more energy-efficient.

This crate is a **thin runtime-loaded bridge** — no compile-time link dependency on VideoToolbox, no Objective-C / Swift. The framework is opened via [`libloading`] on first use; if the load fails, registered factories return `Error::Unsupported` and the framework registry falls back to the pure-Rust codec.

## Platform gating

The whole crate is `#![cfg(target_os = "macos")]`. On Linux / Windows it compiles to an empty rlib; the umbrella `oxideav` crate gates the `register` call behind the same cfg.

## Priority

Hardware factories register with `CodecCapabilities::with_priority(0)` — **lower numbers win at resolution time**, so on macOS hardware paths are preferred over the pure-Rust impls (which sit at priority 100+).

## Opt-out

Users who want to force the pure-Rust path can disable hardware acceleration globally via the `oxideav` CLI's `--no-hwaccel` flag (Round 2 work — see issue tracker). The flag works by skipping `oxideav_videotoolbox::register` (and `oxideav_audiotoolbox::register`) when constructing the runtime context.

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

Round 1 (this commit): scaffolding only. Round 2: H.264 / HEVC decode + encode. Round 3: ProRes + JPEG. Round 4: VP9 / AV1 / MPEG-2.

## Workspace policy

Calling a system OS framework via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule (no embedding source from libvpx, libwebp, libjxl, etc.) does not apply to this crate.

## License

MIT.
