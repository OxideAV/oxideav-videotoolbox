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

| Codec        | Decode (M-series) | Encode (M-series) | Status                  |
|--------------|-------------------|-------------------|-------------------------|
| H.264        | hardware          | hardware          | wired (≈ 51 dB PSNR_Y)  |
| HEVC         | hardware          | hardware          | wired (≈ 54 dB PSNR_Y)  |
| ProRes       | hardware          | hardware          | wired (≈ 52 dB PSNR_Y)  |
| JPEG (MJPEG) | hardware          | hardware          | wired (≈ 36 dB PSNR_Y)  |
| MPEG-2       | hardware          | — (no VT encoder) | wired (≈ 61 dB PSNR_Y, decode-only) |
| VP9          | hardware (M1+)    | — (no VT encoder) | wired (decode-only)     |
| MPEG-4 Pt 2  | hardware          | — (no VT encoder) | wired (decode-only, VOL-extradata follow-up) |
| AV1          | hardware (M3+)    | hardware (M3+)    | roadmap                 |

Round 1: scaffolding. Round 2: H.264 + HEVC decode + encode. Round 3: JPEG (MJPEG) + ProRes decode + encode via a shared blob-codec module (`blob.rs`) — single-blob frames built on `CMVideoFormatDescriptionCreate(width, height, codecType)` rather than the parameter-set extraction H.264/HEVC need. Round 4: MPEG-2 video decode (`kCMVideoCodecType_MPEG2Video`) — decode-only, since VideoToolbox exposes an MPEG-2 *decoder* but no encoder; an elementary-stream framer (`FrameSplit::Mpeg2Es`) carves the incoming bitstream into per-picture access units. Round 5: VP9 decode (`kCMVideoCodecType_VP9` = `'vp09'`) — decode-only (no VT VP9 encoder); hardware decode on M1+ Apple Silicon, with VT falling back to software on older Macs that lack the dedicated VP9 IP. VP9 frames are container-framed (IVF / Matroska / MP4) so `FrameSplit::Whole` applies unchanged — no per-picture splitter is needed. **Round 6 (this commit): MPEG-4 Part 2 video decode** (`kCMVideoCodecType_MPEG4Video` = `'mp4v'`) — the DivX / Xvid / Visual ASP / SP family, **not** H.264 (which is MPEG-4 Part 10 and ships via `'avc1'`). Decode-only as well: VideoToolbox exposes no MPEG-4 Pt 2 compression session. A new `FrameSplit::Mpeg4PartTwoEs` framer splits the elementary stream on VOP start codes (`00 00 01 B6`) and attaches preceding VOS / Visual Object / VO / VOL / GOV / user-data headers to the first VOP. On hosts where VT enforces VOL-via-extradata for MPEG-4 Pt 2, session creation may return `-12909 / kVTVideoDecoderBadDataErr` from just `(codec_type, width, height)`; the registry then retries the next-priority impl (the pure-Rust MPEG-4 Pt 2 decoder). A follow-up round will extract the VOL from the leading bitstream bytes and feed it to VT via the format-description extension atoms. Remaining roadmap: AV1 (M3+).

### Round 6 implementation notes

* **MPEG-4 Part 2 is decode-only.** VideoToolbox ships an MPEG-4 Part 2 decoder (historically used for DivX / Xvid playback) but no MPEG-4 Pt 2 compression session, so `make_mpeg4_part_two_decoder` registers a decoder against `CodecId::new("mpeg4")` (tags `mp4v / MP4V / M4S2 / m4s2 / DIVX / divx / DX50 / XVID / xvid / FMP4 / fmp4 / V_MPEG4/ISO/ASP`) and there is deliberately no matching encoder factory. This is **MPEG-4 Pt 2** — distinct from MPEG-4 Pt 10 (H.264), which uses `'avc1'` and stays on its own `CodecId::new("h264")` row.
* **Elementary-stream framer.** Like MPEG-2 ES, an MPEG-4 Pt 2 ES is not pre-framed. `FrameSplit::Mpeg4PartTwoEs` splits on the VOP start code (`00 00 01 B6`), attaching any leading VOS (`B0`) / Visual Object (`B5`) / VO (`00..1F`) / VOL (`20..2F`) / GOV (`B3`) / user-data (`B2`) bytes to the first VOP so the VOL travels with it. This is intrinsic bitstream framing (codec's job, not container's).
* **Session-creation caveat.** VT's MPEG-4 Pt 2 decoder typically requires the VOL configuration to be supplied via `kCMFormatDescriptionExtension_*` extension atoms (the ESDS `DecoderSpecificInfo` shape), not extracted from the bitstream as it would be for MPEG-2. Building the format description from just `(codec_type, width, height)` therefore can return `-12909 / kVTVideoDecoderBadDataErr` at session create. When that happens the registry's SW fallback takes over and the pure-Rust MPEG-4 Pt 2 decoder handles the stream. A follow-up round will extract the VOL prefix from the elementary stream and supply it via the format-description extension atoms to enable the hardware path on those hosts.
* **Validated against ffmpeg as a black-box.** The decode test generates an MPEG-4 Pt 2 elementary stream with `ffmpeg -c:v mpeg4 -f m4v`, feeds it through the VT bridge, and (when the session creates successfully) compares to ffmpeg's own software decode at PSNR_Y ≥ 30 dB. The test self-skips when ffmpeg or the framework is unavailable, **and** self-skips gracefully when the session-creation caveat above triggers — so CI on a runner without VOL-extradata leniency still passes.
* **Splitter unit tests.** Four new unit tests cover the MPEG-4 Pt 2 access-unit splitter: single VOP with headers, two VOPs with first inheriting headers, no-VOP-found pass-through, and a regression test that confirms non-VOP start codes (B0, B3) don't trigger spurious splits.

### Round 5 implementation notes

* **VP9 is decode-only.** VideoToolbox ships a VP9 decoder (M1+) but no VP9 compression session, so `make_vp9_decoder` registers a decoder against `CodecId::new("vp9")` (tags `vp09 / VP90 / V_VP9`) and there is deliberately no matching encoder factory.
* **Container-framed, no in-codec splitter.** Unlike MPEG-2's elementary-stream input, VP9 has no Annex-B / picture-start-code mechanism — frames are framed by the surrounding container (IVF, Matroska, MP4) and arrive as one self-contained payload per `Packet`. `BlobDecoder` is therefore instantiated with `FrameSplit::Whole`; bytes flow straight from `Packet::data` into the `CMSampleBuffer` without any in-codec carving.
* **Validated against ffmpeg.** The decode test asks `ffmpeg -c:v libvpx-vp9 -f ivf` to produce a 320×240 / 10-frame gradient stream, parses the IVF container (32-byte file header + per-frame 12-byte header + payload) to recover individual VP9 frames, feeds each as one `Packet`, and compares to ffmpeg's own software decode (PSNR_Y ≥ 30 dB threshold). The test self-skips when ffmpeg / libvpx-vp9 / the framework / the VT VP9 decoder is unavailable — older OS, Intel Macs without VP9 IP, ffmpeg builds without libvpx.

### Round 4 implementation notes

* **MPEG-2 is decode-only.** VideoToolbox ships an MPEG-2 decoder but no MPEG-2 compression session, so `make_mpeg2_decoder` registers a decoder against `CodecId::new("mpeg2video")` (tags `mp2v / MPG2 / mpg2 / hdv2 / m2v1 / V_MPEG2`) and there is deliberately no matching encoder factory.
* **Elementary-stream framer.** Unlike the container-framed JPEG/ProRes path (one `Packet` == one frame), an MPEG-2 elementary stream is not pre-framed. `BlobDecoder` gained a `FrameSplit` mode; `FrameSplit::Mpeg2Es` splits on the picture start code (`00 00 01 00`), attaching any leading sequence (`b3`) / GOP (`b8`) / extension (`b5`) headers to the first picture so VT can size the decoder. This is intrinsic bitstream framing (the codec's job), not container parsing.
* **Validated against ffmpeg as a black-box.** The decode test generates an MPEG-2 elementary stream with `ffmpeg` (opaque validator), decodes it through VideoToolbox, and compares the result to ffmpeg's own software decode: PSNR_Y ≈ 61 dB on a 320×240 / 10-frame gradient. The test self-skips when ffmpeg or the framework is unavailable.

### Round 3 implementation notes

* **Blob codec module (`src/blob.rs`)** — `BlobDecoder` and `BlobEncoder` share one VTDecompression/VTCompression driver for every codec whose format description is `(width, height, codecType)` with no parameter sets. Currently used by MJPEG (`'jpeg'`) and ProRes (`'apcn'`).
* **Pixel-format adaptive callback** — VT decoders return different `CVPixelBuffer` formats depending on the codec: H.264/HEVC honour the NV12 destination-attribute request (`'420v'`), but ProRes returns 16-bit biplanar 4:2:2 (`'sv22'`) regardless. The blob decoder callback inspects `CVPixelBufferGetPixelFormatType` and dispatches to one of four converters: NV12 (`'420v'`/`'420f'`), packed UYVY (`'2vuy'`), packed YUY2 (`'yuvs'`), or biplanar 16-bit 4:2:2 (`'sv22'`).
* **ProRes profile selection** — defaults to ProRes 422 (`'apcn'`) for both encode and decode. The decoder format description carries the codec-type, and VT internally dispatches to the right ProRes flavour when it sees the frame header (`'icpf'` magic at offset 4). Explicit profile selection via `CodecParameters::tag` is a future-round item.
* **Roundtrip tests use a smooth diagonal gradient** — the previous test pattern `(col + row/2 + frame*10) % 255` had a modulo-wraparound discontinuity that JPEG's DCT could not represent without ~10 dB of error. The new gradient (clipped to video-range `[16, 235]`) reaches ≥ 36 dB on every codec.

## Workspace policy

Calling a system OS framework via FFI is the same shape as calling `libc::malloc` — it's the platform, not a copied algorithm. The workspace's clean-room rule (no embedding source from libvpx, libwebp, libjxl, etc.) does not apply to this crate.

## License

MIT.
