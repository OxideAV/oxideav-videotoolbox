# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- **Decompression-output callback ABI + decoded-frame PTS recovery.**
  The `VTDecompressionOutputCallback` prototype in
  `VideoToolbox/VTDecompressionSession.h` takes **seven** parameters —
  the binding omitted the two trailing by-value `CMTime`s
  (`presentationTimeStamp`, `presentationDuration`). That happened to
  work on the AArch64 / x86-64 C calling conventions (a callee may
  ignore trailing arguments), but the decoded frame's presentation time
  was unrecoverable, so **every** decoded `VideoFrame` came back with
  `pts: None` across all nine codecs. The callback type and both
  callback implementations (`decoder.rs` H.264/HEVC and `blob.rs`
  MJPEG / ProRes / MPEG-2 / VP9 / MPEG-4 Pt 2 / AV1 / VVC) now match the
  header exactly and propagate the returned time into `VideoFrame::pts`
  (when the CMTime is valid; the value round-trips the caller's own
  `packet.pts` number, or the sequential decode-order counter for
  packets that carried none). New hardware integration tests
  `h264_pts_survival` / `mjpeg_pts_survival` drive distinct
  non-contiguous PTS values through the encode → decode pipeline and
  assert each decoded frame carries one of the submitted timestamps in
  ascending presentation order.

- **`CMTime` validity semantics.** Per CoreMedia's `CMTime.h`,
  `kCMTimeFlags_Valid` (bit 0) "must be set, or the CMTime is considered
  invalid" — the exported `kCMTimeInvalid` constant is the all-zero
  struct. The bridge previously fabricated an "unknown DTS" as
  `CMTime::make(i64::MIN, 1)`, which has the Valid flag **set** and is
  therefore a *valid* decode timestamp of `i64::MIN` as far as the
  framework is concerned. All three submission sites (`decoder.rs`
  H.264/HEVC, `blob.rs` blob/framer path, and the
  `CMSampleTimingInfo::zero()` template) now pass a true
  `CMTime::invalid()` (all-zero) DTS. New `sys` API:
  `CMTime::invalid()`, `CMTime::is_valid()`, and the
  `K_CM_TIME_FLAGS_VALID` constant, with unit tests pinning the flag
  semantics.

### Added

- **iOS target support.** The crate `#![cfg(...)]` widens from
  `target_os = "macos"` to `any(target_os = "macos", target_os = "ios")`
  so the entire VT decode/encode surface — `VTDecompressionSession*` /
  `VTCompressionSession*` / `CMVideoFormatDescription*` / `CVPixelBuffer*`
  / `CF*` — is now exposed on iOS (`aarch64-apple-ios`,
  `aarch64-apple-ios-sim`, `x86_64-apple-ios-sim`) as well as macOS.
  - A new `build.rs` emits `cargo:rustc-link-lib=framework=VideoToolbox`
    (plus `CoreVideo`, `CoreMedia`, `CoreFoundation`) on
    `target_os = "ios"` only. macOS keeps zero compile-time link
    dependency; the build script is a no-op on every other target.
  - `sys::open()` gains a `#[cfg(target_os = "ios")]` branch that uses
    `libloading::os::unix::Library::this()` (equivalent to
    `dlopen(NULL, ...)`) instead of `Library::new(absolute_path)`. iOS
    sandboxed apps cannot reliably `dlopen("/System/Library/Frameworks/...")`,
    but the four frameworks have already been link-loaded by the system
    dyld at process start, so symbol resolution via `RTLD_DEFAULT`
    against the dyld shared cache works for every VT / CV / CM / CF
    symbol the vtable needs.
  - The four `open("/System/Library/Frameworks/<Name>.framework/<Name>")`
    call sites at vtable-load time keep their string arguments
    unchanged — on iOS the path argument is unused, on macOS it remains
    the dlopen target. **The vtable assembly code and every symbol
    `.get(b"VT...")` call are byte-identical across the two platforms.**
  - `Cargo.toml` `[target.'cfg(target_os = "macos")'.dependencies]` widens
    to `[target.'cfg(any(target_os = "macos", target_os = "ios"))'.dependencies]`
    so `libloading` is now pulled on both Apple platforms. Linux /
    Windows still see an empty rlib with no extra deps.
  - macOS behaviour is unchanged — the existing dlopen-at-first-use
    code path is preserved exactly. No new failure modes on macOS, no
    regression risk on the existing integration suite, no version-bump
    breaking-change implication.

- **Round 14: hard-cap + CBR rate-control writes
  (`DataRateLimits` / `ConstantBitRate`) across every VT encoder.**
  Round 13 closed the cadence-knob trio (`MaxKeyFrameInterval` /
  `MaxKeyFrameIntervalDuration` / `ExpectedFrameRate`); round 14 wires
  the two remaining rate-control knobs Apple documents alongside
  `AverageBitRate` in `VideoToolbox/VTCompressionProperties.h`:
  - `kVTCompressionPropertyKey_DataRateLimits` (CFArray\<CFNumber\>,
    alternating `[bytes, seconds, ...]` pairs). Per the SDK doc, "each
    hard limit is described by a data size in bytes and a duration in
    seconds, and requires that the total size of compressed data for any
    contiguous segment of that duration (in decode time) must not exceed
    the data size"; Apple documents "zero, one or two hard limits".
    Source: `params.options["data_rate_limits"]` parsed as a
    comma-separated list of `bytes:seconds` pairs (1–2 segments;
    whitespace tolerated). Examples: `"100000:1"` (single 100 KB / 1 s
    cap) or `"100000:1, 500000:5"` (composable 1 s + 5 s caps). Bytes
    is parsed as a non-negative integer clamped to `i32::MAX` (the
    SDK's `CFNumber<SInt32>` array-element type); seconds is parsed as
    a finite strictly-positive Float64. Negative bytes / non-positive
    or non-finite seconds / malformed input / 3+ segments are silently
    rejected and the encoder keeps VT's default of "no data rate
    limits".
  - `kVTCompressionPropertyKey_ConstantBitRate` (CFNumber bits/second,
    macOS 13.0+). Source: `params.options["constant_bit_rate"]` parsed
    as a non-negative integer clamped to `i32::MAX`. Per the SDK
    header, CBR "is intended for legacy CDN interop, not general
    streaming scenarios" and is mutually exclusive with
    `AverageBitRate` + `DataRateLimits`; on encoders / OS versions
    that don't support CBR, `VTSessionSetProperty` returns
    `kVTPropertyNotSupportedErr` and the bridge keeps the prior
    rate-control mode (non-fatal, matching every other round-9 /
    round-13 knob's failure semantics).
- New `sys::cf_array` helper backed by `CFArrayCreate` +
  `kCFTypeArrayCallBacks` (the exported `CFArrayCallBacks` singleton for
  `CFType` element arrays). The helper produces a `CFArray` whose
  retain/release pairs with each element's own ref-count, so the
  `DataRateLimits` write path can build a flat
  `[CFNumber<i32>, CFNumber<f64>, ...]` array, hand it to
  `VTSessionSetProperty`, and release every element + the array
  afterwards. New `FnCFArrayCreate` function-pointer type + raw
  `*const c_void` field on `Vtable` for the `kCFTypeArrayCallBacks` data
  symbol; new `unsafe impl Send + Sync for Vtable` (the callbacks symbol
  is a process-lifetime read-only data export — no thread affinity).
  Every prior CF helper (`cf_string`, `cf_number_i32`, `cf_number_f32`,
  `cf_number_f64`, `cf_data`, `cf_empty_dict`) is unchanged.
- The new property writes land in both encoder paths: `encoder.rs`
  (H.264 / HEVC `VtEncoder::create`) and `blob.rs` (MJPEG / ProRes
  `BlobEncoder::new`). The plumbing is identical so the bridge surface
  stays uniform; ProRes (fixed-CBR per profile) silently ignores both
  properties, matching the existing `AverageBitRate`-is-a-no-op-on-ProRes
  pattern. The recommended composition `AverageBitRate +
  DataRateLimits` (soft target + hard cap) works on H.264 / HEVC / MJPEG
  by setting `params.bit_rate` and `options["data_rate_limits"]`
  together.
- New `DataRateLimit` `pub(crate)` struct (`{ bytes: i32, seconds: f64 }`)
  in `encoder.rs` to model one CFArray segment. New parsers
  (`parse_data_rate_limits`, `parse_constant_bit_rate`) live alongside
  the round-13 cadence parsers, also `pub(crate)`, imported by `blob.rs`
  so the H.264/HEVC and MJPEG/ProRes paths share identical input
  semantics.
- Nine new unit tests covering the new parsers and their input ranges:
  `data_rate_limits_parser_single_segment` /
  `data_rate_limits_parser_two_segments` /
  `data_rate_limits_parser_whitespace_tolerated` (accepts the documented
  shapes — 1 segment, 2 segments, whitespace tolerated);
  `data_rate_limits_parser_rejects_more_than_two_segments` /
  `data_rate_limits_parser_rejects_zero_or_negative_seconds` /
  `data_rate_limits_parser_rejects_negative_or_oversize_bytes` /
  `data_rate_limits_parser_rejects_malformed_input` (rejects every
  SDK-documented out-of-range / malformed input class);
  `constant_bit_rate_parser` (accepts 0 / positive / whitespace, clamps
  overflow at `i32::MAX`, rejects negatives / floats / empty / non-
  numeric). One new integration test
  (`data_rate_and_cbr_knobs_round_trip_without_regression`) drives the
  live VT session for H.264 with `AverageBitRate + DataRateLimits` set
  (`make_h264_encoder`) and MJPEG with `ConstantBitRate` set
  (`make_jpeg_encoder`), and asserts both encoders still produce ≥ 1
  packet after 5 input frames + flush — the visible signal that VT
  accepted (or non-fatally ignored) the property writes.
- **Round 13: cadence-knob property writes
  (`MaxKeyFrameInterval` / `MaxKeyFrameIntervalDuration` /
  `ExpectedFrameRate`) across every VT encoder.** Until round 12 the
  bridge configured five compression properties (`RealTime`,
  `AllowFrameReordering`, `ProfileLevel`, `AverageBitRate`, `Quality`).
  Round 13 adds the three keyframe-cadence / frame-rate-hint properties
  Apple documents in `VideoToolbox/VTCompressionProperties.h` as the
  next-tier rate-control knobs alongside `AverageBitRate`:
  - `kVTCompressionPropertyKey_MaxKeyFrameInterval` (CFNumber\<int\>,
    "maximum interval between key frames in frames", 0 = no forced
    cadence). Source: `params.options["keyframe_interval"]` parsed as a
    non-negative integer; negatives / unparseable input are silently
    dropped and the encoder keeps VT's built-in default. Values above
    `i32::MAX` clamp to that ceiling rather than overflow.
  - `kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration`
    (CFNumber\<seconds\>, "maximum interval in seconds", 0 = no forced
    cadence). Source: `params.options["keyframe_interval_duration"]`
    parsed as a non-negative finite Float64; negatives / NaN /
    infinities are rejected. Per Apple's SDK doc, the duration cap and
    the frame-count cap are composable — VT picks whichever forces a
    keyframe first.
  - `kVTCompressionPropertyKey_ExpectedFrameRate` (CFNumber, fps). The
    encoder uses this for rate-control and energy budgeting. Source
    precedence: `params.options["expected_frame_rate"]` (Float64) if
    finite-positive, otherwise the stream's `params.frame_rate`
    (`Rational`) reduced via `as_f64()`. Zero / negative / non-finite
    inputs from either source are skipped so the encoder keeps VT's
    default cadence hint.
- New `sys::cf_number_f64` helper backed by the existing
  `CFNumberCreate` symbol and a new `K_CF_NUMBER_FLOAT_64_TYPE = 6`
  constant (per `CFNumber.h` enum order:
  `kCFNumberSInt8Type = 1 .. kCFNumberFloat32Type = 5,
  kCFNumberFloat64Type = 6`). Used by the new `MaxKeyFrameIntervalDuration`
  and `ExpectedFrameRate` writes. Every prior CFNumber call site
  (`cf_number_i32`, `cf_number_f32`) is unchanged.
- The new property writes land in both encoder paths: `encoder.rs`
  (H.264 / HEVC `VtEncoder::create`) and `blob.rs` (MJPEG / ProRes
  `BlobEncoder::new`). The plumbing is identical so the bridge surface
  stays uniform; ProRes / MJPEG are intra-only so VT silently ignores
  the keyframe-cadence properties on those codecs, matching the existing
  `AverageBitRate`-is-a-no-op-on-ProRes pattern. The `ExpectedFrameRate`
  hint reaches all four encoders.
- Five new unit tests covering the new helpers and their input ranges:
  `keyframe_interval_parser` (accepts 0 / positive / whitespace,
  clamps overflow at `i32::MAX`, rejects negatives / floats / empty),
  `keyframe_interval_duration_parser` (accepts 0 / finite-positive /
  whitespace, rejects negatives / NaN / ±infinity / empty),
  `expected_frame_rate_resolver` (4 sub-cases: derived from
  `Rational`, zero-denominator rejection, options override taking
  precedence, options fall-back when override is junk / non-finite /
  zero / negative). One new integration test
  `cadence_knobs_round_trip_without_regression` drives the live VT
  session for H.264 (`make_h264_encoder`) and MJPEG
  (`make_jpeg_encoder`) with all three knobs set and asserts both
  encoders still produce ≥ 1 packet after 5 input frames + flush —
  the visible signal that VT accepted the property writes.
- **Round 12: H.264 + HEVC profile-alias map expanded; canonical
  `H264_*` / `HEVC_*` pass-through.** Round 9 wired the short `_AutoLevel`
  aliases (H.264: `baseline` / `main` / `high` / `extended`; HEVC: `main` /
  `main10` / `main4_2_2_10`) onto `kVTCompressionPropertyKey_ProfileLevel`,
  and *documented* a literal-Apple-string pass-through ("callers can write
  the literal Apple string into `options["profile"]` if needed without
  breaking"). The underlying `match` arm fell through to `_ => None`,
  silently dropping every literal Apple string. Round 12 closes that gap
  and expands the alias table to every value the macOS SDK header
  `VideoToolbox/VTCompressionProperties.h` declares:
  - H.264 named-level aliases (per `VTCompressionProperties.h`):
    `baseline_{1_3, 3_0, 3_1, 3_2, 4_0, 4_1, 4_2, 5_0, 5_1, 5_2}` →
    `H264_Baseline_{1_3, 3_0, ..., 5_2}`; `main_{3_0..5_2}` →
    `H264_Main_{3_0..5_2}`; `high_{3_0..5_2}` →
    `H264_High_{3_0..5_2}`; `extended_5_0` → `H264_Extended_5_0`
    (the only Extended-with-level constant Apple declares). 30 new
    aliases. Inputs are case-insensitive; outputs match the SDK symbol
    string verbatim.
  - H.264 Constrained Baseline / Constrained High (macOS 12.0+ per
    SDK header): new aliases `constrained_baseline` / `constrainedbaseline`
    / `constrained_baseline_auto` / `constrained_baseline_autolevel` →
    `H264_ConstrainedBaseline_AutoLevel`; same shape for `constrained_high`
    → `H264_ConstrainedHigh_AutoLevel`.
  - H.264 canonical-form pass-through: input strings that exactly match
    the documented set of `H264_*` SDK-symbol values (`H264_Baseline_AutoLevel`,
    `H264_High_5_1`, …) pass through verbatim. The pass-through is
    deliberately a closed set — junk strings like `"H264_DoesNotExist"`
    or `"H264_High_99_9"` are rejected so a caller can't drive arbitrary
    CFString junk into the VT property.
  - HEVC bug fix: round 9's `main4_2_2_10` / `main422_10` aliases emitted
    `"HEVC_Main4_2_2_10_AutoLevel"`, but the **actual** CFString value
    Apple declares (per `VTCompressionProperties.h`'s
    `kVTProfileLevel_HEVC_Main42210_AutoLevel`, macOS 12.3+) is
    `"HEVC_Main42210_AutoLevel"` — five contiguous digits, no interior
    underscores. VT would have rejected the malformed string and silently
    kept the default Main profile, so the alias didn't actually do
    anything before. Round 12 emits the correct `"HEVC_Main42210_AutoLevel"`
    string, preserves the round-9 input aliases, and adds the
    SDK-symbol-form aliases `main42210` / `main_42210` /
    `main_4_2_2_10_autolevel`.
  - HEVC canonical-form pass-through: the three documented values
    (`HEVC_Main_AutoLevel`, `HEVC_Main10_AutoLevel`,
    `HEVC_Main42210_AutoLevel`) pass through verbatim. The pre-round-12
    buggy form `"HEVC_Main4_2_2_10_AutoLevel"` is **rejected** (it was
    never a valid VT property value anyway).
  - No new sys.rs / blob.rs / decoder.rs FFI surface. Round 12 is a
    pure alias-table / output-value change in `encoder.rs`; the
    `kVTCompressionPropertyKey_ProfileLevel` write path itself is
    unchanged.
- Seven new unit tests on top of the existing two:
  - `h264_constrained_aliases` — every input form × the two constrained
    profiles maps to the canonical AutoLevel string.
  - `h264_named_level_aliases` — every Baseline / Main / High named-level
    alias from the SDK header (mixed case) maps to the corresponding
    `H264_*` SDK value plus `Extended_5_0`.
  - `h264_canonical_passthrough` — every documented Apple value passes
    through verbatim; junk strings like `"H264_DoesNotExist"` /
    `"H264_High_99_9"` are rejected.
  - `hevc_main42210_emits_actual_sdk_value` — every input alias and the
    canonical pass-through emit `HEVC_Main42210_AutoLevel`.
  - `hevc_canonical_passthrough` — the three documented Apple values
    pass; the pre-round-12 buggy `HEVC_Main4_2_2_10_AutoLevel` form is
    rejected.
  - `h264_profile_aliases` / `hevc_profile_aliases` — unchanged except
    the `hevc_profile_aliases` test drops the now-incorrect
    `Main4_2_2_10_AutoLevel` assertion (covered by the dedicated
    `hevc_main42210_emits_actual_sdk_value` test above).

- **Round 11: VVC (H.266) video decode** via `VTDecompressionSession`
  (`kCMVideoCodecType_VVC` = `'vvc1'` = `0x7676_6331`). Decode-only —
  VideoToolbox does not yet expose a VVC compression session at the time
  of this round, so `make_vvc_decoder` registers a decoder against
  `CodecId::new("h266")` (matching the workspace's pure-Rust
  `oxideav-h266` codec id) and there is no encoder factory.
  - Hardware decode is gated to Apple Silicon **M3+** on macOS 26+; on
    older OS / hardware VideoToolbox either falls back to its internal
    software VVC path (where available) or returns a non-zero
    `OSStatus` at session creation, in which case the registry's SW
    fallback to the pure-Rust `oxideav-h266` decoder handles the stream.
  - New `FrameSplit::VvcEs` framer on `BlobDecoder`. Splits an incoming
    VVC Annex-B elementary stream into per-access-unit payloads per
    H.266 §7.4.2.4 / Annex B. Recognises both 3-byte (`00 00 01`) and
    4-byte (`00 00 00 01`) start code prefixes. Opens a new access unit
    at the next `AUD_NUT` (= 20), the next `PH_NUT` (= 19), or the next
    VCL NAL when no `PH_NUT` has been seen since the previous boundary.
    Leading non-VCL NAL units (DCI / OPI / VPS / SPS / PPS /
    PREFIX_APS) preceding the first VCL ride with the first access
    unit so the configuration travels with it. Existing
    Whole / Mpeg2Es / Mpeg4PartTwoEs / Av1Whole framers unchanged.
  - vvcC configuration-record path. On the first packet, `BlobDecoder`
    calls `extract_vvc_config_prefix` to harvest the leading non-VCL
    NAL units (DCI / OPI / VPS / SPS / PPS / PREFIX_APS), wraps them in
    a `VvcDecoderConfigurationRecord` via
    `build_vvc_decoder_config_record` (per ISO/IEC 14496-15 §11.2.4.2.2),
    and supplies the blob to `CMVideoFormatDescriptionCreate` via
    `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms = {
    "vvcC": CFData }`. Same shape as round 7's MPEG-4 Part 2 ESDS and
    round 10's AV1 av1C paths — the round-10 `extradata:
    Option<(&'static str, Vec<u8>)>` storage form already supports the
    new `"vvcC"` atom key with no plumbing change.
  - VvcDecoderConfigurationRecord layout (`ptl_present_flag = 0` form,
    `LengthSizeMinusOne = 3` — VT re-extracts PTL from the SPS):
    - Byte 0: `reserved(5) = 0b11111` | `LengthSizeMinusOne(2) = 3` |
      `ptl_present_flag(1) = 0` → `0b11111110` = `0xFE`.
    - Byte 1: `num_of_arrays`.
    - Bytes 2..: per array (in the spec-recommended order DCI, OPI, VPS,
      SPS, PPS, PREFIX_APS — empty arrays omitted) —
      `array_completeness(1) = 0 | reserved(2) = 0 | NAL_unit_type(5)`;
      then (when `NAL_unit_type` is neither `DCI_NUT` nor `OPI_NUT`) a
      `u16 num_nalus`; then per NAL unit a `u16 nal_unit_length` and
      `nal_unit_length` raw NAL bytes (header + RBSP, no start code
      prefix).
  - VVC NAL header decode per H.266 §7.3.1.2: `forbidden_zero_bit(1) |
    nuh_reserved_zero_bit(1) | nuh_layer_id(6)` in byte 0,
    `nal_unit_type(5) | nuh_temporal_id_plus1(3)` in byte 1. The
    `vvc_nal_unit_type` helper refuses non-zero values in the top two
    bits of byte 0 (per §7.4.2.2 those must always be 0) and short
    inputs.
  - Codec tags: `vvc1 / vvi1 / VVC1 / H266 / h266` (fourcc) and
    `V_MPEGI/ISO/VVC` (Matroska). Registers with `priority = 10`,
    `hardware_accelerated = true`.
  - Public API additions on `oxideav_videotoolbox::blob`:
    - `K_CM_VIDEO_CODEC_TYPE_VVC = 0x7676_6331`.
    - NAL-unit-type constants from H.266 Table 5: `VVC_NUT_TRAIL`,
      `VVC_NUT_STSA`, `VVC_NUT_RADL`, `VVC_NUT_RASL`,
      `VVC_NUT_IDR_W_RADL`, `VVC_NUT_IDR_N_LP`, `VVC_NUT_CRA`,
      `VVC_NUT_GDR`, `VVC_NUT_OPI`, `VVC_NUT_DCI`, `VVC_NUT_VPS`,
      `VVC_NUT_SPS`, `VVC_NUT_PPS`, `VVC_NUT_PREFIX_APS`,
      `VVC_NUT_SUFFIX_APS`, `VVC_NUT_PH`, `VVC_NUT_AUD`, `VVC_NUT_EOS`,
      `VVC_NUT_EOB`, `VVC_NUT_PREFIX_SEI`, `VVC_NUT_SUFFIX_SEI`,
      `VVC_NUT_FD`.
    - `vvc_nal_unit_type(&[u8]) -> Option<u8>` (decode `nal_unit_type`).
    - `vvc_is_vcl_nut(u8) -> bool` (Table 5 VCL classification: types
      0..11).
    - `split_vvc_nal_units(&[u8]) -> Vec<(usize, usize)>` (Annex B byte
      stream → list of `(offset, length)` NAL ranges, both 3-byte and
      4-byte start codes).
    - `extract_vvc_nals_of_type(&[u8], u8) -> Vec<&[u8]>` (filter by
      NAL unit type).
    - `extract_vvc_config_prefix(&[u8]) -> Option<&[u8]>` (slice from
      offset 0 up to the start code prefix of the first VCL NAL unit;
      `None` when the stream starts with a VCL or contains no start
      codes).
    - `build_vvc_decoder_config_record(&[u8]) -> Vec<u8>`.
    - `split_vvc_access_units(&[u8]) -> Vec<&[u8]>`.
    - `make_vvc_decoder(&CodecParameters) -> Result<Box<dyn Decoder>>`.
  - `register` now installs a VVC decoder (and asserts via
    `register_installs_vvc_decode_only` that it installs *no* VVC
    encoder).
  - **No `vvc_decode_against_ffmpeg` integration test yet.** ffmpeg
    ships a VVC *decoder* but no VVC *encoder*, so this round cannot
    synthesise a fixture in-process the way the AV1 / VP9 / MPEG-2 /
    MPEG-4 Pt 2 rounds did. A follow-up round can either commit a
    pre-extracted clean-room VVC bitstream as a test fixture (no
    encoder dependency) or invoke a vendor encoder binary as an opaque
    black-box validator. The wiring is complete; only the encoder-side
    of the integration fixture is missing.
- Twenty new unit tests covering the VVC paths:
  - NAL header decoder — `vvc_nal_unit_type_extracts_5_bit_field`,
    `vvc_nal_unit_type_rejects_nonzero_top_bits`,
    `vvc_nal_unit_type_short_input_is_none`,
    `vvc_vcl_classification_matches_table_5`.
  - Annex-B walker — `vvc_split_nal_units_three_byte_start_codes`,
    `vvc_split_nal_units_four_byte_start_codes`,
    `vvc_extract_nals_of_type_returns_only_matching`.
  - Config-prefix extractor —
    `vvc_extract_config_prefix_stops_at_first_vcl`,
    `vvc_extract_config_prefix_none_when_starts_with_vcl`,
    `vvc_extract_config_prefix_none_on_empty_or_no_start_codes`.
  - vvcC builder — `vvc_decoder_config_record_byte_0_is_0xfe`,
    `vvc_decoder_config_record_lists_only_present_arrays`,
    `vvc_decoder_config_record_array_layout`,
    `vvc_decoder_config_record_dci_array_omits_num_nalus`.
  - Access-unit splitter —
    `vvc_split_access_units_single_picture_with_param_sets`,
    `vvc_split_access_units_two_pictures_first_keeps_param_sets`,
    `vvc_split_access_units_aud_starts_new_unit`,
    `vvc_split_access_units_ph_starts_new_unit`,
    `vvc_split_access_units_empty_buffer_yields_nothing`.
  - Codec-type constant — `vvc_codec_type_is_vvc1_fourcc`.
- Three new integration tests in `tests/roundtrip.rs`:
  - `register_installs_vvc_decode_only` — decoder registered for
    `CodecId::new("h266")` and no encoder.
  - `vvc_codec_type_equals_vvc1_fourcc` — the public
    `K_CM_VIDEO_CODEC_TYPE_VVC` constant equals
    `u32::from_be_bytes(b"vvc1")` = `0x7676_6331`.
  - `vvc_make_decoder_requires_width_height` — `make_vvc_decoder`
    rejects calls without explicit width / height in `CodecParameters`,
    matching the other blob decoders.

- **Round 10: AV1 `av1C` extension-atom path** — AV1 Sequence Header OBU
  → `AV1CodecConfigurationRecord` wrapper supplied to VT via
  `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms`. Round
  8 wired the AV1 decoder against `(codec_type, width, height)` only —
  fine on hosts where the VT AV1 decoder is lenient about extracting the
  Sequence Header from the first temporal unit, but on stricter hosts
  session creation returned a non-zero `OSStatus`. Round 10 closes that
  gap by mirroring the round-7 MPEG-4 Part 2 ESDS pattern:
  - A new `FrameSplit::Av1Whole` framer (semantically `Whole`, with a
    first-packet sniffer) walks the leading temporal unit's OBU list,
    locates the OBU whose `obu_type == OBU_SEQUENCE_HEADER` (= 1, per
    AV1 spec §6.2.2), and builds an av1C record from it.
  - `AV1CodecConfigurationRecord` layout per the AV1 ISO Base Media
    File Format Binding Specification §2.3.3:
    - Byte 0: `marker(1) = 1`, `version(7) = 1` → `0x81`.
    - Byte 1: `seq_profile(3)` + `seq_level_idx_0(5)`.
    - Byte 2: `seq_tier_0(1)` + `high_bitdepth(1)` + `twelve_bit(1)` +
      `monochrome(1)` + `chroma_subsampling_x(1)` +
      `chroma_subsampling_y(1)` + `chroma_sample_position(2)`.
    - Byte 3: `reserved(3) = 0` + `initial_presentation_delay_present(1)
      = 0` + `reserved(4) = 0`.
    - Bytes 4..: `configOBUs[]` — the Sequence Header OBU verbatim.
      The binding spec §2.3.4 mandates the Sequence Header be the first
      OBU in `configOBUs` when present.
  - `parse_av1_seq_header_fields` is a small MSB-first bit-reader that
    walks the Sequence Header OBU payload per AV1 spec §5.5.1 + §5.5.2
    to recover `seq_profile`, `seq_level_idx_0`, `seq_tier_0`,
    `high_bitdepth`, `twelve_bit`, `monochrome`,
    `chroma_subsampling_{x,y}`, and `chroma_sample_position`. The walker
    bails to `Av1SeqHeaderFields::defaults()` (8-bit 4:2:0 main-profile)
    on any short read or any path that would require a uvlc decode —
    the `configOBUs` field still carries the Sequence Header verbatim,
    which per the binding spec §2.3.4 is the authoritative source for
    consumers that re-derive the record body.
  - Extension-atom storage generalised: round 7's
    `BlobDecoder::extradata_esds: Option<Vec<u8>>` is now
    `extradata: Option<(&'static str, Vec<u8>)>` so the same
    `ensure_session` plumbing supports both `"esds"` (MPEG-4 Part 2)
    and `"av1C"` (AV1). No behavioural change on the MPEG-4 Part 2 path
    — the new field is identical in shape, just typed.
  - `find_av1_obu` walks an AV1 low-overhead-bitstream temporal unit's
    OBU list per spec §5.3.2 (1-byte header, optional 1-byte extension
    header, uleb128 `obu_size`, payload). The low-overhead bitstream
    format requires `obu_has_size_field = 1` on every OBU (spec §5.2);
    the walker refuses any header without that bit and any header whose
    `obu_forbidden_bit` is set. `read_uleb128` decodes the AV1 uleb128
    form (spec §4.10.5 / §5.3.1) with a strict 8-continuation-byte and
    32-bit-value cap.
  - Public API additions on `oxideav_videotoolbox::blob`:
    `extract_av1_sequence_header_obu(&[u8]) -> Option<&[u8]>`,
    `build_av1c_config_record(&[u8]) -> Vec<u8>`,
    `parse_av1_seq_header_fields(&[u8]) -> Av1SeqHeaderFields`, and the
    `Av1SeqHeaderFields` struct (9 `u8` fields covering `seq_profile`,
    `seq_level_idx_0`, `seq_tier_0`, plus the colour-config quintet).
  - `make_av1_decoder` now dispatches through `FrameSplit::Av1Whole`
    instead of `FrameSplit::Whole`; existing callers see no API change.
- Eleven new unit tests covering the round-10 paths:
  - OBU walker — `av1_extract_sequence_header_obu_returns_full_obu_bytes`,
    `av1_extract_sequence_header_obu_none_when_absent`,
    `av1_extract_sequence_header_obu_rejects_forbidden_bit`,
    `av1_extract_sequence_header_obu_requires_size_field`,
    `av1_extract_sequence_header_obu_rejects_truncated_payload`.
  - av1C builder — `av1c_marker_and_version_byte_is_0x81`,
    `av1c_byte_3_is_zero_reserved`,
    `av1c_configobus_includes_sequence_header_obu_verbatim`,
    `av1c_byte_1_packs_seq_profile_and_level`.
  - Sequence Header field parser —
    `av1_seq_header_fields_reduced_still_picture_header`,
    `av1_seq_header_fields_defaults_on_empty`.

- **Round 9: encoder knobs across all four VT encoders.** Until round 8 the
  H.264 / HEVC / MJPEG / ProRes compression sessions configured only
  `RealTime = true`, `AllowFrameReordering = false`, and a hardcoded H.264
  Baseline / HEVC Main `ProfileLevel`. Round 9 turns three of the public
  `CodecParameters` fields into live properties:
  - `params.bit_rate: Option<u64>` flows into
    `kVTCompressionPropertyKey_AverageBitRate` (CFNumber-i32, saturating-
    clamped to `i32::MAX`) for H.264 / HEVC / MJPEG. ProRes accepts and
    silently ignores the property (each ProRes profile is fixed-CBR).
  - `params.options["quality"]` parses as a Float32 in `[0.0, 1.0]`
    (validated finite) and flows into `kVTCompressionPropertyKey_Quality`
    (CFNumber-Float32). The MJPEG encoder treats it as its primary
    quality lever; H.264 / HEVC / ProRes accept it as a hint that
    interacts with the rate-control mode.
  - `params.options["profile"]` accepts the short aliases `baseline` /
    `main` / `high` / `extended` (H.264) and `main` / `main10` /
    `main4_2_2_10` (HEVC), case-insensitively, and maps each to the
    canonical `kVTProfileLevel_*_AutoLevel` string Apple's VideoToolbox
    expects on `kVTCompressionPropertyKey_ProfileLevel`. Empty / unknown
    values keep the codec's built-in default — every existing call site
    sees no behaviour change.
- **ProRes profile selection in the factory.** `make_prores_encoder` now
  reads `params.tag` and dispatches to one of the six
  `kCMVideoCodecType_AppleProRes*` constants (`apco` Proxy, `apcs` LT,
  `apcn` 422 [default], `apch` HQ, `ap4h` 4444, `ap4x` 4444 XQ) via a new
  public helper `prores_codec_type_for_tag(Option<&CodecTag>) -> Option<u32>`.
  Missing / non-ProRes tags fall back to ProRes 422 (the round-3 default)
  so existing callers see no change.
- `K_CF_NUMBER_FLOAT_32_TYPE = 5` constant and `cf_number_f32(vt, v)`
  helper in `sys.rs` (per Apple's `CFNumber.h` enum order). Used by the
  new `Quality` plumbing; the prior `cf_number_i32` is unchanged.
- Five new unit tests:
  - `h264_profile_aliases` — every documented alias maps to the canonical
    `kVTProfileLevel_H264_*_AutoLevel` string; empty / unknown returns
    `None`.
  - `hevc_profile_aliases` — same for HEVC's `Main` / `Main10` /
    `Main4_2_2_10` flavours.
  - `prores_codec_type_constants_match_fourcc` — every
    `K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_*` equals its documented fourcc.
  - `prores_tag_dispatch_each_fourcc` — walks every ProRes fourcc
    through `prores_codec_type_for_tag`.
  - `prores_tag_dispatch_falls_back_on_unknown` — covers the unknown
    fourcc / non-Fourcc / `None` paths.
- One new integration test (`encoder_knobs_round_trip_without_regression`)
  exercises the live property writes against the macOS VT session for
  H.264 (`bit_rate` + `profile` + `quality`) and ProRes LT (tag-driven
  profile selection) and verifies the round-trip still produces packets.

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

- **Round 8: AV1 video decode** via `VTDecompressionSession`
  (`kCMVideoCodecType_AV1` = `'av01'` = `0x6176_3031`). Decode-only —
  VideoToolbox exposes an AV1 *compression* session on macOS 14+ for
  M3+ hardware, but encode wiring (callback, source pixel-buffer pool,
  rate-control properties) is a follow-up round, so
  `make_av1_decoder` registers a decoder against
  `CodecId::new("av1")` and there is no encoder factory.
  - Hardware decode is gated to Apple Silicon **M3+**; on older Apple
    Silicon (M1 / M2) and Intel hosts VideoToolbox either falls back to
    its internal software AV1 path (on macOS versions where it exists)
    or returns a non-zero `OSStatus` at session creation, in which case
    the registry's SW fallback to the pure-Rust `oxideav-av1` decoder
    handles the stream.
  - AV1 reuses the existing blob `FrameSplit::Whole` path: frames are
    container-framed (IVF / Matroska / MP4 / WebM / RTP) and arrive as
    one self-contained AV1 temporal unit per `Packet`. No in-codec
    splitter is needed — AV1 has no Annex-B / picture-start-code
    mechanism that would require a per-frame carve.
  - Codec tags: `av01 / AV01` (fourcc, matching the AV1 ISOBMFF
    `'av01'` sample entry) and `V_AV1` (Matroska). Registers with
    `priority = 10`, `hardware_accelerated = true`.
  - Decode validated against ffmpeg (selecting the AV1 reference
    encoder) as a black-box validator. `av1_decode_against_ffmpeg`
    parses the IVF container (32-byte file header + per-frame 12-byte
    `(frame_size, pts)` header + payload, via the existing `parse_ivf`
    helper shared with the VP9 test), feeds each temporal unit through
    the VT decoder, and compares to ffmpeg's own software decode
    (PSNR_Y ≥ 30 dB). The test self-skips when ffmpeg, its AV1
    reference encoder, the framework, or the VT AV1 decoder is
    unavailable on the host.
  - `av1C` configuration-record (extension-atom) path is **not** wired
    yet — the round-7 ESDS plumbing in `BlobDecoder::ensure_session`
    already supports an arbitrary extension-atom key, so adding `av1C`
    is a small follow-up once a host needs it (analogous to the
    round-6→round-7 MPEG-4 Part 2 gap closure).
  - `register` now installs an AV1 decoder (and asserts via
    `register_installs_av1_decode_only` that it installs *no* AV1
    encoder in round 8).
  - Public API additions on `oxideav_videotoolbox::blob`:
    `make_av1_decoder(&CodecParameters) -> Result<Box<dyn Decoder>>`
    and the constant `K_CM_VIDEO_CODEC_TYPE_AV1`.
- Two new unit / integration tests:
  - `av1_codec_type_is_av01_fourcc` — the codec-type constant equals
    `u32::from_be_bytes(b"av01")` = `0x6176_3031`.
  - `register_installs_av1_decode_only` — decoder is installed for
    `CodecId::new("av1")` and no encoder is.
  - `av1_decode_against_ffmpeg` (integration) — end-to-end IVF → VT →
    PSNR_Y ≥ 30 dB against ffmpeg's own decode of the same fixture.

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
  - Decode validated against ffmpeg (selecting the VP9 reference encoder) as
    a black-box validator. The test parses the IVF container (32-byte file
    header + per-frame 12-byte `(frame_size, pts)` header + payload) to
    recover individual VP9 frames, feeds each through the VT decoder, and
    compares to ffmpeg's own software decode (PSNR_Y ≥ 30 dB). The test
    self-skips when ffmpeg, its VP9 reference encoder, the framework, or
    the VT VP9 decoder is unavailable on the host.
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
