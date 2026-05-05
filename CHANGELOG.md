# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
