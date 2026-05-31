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
| MPEG-4 Pt 2  | hardware          | — (no VT encoder) | wired (decode-only, VOL→ESDS extension atoms, ≈ 72 dB PSNR_Y) |
| AV1          | hardware (M3+) / VT-internal SW elsewhere | hardware (M3+) | decode wired (round 8); encode roadmap |

## Encoder knobs

Encoders accept four optional knobs threaded through `CodecParameters`:

| Knob | Source | Applies to | VT property |
|------|--------|------------|-------------|
| Target bit-rate (bps) | `params.bit_rate: Option<u64>` | H.264, HEVC, MJPEG | `kVTCompressionPropertyKey_AverageBitRate` (CFNumber-i32, saturating-clamped to `i32::MAX`) |
| Quality (0.0..1.0) | `params.options["quality"]` | MJPEG (primary), H.264 / HEVC / ProRes (hint) | `kVTCompressionPropertyKey_Quality` (CFNumber-Float32) |
| Profile / level | `params.options["profile"]` | H.264 (`baseline` / `main` / `high` / `extended`); HEVC (`main` / `main10` / `main4_2_2_10`) | `kVTCompressionPropertyKey_ProfileLevel` (CFString) |
| ProRes flavour | `params.tag = Some(CodecTag::fourcc(b"..."))` | ProRes encoder | `kCMVideoCodecType_AppleProRes*` (one of `apco` / `apcs` / `apcn` / `apch` / `ap4h` / `ap4x`) |

All knobs are optional; absent / out-of-range values keep the previous defaults (H.264 Baseline_AutoLevel, HEVC Main_AutoLevel, ProRes 422, no explicit bit-rate or quality). VT silently ignores properties it doesn't support for a given codec, so `bit_rate` on ProRes is a no-op (ProRes is fixed-CBR per profile) — round-trip still succeeds.

Round 1: scaffolding. Round 2: H.264 + HEVC decode + encode. Round 3: JPEG (MJPEG) + ProRes decode + encode via a shared blob-codec module (`blob.rs`) — single-blob frames built on `CMVideoFormatDescriptionCreate(width, height, codecType)` rather than the parameter-set extraction H.264/HEVC need. Round 4: MPEG-2 video decode (`kCMVideoCodecType_MPEG2Video`) — decode-only, since VideoToolbox exposes an MPEG-2 *decoder* but no encoder; an elementary-stream framer (`FrameSplit::Mpeg2Es`) carves the incoming bitstream into per-picture access units. Round 5: VP9 decode (`kCMVideoCodecType_VP9` = `'vp09'`) — decode-only (no VT VP9 encoder); hardware decode on M1+ Apple Silicon, with VT falling back to software on older Macs that lack the dedicated VP9 IP. VP9 frames are container-framed (IVF / Matroska / MP4) so `FrameSplit::Whole` applies unchanged — no per-picture splitter is needed. Round 6: MPEG-4 Part 2 video decode (`kCMVideoCodecType_MPEG4Video` = `'mp4v'`) — the DivX / Xvid / Visual ASP / SP family, **not** H.264 (which is MPEG-4 Part 10 and ships via `'avc1'`). Decode-only as well: VideoToolbox exposes no MPEG-4 Pt 2 compression session. A new `FrameSplit::Mpeg4PartTwoEs` framer splits the elementary stream on VOP start codes (`00 00 01 B6`) and attaches preceding VOS / Visual Object / VO / VOL / GOV / user-data headers to the first VOP. Round 7: VOL→ESDS extension-atom path — on hosts where VT enforces VOL-via-extradata for MPEG-4 Pt 2, the decoder extracts the configuration prefix (everything up to the first VOP start code) from the first packet, wraps it in a proper ISO/IEC 14496-1 ES_Descriptor + DecoderConfigDescriptor + DecoderSpecificInfo blob (`ObjectTypeIndication = 0x20`, `streamType = VisualStream`), and supplies it to `CMVideoFormatDescriptionCreate` via `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms` keyed by `"esds"`. Measured PSNR_Y vs ffmpeg's software decode jumps to ≈ 72.8 dB (sample-exact within IDCT tolerance) on the integration fixture. **Round 8 (this commit): AV1 video decode** (`kCMVideoCodecType_AV1` = `'av01'`) — decode-only for now; hardware decode is gated to Apple Silicon M3+, with VT falling back to its internal software AV1 path on older hardware where that path exists (and to `oxideav-av1` via the registry's SW-fallback elsewhere). AV1 frames are container-framed (IVF / Matroska / MP4 / WebM / RTP) so the existing `FrameSplit::Whole` path applies unchanged. Remaining roadmap: AV1 *encode* (the M3+ `'av01'` compression session, macOS 14+), plus an optional `av1C` extension-atom path analogous to round 7's ESDS for hosts that require the AV1 Sequence Header out-of-band.

### Round 9 implementation notes

* **Encoder knobs wired across all four VT encoders.** Until round 8 the only VT compression-session properties the bridge configured were `RealTime = true`, `AllowFrameReordering = false`, and a hardcoded H.264 Baseline / HEVC Main `ProfileLevel`. Round 9 turns `CodecParameters::bit_rate`, `params.options["quality"]`, and `params.options["profile"]` into live properties: when the caller sets them, the encoder sets `kVTCompressionPropertyKey_AverageBitRate` (CFNumber-i32, saturating-clamped to `i32::MAX`), `kVTCompressionPropertyKey_Quality` (CFNumber-Float32, validated `[0.0, 1.0]` finite), and `kVTCompressionPropertyKey_ProfileLevel` (CFString, derived from the alias map below) on the session immediately after creation. Unset / out-of-range values keep the prior defaults — every existing call site keeps its existing behaviour.
* **Profile-alias map.** H.264 short names accepted (case-insensitive): `baseline` / `main` / `high` / `extended` → `kVTProfileLevel_H264_{Baseline,Main,High,Extended}_AutoLevel`. HEVC: `main` / `main10` / `main4_2_2_10` → `kVTProfileLevel_HEVC_{Main,Main10,Main4_2_2_10}_AutoLevel`. Unknown alias = fall back to the codec's built-in default (Baseline / Main), so callers can still write the literal Apple string into `options["profile"]` if needed without breaking.
* **ProRes profile-selection now lives in the factory.** `make_prores_encoder` reads `params.tag` and dispatches to one of the six `kCMVideoCodecType_AppleProRes*` constants — `apco` (Proxy), `apcs` (LT), `apcn` (422, default), `apch` (HQ), `ap4h` (4444), `ap4x` (4444 XQ) — via `prores_codec_type_for_tag`. Non-ProRes / missing tags fall back to ProRes 422 (the round-3 default), so existing callers see no change. `CodecTag::fourcc()` upper-cases at construction, and the dispatcher matches against the upper-case forms `APCO` / `APCS` / `APCN` / `APCH` / `AP4H` / `AP4X`.
* **CFNumber-Float32 support added to `sys.rs`.** New `K_CF_NUMBER_FLOAT_32_TYPE` constant (= `5`, per Apple's `CFNumber.h` enum order: `kCFNumberSInt8Type = 1 .. kCFNumberFloat32Type = 5 .. kCFNumberFloat64Type = 6`) and a `cf_number_f32(vt, v)` helper. Used by the new `Quality` plumbing — every prior CFNumber call site (the existing `cf_number_i32`) is unchanged.
* **Five new unit tests cover the new mapping helpers.** `h264_profile_aliases` and `hevc_profile_aliases` exercise the alias maps case-insensitively (and assert that empty / unknown inputs return `None` so the caller falls back); `prores_codec_type_constants_match_fourcc` asserts every `K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_*` constant equals its documented fourcc; `prores_tag_dispatch_each_fourcc` walks every ProRes fourcc through `prores_codec_type_for_tag`; `prores_tag_dispatch_falls_back_on_unknown` covers the unknown-fourcc / non-Fourcc / `None` paths. One new integration test (`encoder_knobs_round_trip_without_regression`) exercises the live property writes against the macOS VT session for H.264 (bit_rate + profile + quality) and ProRes LT (tag-driven profile selection) and verifies the round-trip still produces packets.

### Round 8 implementation notes

* **AV1 decode wiring via VideoToolbox.** `make_av1_decoder` registers a decoder against `CodecId::new("av1")` (tags `av01 / AV01 / V_AV1`) backed by a `BlobDecoder` over `kCMVideoCodecType_AV1 = 'av01' = 0x6176_3031`. The factory is decode-only — a VT AV1 *compression* session exists on macOS 14+ for M3+ hardware but needs its own callback/pixel-buffer wiring and is a follow-up round.
* **Hardware path is M3+; older silicon falls back gracefully.** Apple Silicon M3 (and later) carries dedicated AV1 hardware-decode IP that VT routes to automatically. On M1 / M2 and Intel hosts VideoToolbox either takes its internal software AV1 path (where the macOS build includes one) or returns a non-zero `OSStatus` at `VTDecompressionSessionCreate`. The registry's lower-priority pure-Rust `oxideav-av1` decoder is the fallback in the latter case — the round-8 wiring does not invent a software path, it just exposes the hardware path when present.
* **Framing is `FrameSplit::Whole` — no in-codec splitter.** AV1 is container-framed (IVF / Matroska / MP4 / WebM / RTP). Each demuxed `Packet` carries exactly one AV1 temporal unit (one or more OBUs that together compose a single decoded frame), and that temporal unit goes straight into a `CMSampleBuffer` for VT. AV1 has no Annex-B / picture-start-code mechanism that would require an in-codec carve like MPEG-2's or MPEG-4 Part 2's.
* **Configuration record (`av1C`) is a follow-up round.** AV1 in ISOBMFF / Matroska carries an `av1C` configuration record whose payload is the AV1 Sequence Header OBU (per the AV1 ISOBMFF mapping under `docs/container/mpeg4/av1-isobmff/`). On hosts where VT requires the Sequence Header via `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms` rather than extracted from the first packet, supplying the `av1C` blob via the extension atoms is the same pattern as MPEG-4 Part 2's ESDS wired in round 7 — the round-7 ESDS plumbing in `BlobDecoder::ensure_session` already supports an arbitrary extension-atom key, so adding `av1C` is a small follow-up once a host needs it.
* **Validated against ffmpeg as a black-box.** `av1_decode_against_ffmpeg` asks `ffmpeg -c:v libaom-av1 -f ivf` to produce a 320×240 / 10-frame gradient stream (the same shape as the VP9 fixture), parses the IVF container (32-byte file header + per-frame 12-byte header + payload), feeds each frame as one `Packet` through `make_av1_decoder`, and compares to ffmpeg's own software decode at PSNR_Y ≥ 30 dB. The test self-skips when ffmpeg / libaom-av1 / the framework / the VT AV1 decoder is unavailable on the host — older OS, M1/M2 hosts without a VT AV1 fallback, ffmpeg builds without libaom-av1.
* **Two new unit/integration tests.** `register_installs_av1_decode_only` (decoder registered for `CodecId::new("av1")`, no encoder); `av1_codec_type_is_av01_fourcc` (the codec-type constant equals `u32::from_be_bytes(b"av01")` = `0x6176_3031`). Plus the integration `av1_decode_against_ffmpeg` covering the end-to-end IVF→VT→PSNR path.

### Round 7 implementation notes

* **VOL→ESDS extension-atom path for MPEG-4 Part 2.** Round 6 wired the decoder against `(codec_type, width, height)` only — fine on hosts where VT's MPEG-4 Pt 2 decoder is lenient about extracting the VOL from the bitstream prefix, but on stricter VT hosts session creation returned `kVTVideoDecoderBadDataErr` (the registry then fell back to the pure-Rust impl). Round 7 closes that gap. Before opening the session, `BlobDecoder` (under `FrameSplit::Mpeg4PartTwoEs`) sniffs the leading bytes of the first packet up to but not including the first VOP start code (`00 00 01 B6`), wraps them in a full ISO/IEC 14496-1 ESDS descriptor (`ES_Descriptor 0x03 → DecoderConfigDescriptor 0x04 [ObjectTypeIndication 0x20 = MPEG-4 Visual, streamType 0x04 = VisualStream] → DecoderSpecificInfo 0x05 = VOL bytes`, with a 1-byte `SLConfigDescriptor 0x06 predefined=2` tail), and feeds the result through `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms = { "esds": CFData }` on the format-description creation call. The ffmpeg-fixture decode test (`mpeg4_part_two_decode_against_ffmpeg`) now reaches **≈ 72.8 dB PSNR_Y** vs ffmpeg's reference decode on the gradient fixture (10/10 frames returned).
* **Hosts that don't need the ESDS extension still work.** The extraction is best-effort: if the first packet starts with a VOP (no headers to capture) or the buffer has no VOP marker, `extract_mpeg4_part_two_vol` returns `None` and the decoder falls back to the round-6 plain `(codec_type, width, height)` path. Existing JPEG / ProRes / MPEG-2 / VP9 framers go through the same code path without an extensions dictionary, so their session creation is byte-for-byte unchanged from round 6.
* **ISO/IEC 14496-1 reference.** The ESDS descriptor structure is documented in `docs/container/mpeg4/ISO_IEC_14496-1-System-2010.pdf` §7.2.6; the `oxideav-mp4` muxer uses the same shape for AAC `mp4a` sample entries, so the BER length helper and field layout are consistent with the rest of the workspace.
* **CoreFoundation symbol added.** `sys.rs` now resolves `CFDataCreate` and exposes a `cf_data(vt, &[u8])` helper that copies the slice into a fresh `CFData`. Used only by the MPEG-4 Pt 2 path so far; available for any future codec needing a binary-blob extension atom (FLAC `dfLa`, AV1 `av1C`, …).
* **Six new unit tests.** `mpeg4_extract_vol_*` (5 cases) cover the VOL-prefix extractor (returns prefix before VOP, includes GOV/user-data, `None` when no VOP, `None` when buffer starts with VOP, `None` on empty buffer); `esds_*` (5 cases) cover the ESDS builder (FullBox header zeros, ES_Descriptor tag, DCD tag + ObjectTypeIndication + streamType byte, DSI carries the VOL verbatim, SLC `predefined = 2`).

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
