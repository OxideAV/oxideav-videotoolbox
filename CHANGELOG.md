# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
