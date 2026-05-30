# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.3](https://github.com/OxideAV/oxideav-videotoolbox/compare/v0.0.2...v0.0.3) - 2026-05-30

### Other

- round 7: MPEG-4 Part 2 VOL→ESDS extension-atom path
- round 6: MPEG-4 Part 2 video decode via VideoToolbox (decode-only)
- round 5: VP9 video decode via VideoToolbox (decode-only)
- unit-cover the MPEG-2 elementary-stream access-unit splitter
- round 4: MPEG-2 video decode via VideoToolbox (decode-only)
- handle ProRes 'v216' format on macos-latest CI runner
- round 3: MJPEG + ProRes decode + encode via VideoToolbox
- add .gitignore + drop committed Cargo.lock

### Added

- **Round 7: MPEG-4 Part 2 VOL→ESDS extension-atom path.** On VT hosts that
  enforce VOL-via-extradata (rather than letting the decoder extract the VOL
  from the bitstream prefix), `BlobDecoder` now sniffs the configuration
  prefix from the first packet's leading bytes (everything up to but not
  including the first VOP start code `00 00 01 B6`), wraps it in a full
  ISO/IEC 14496-1 ESDS descriptor, and supplies it to
  `CMVideoFormatDescriptionCreate` via
  `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms = { "esds":
  CFData }`. ESDS structure (per ISO/IEC 14496-1 §7.2.6 + ISO/IEC 14496-14
  §5.6):
  - `ES_Descriptor` (tag `0x03`) — `ES_ID = 0`, flags = `0`.
  - `DecoderConfigDescriptor` (tag `0x04`) — `ObjectTypeIndication = 0x20`
    (MPEG-4 Visual / Part 2), `streamType << 2 | upStream | reserved =
    (0x04 << 2) | 0 | 1 = 0x11` (VisualStream).
  - `DecoderSpecificInfo` (tag `0x05`) — VOL bytes verbatim.
  - `SLConfigDescriptor` (tag `0x06`) — 1-byte `predefined = 0x02` (mp4 file).
  The `mpeg4_part_two_decode_against_ffmpeg` integration test now reaches
  ≈ 72.8 dB PSNR_Y vs ffmpeg's software decode on the gradient fixture
  (10/10 frames returned). Other framers (Whole / Mpeg2Es) skip the
  extension dictionary entirely so their session-creation calls are
  byte-for-byte unchanged.
  - Public API additions on `oxideav_videotoolbox::blob`:
    `extract_mpeg4_part_two_vol(&[u8]) -> Option<&[u8]>` and
    `build_mpeg4_part_two_esds(vol: &[u8]) -> Vec<u8>`. Available for the
    auditor / tests / future codecs that need a documented ESDS shape.
  - New `cf_data(vt, &[u8])` helper in `sys` (resolves `CFDataCreate` via
    the cached vtable).
- Added `CFDataRef` opaque type and `FnCFDataCreate` to `sys.rs`.
- Ten new unit tests:
  - `mpeg4_extract_vol_returns_prefix_before_vop`
  - `mpeg4_extract_vol_includes_gov_user_data`
  - `mpeg4_extract_vol_none_when_no_vop`
  - `mpeg4_extract_vol_none_when_starts_with_vop`
  - `mpeg4_extract_vol_empty_buffer`
  - `esds_has_full_box_header`
  - `esds_es_descriptor_tag_0x03`
  - `esds_decoder_config_descriptor_tag_and_oti`
  - `esds_decoder_specific_info_carries_vol`
  - `esds_sl_config_descriptor_predefined_2`
- **Round 6: MPEG-4 Part 2 video decode** via `VTDecompressionSession`
  (`kCMVideoCodecType_MPEG4Video` = `'mp4v'`). Decode-only — VideoToolbox
  exposes an MPEG-4 Part 2 decoder (historically used for DivX / Xvid
  playback on macOS) but no MPEG-4 Pt 2 compression session, so
  `make_mpeg4_part_two_decoder` registers a decoder against
  `CodecId::new("mpeg4")` and there is no encoder factory. **This is MPEG-4
  Part 2 (Visual ASP / SP), not H.264 (MPEG-4 Part 10).**
  - New `FrameSplit::Mpeg4PartTwoEs` framer on `BlobDecoder`. Splits an
    incoming MPEG-4 Pt 2 elementary stream on the VOP (Video Object Plane)
    start code (`00 00 01 B6`), attaching any leading VOS (`B0`) / Visual
    Object (`B5`) / VO (`00..1F`) / VOL (`20..2F`) / GOV (`B3`) / user-data
    (`B2`) bytes to the first VOP so the VOL travels with it. Existing
    MPEG-2 / VP9 / JPEG / ProRes framers unchanged.
  - Codec tags: `mp4v / MP4V / M4S2 / m4s2 / DIVX / divx / DX50 / XVID /
    xvid / FMP4 / fmp4` (fourcc) and `V_MPEG4/ISO/ASP` (Matroska). Registers
    with `priority = 10`, `hardware_accelerated = true`.
  - Decode validated against `ffmpeg -c:v mpeg4 -f m4v` as a black-box
    validator. The test feeds the ES through `make_mpeg4_part_two_decoder`
    and (when the session creates) compares to ffmpeg's own software decode
    at PSNR_Y ≥ 30 dB. The test self-skips when ffmpeg or the framework is
    unavailable, **and** when VT returns `kVTVideoDecoderBadDataErr` at
    session-create time (some VT hosts require the VOL to be supplied via
    format-description extension atoms rather than extracted from the
    bitstream; the registry's SW fallback handles those hosts).
  - Five new unit tests cover the MPEG-4 Part 2 access-unit splitter:
    single VOP with headers, two VOPs with first inheriting headers,
    no-VOP-found pass-through, empty buffer, and a regression test that
    confirms non-VOP start codes (B0, B3) don't trigger spurious splits.
  - `register` now installs an MPEG-4 Part 2 decoder (and asserts via test
    that it installs *no* MPEG-4 Part 2 encoder).
- Added `kCMVideoCodecType_MPEG4Video` constant.
- **Round 5: VP9 video decode** via `VTDecompressionSession`
  (`kCMVideoCodecType_VP9` = `'vp09'`). Decode-only — VideoToolbox exposes a
  VP9 decoder (hardware on M1+ Apple Silicon, with VT-internal software
  fallback elsewhere) but no VP9 compression session, so `make_vp9_decoder`
  registers a decoder against `CodecId::new("vp9")` and there is no encoder
  factory.
  - VP9 reuses the existing blob `FrameSplit::Whole` path: frames are
    container-framed (IVF / Matroska / MP4) and arrive as one self-contained
    payload per `Packet`. No in-codec splitter is needed (VP9 has no
    Annex-B / picture-start-code mechanism).
  - Codec tags: `vp09 / VP90` (fourcc) and `V_VP9` (Matroska). Registers
    with `priority = 10`, `hardware_accelerated = true`.
  - Decode validated against `ffmpeg -c:v libvpx-vp9 -f ivf` as a black-box
    validator. The test parses the IVF container (32-byte file header +
    per-frame 12-byte `(frame_size, pts)` header + payload) to recover
    individual VP9 frames, feeds each through the VT decoder, and compares
    to ffmpeg's own software decode (PSNR_Y ≥ 30 dB). The test self-skips
    when ffmpeg, libvpx-vp9, the framework, or the VT VP9 decoder is
    unavailable on the host.
  - Three new unit tests cover the IVF parser (multi-frame parse, signature
    rejection, truncated-payload rejection).
  - `register` now installs a VP9 decoder (and asserts via test that it
    installs *no* VP9 encoder).
- Added `kCMVideoCodecType_VP9` constant.
- **Round 4: MPEG-2 video decode** via `VTDecompressionSession`
  (`kCMVideoCodecType_MPEG2Video` = `'mp2v'`). Decode-only — VideoToolbox
  exposes an MPEG-2 decoder but no MPEG-2 encoder, so `make_mpeg2_decoder`
  registers a decoder against `CodecId::new("mpeg2video")` and there is
  deliberately no encoder factory.
  - New `FrameSplit` mode on `BlobDecoder`. `FrameSplit::Mpeg2Es` carves an
    incoming MPEG-2 elementary stream into per-picture access units (split on
    the picture start code `00 00 01 00`, attaching leading sequence/GOP/
    extension headers to the first picture) before handing each to VT. JPEG
    and ProRes keep `FrameSplit::Whole` (one `Packet` == one frame).
  - Codec tags: `mp2v / MPG2 / mpg2 / hdv2 / m2v1` (fourcc) and `V_MPEG2`
    (Matroska). Registers with `priority = 10`, `hardware_accelerated = true`.
  - Decode validated against `ffmpeg` as a black-box validator: an
    ffmpeg-produced MPEG-2 elementary stream decoded through VideoToolbox
    matches ffmpeg's own software decode at PSNR_Y ≈ 61 dB (320×240, 10
    frames). Test self-skips when ffmpeg or the framework is unavailable.
  - `register` now installs an MPEG-2 decoder (and asserts via test that it
    installs *no* MPEG-2 encoder).
- Added `kCMVideoCodecType_MPEG2Video` constant.
- **Round 3: JPEG (MJPEG) + ProRes decode + encode** via `VTDecompressionSession` / `VTCompressionSession`.
  - New `blob.rs` module factors out a `BlobDecoder` / `BlobEncoder` pair for codecs whose format description is built from `CMVideoFormatDescriptionCreate(codec_type, width, height)` with no out-of-band parameter sets. Used by JPEG (`'jpeg'`) and ProRes (`'apcn'`).
  - `make_jpeg_decoder` / `make_jpeg_encoder` register against `CodecId::new("mjpeg")` with the JPEG fourcc tags (`jpeg / JPEG / MJPG / mjpg`).
  - `make_prores_decoder` / `make_prores_encoder` register against `CodecId::new("prores")` with all six ProRes fourccs (`apco / apcs / apcn / apch / ap4h / ap4x`). Defaults to ProRes 422 (`'apcn'`); profile selection from `CodecParameters::tag` is a future-round item.
  - Both codec ids register with `priority = 10`, `hardware_accelerated = true`, `intra_only = true`.
  - Pixel-format adaptive decode callback: VT honours the NV12 destination-attribute request for H.264/HEVC but returns 16-bit biplanar 4:2:2 (`'sv22'`) for ProRes regardless. The blob callback inspects `CVPixelBufferGetPixelFormatType` and dispatches to one of NV12 (`'420v'`/`'420f'`), packed UYVY (`'2vuy'`), packed YUY2 (`'yuvs'`), or biplanar 16-bit 4:2:2 (`'sv22'`).
  - End-to-end roundtrip tests: 320×240 synthetic gradient, 10 frames. Measured PSNR_Y: MJPEG ≈ 36 dB, ProRes ≈ 52 dB (both well above the 35 dB threshold).
- Added `CMVideoFormatDescriptionCreate`, `CVPixelBufferGetPixelFormatType`, `CVPixelBufferGetPlaneCount`, `CVPixelBufferIsPlanar`, `CVPixelBufferGetBaseAddress`, `CVPixelBufferGetBytesPerRow` to the vtable.

### Changed

- Roundtrip test fixture switched from `(col + row/2 + frame*10) % 255` (which had a modulo-wraparound discontinuity that JPEG's DCT could not represent without ~10 dB of error) to a smooth diagonal gradient clipped to video-range `[16, 235]`.

## [0.0.2](https://github.com/OxideAV/oxideav-videotoolbox/compare/v0.0.1...v0.0.2) - 2026-05-06

### Fixed

- apply cargo fmt (rustfmt CI check was failing)

### Other

- use `usize::div_ceil` for chroma subsampling
- drop dead `linkme` dep
- clarify load-vs-init fallback + document require_hardware opt-out
- round 2: real H.264 + HEVC decode + encode via VideoToolbox
- auto-register via oxideav_core::register! macro (linkme distributed slice)

### Added

- **Round 2: real H.264 + HEVC decode + encode** via
  `VTDecompressionSession` / `VTCompressionSession`.
  - `H264VtDecoder` + `HevcVtDecoder` implement `oxideav_core::Decoder`.
    Parse Annex-B input, extract SPS/PPS (VPS/SPS/PPS for HEVC),
    build `CMVideoFormatDescription`, decode via VideoToolbox,
    convert NV12 output to planar I420 `VideoFrame`.
  - `H264VtEncoder` + `HevcVtEncoder` implement `oxideav_core::Encoder`.
    Convert I420 `VideoFrame` to biplanar NV12 `CVPixelBuffer`, encode
    via VideoToolbox, convert AVCC output to Annex-B (SPS/PPS prepended
    on keyframes).
  - Both codec ids register with `priority = 10`, `hardware_accelerated =
    true`, `decode = true`, `encode = true`.
  - Graceful degradation: if the framework fails to load at runtime,
    `register` logs and returns without installing any factories.
  - End-to-end roundtrip tests: 320×240 synthetic I420 ramp, 10 frames.
    Measured PSNR_Y: H.264 ≈ 46 dB, HEVC ≈ 50 dB (both well above
    the 35 dB threshold).
- Expanded `sys.rs` vtable with all VT/CM/CV/CF symbols needed:
  `VTDecompressionSessionCreate/DecodeFrame/FinishDelayedFrames/Invalidate`,
  `VTCompressionSessionCreate/EncodeFrame/CompleteFrames/Invalidate/PrepareToEncodeFrames`,
  `CMSampleBufferCreateReady`, `CMBlockBufferCreateWithMemoryBlock`,
  `CMVideoFormatDescriptionGetH264ParameterSetAtIndex`,
  `CMVideoFormatDescriptionGetHEVCParameterSetAtIndex`,
  `CMSampleBufferGetFormatDescription`,
  `CVPixelBufferCreateWithPlanarBytes`, and all CV plane accessors.

### Changed

- `register()` body replaced: was `// Round 1: no factories yet.`,
  now installs four `CodecInfo` entries (h264 decoder, h264 encoder,
  hevc decoder, hevc encoder).

### From round 1

- Initial scaffolding: `#![cfg(target_os = "macos")]` crate that
  dlopens VideoToolbox + CoreVideo + CoreMedia + CoreFoundation
  via `libloading` on first use.
- Unified `register(&mut RuntimeContext)` entry point.
- Standalone-friendly `registry` feature.
- README coverage roadmap and priority explanation.
