//! Runtime-loaded VideoToolbox + supporting framework handles.
//!
//! Loaded once via `OnceLock` on first use and cached for the process
//! lifetime. If any framework fails to dlopen the cache stores the
//! error so subsequent calls don't repeatedly hammer dyld.
//!
//! Frameworks needed for a usable VT bridge:
//!
//! | Framework        | Purpose                                          |
//! |------------------|--------------------------------------------------|
//! | VideoToolbox     | `VTDecompressionSession*`, `VTCompressionSession*` |
//! | CoreVideo        | `CVPixelBuffer*`, `CVImageBuffer*`               |
//! | CoreMedia        | `CMSampleBuffer*`, `CMVideoFormatDescription*`   |
//! | CoreFoundation   | `CFRetain` / `CFRelease` / `CFNumber*` / `CFDictionary*` |

use libloading::Library;
use std::ffi::c_void;
use std::sync::OnceLock;

// ─────────────────────────── opaque CF/CM/CV/VT types ─────────────────────────

/// Opaque types — pointer-sized; we only pass them around as raw `*mut`.
pub type CFTypeRef = *mut c_void;
pub type CFDictionaryRef = *mut c_void;
pub type CFAllocatorRef = *mut c_void;
pub type CFStringRef = *mut c_void;
pub type CFNumberRef = *mut c_void;
pub type CFArrayRef = *mut c_void;
pub type CFDataRef = *mut c_void;

pub type CMFormatDescriptionRef = *mut c_void;
pub type CMVideoFormatDescriptionRef = CMFormatDescriptionRef;
pub type CMSampleBufferRef = *mut c_void;
pub type CMBlockBufferRef = *mut c_void;

pub type CVPixelBufferRef = *mut c_void;
pub type CVImageBufferRef = *mut c_void;

pub type VTDecompressionSessionRef = *mut c_void;
pub type VTCompressionSessionRef = *mut c_void;

/// OSStatus (returned by most CM/VT functions).
pub type OSStatus = i32;
/// CVReturn (returned by CV functions).
pub type CVReturn = i32;

pub const K_OS_STATUS_NO_ERROR: OSStatus = 0;
pub const K_CV_RETURN_SUCCESS: CVReturn = 0;

/// CMTime: numerator/denominator/flags/epoch (64+32+32+64 = 24 bytes).
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

/// `kCMTimeFlags_Valid` (1 << 0) per CoreMedia's `CMTime.h`: "Must be
/// set, or the CMTime is considered invalid." All other flag bits
/// (rounded / ±infinity / indefinite) qualify a *valid* time; a CMTime
/// whose flags word has bit 0 clear is invalid regardless of its
/// value/timescale fields.
pub const K_CM_TIME_FLAGS_VALID: u32 = 1 << 0;

impl CMTime {
    pub fn make(value: i64, timescale: i32) -> Self {
        Self {
            value,
            timescale,
            flags: K_CM_TIME_FLAGS_VALID,
            epoch: 0,
        }
    }

    pub fn zero() -> Self {
        Self::make(0, 1)
    }

    /// `kCMTimeInvalid` — the all-zero struct. Per `CMTime.h` a CMTime
    /// is invalid exactly when `kCMTimeFlags_Valid` is clear, and the
    /// exported `kCMTimeInvalid` constant is the zero-initialised
    /// struct. Use this (never a "valid" sentinel value such as
    /// `i64::MIN`) wherever the framework accepts an optional time,
    /// e.g. an unknown decode timestamp in `CMSampleTimingInfo`.
    pub fn invalid() -> Self {
        Self {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        }
    }

    /// Whether the framework will treat this time as valid
    /// (`kCMTimeFlags_Valid` set).
    pub fn is_valid(&self) -> bool {
        self.flags & K_CM_TIME_FLAGS_VALID != 0
    }
}

/// CMSampleTimingInfo — used in CMSampleBufferCreateReady.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct CMSampleTimingInfo {
    pub duration: CMTime,
    pub presentation_time_stamp: CMTime,
    pub decode_time_stamp: CMTime,
}

impl CMSampleTimingInfo {
    pub fn zero() -> Self {
        Self {
            duration: CMTime::make(0, 1),
            presentation_time_stamp: CMTime::make(0, 1),
            decode_time_stamp: CMTime::invalid(),
        }
    }
}

// ─────────────────────────── VT callback types ────────────────────────────────

/// Callback for VTDecompressionSession: called by VT with each decoded frame.
/// `output_callback_ref_con` is our Box<dyn FnMut(...)> as *mut c_void.
/// `source_frame_ref_con` is unused (we use `output_callback_ref_con`).
///
/// The signature mirrors `VTDecompressionOutputCallback` from
/// `VideoToolbox/VTDecompressionSession.h` exactly — including the two
/// trailing by-value `CMTime` parameters (`presentationTimeStamp`,
/// `presentationDuration`). An earlier revision of this binding omitted
/// them; that happened to work on the AArch64 / x86-64 C calling
/// conventions (a callee may ignore trailing arguments), but it made the
/// decoded frame's presentation timestamp unrecoverable, so every
/// decoded `VideoFrame` came back with `pts: None`.
pub type VTDecompressionOutputCallback = unsafe extern "C" fn(
    decomp_output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image_buffer: CVImageBufferRef,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

/// Wrapper passed to VT as the record.
#[repr(C)]
pub struct VTDecompressionOutputCallbackRecord {
    pub decomp_output_callback: VTDecompressionOutputCallback,
    pub decomp_output_ref_con: *mut c_void,
}

/// Callback for VTCompressionSession: called by VT with each encoded sample buffer.
pub type VTCompressionOutputCallback = unsafe extern "C" fn(
    output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    sample_buffer: CMSampleBufferRef,
);

// ─────────────────────────── CFStringEncoding ─────────────────────────────────

pub const K_CF_STRING_ENCODING_UTF8: u32 = 0x08000100;

// ─────────────────────────── CFNumberType ─────────────────────────────────────

pub const K_CF_NUMBER_SI_NT32_TYPE: i32 = 3;
pub const K_CF_NUMBER_SI_NT64_TYPE: i32 = 4;
/// `kCFNumberFloat32Type` per Apple's `CFNumber.h` enum order
/// (kCFNumberSInt8Type = 1 .. kCFNumberFloat32Type = 5,
/// kCFNumberFloat64Type = 6). Used to wrap the
/// `kVTCompressionPropertyKey_Quality` value (CFNumber 0.0..1.0).
pub const K_CF_NUMBER_FLOAT_32_TYPE: i32 = 5;
/// `kCFNumberFloat64Type` per Apple's `CFNumber.h` enum order
/// (immediately follows `kCFNumberFloat32Type`). Used to wrap the
/// `kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration` value
/// (CFNumber\<seconds\>) and the `kVTCompressionPropertyKey_ExpectedFrameRate`
/// value (CFNumber, fps), both documented in
/// `VideoToolbox/VTCompressionProperties.h`.
pub const K_CF_NUMBER_FLOAT_64_TYPE: i32 = 6;

// ─────────────────────────── kCVPixelFormatType constants ─────────────────────

/// '420v' — video-range 4:2:0, biplanar Y+UV (NV12 variant used by VT).
#[allow(non_upper_case_globals)]
pub const K_CV_PIXEL_FORMAT_420_YPCBCRi8_BI_PLANAR_VIDEO_RANGE: u32 = 0x34323076; // '420v'
/// '420f' — full-range 4:2:0, biplanar Y+UV.
#[allow(non_upper_case_globals)]
pub const K_CV_PIXEL_FORMAT_420_YPCBCRi8_BI_PLANAR_FULL_RANGE: u32 = 0x34323066; // '420f'

// kCVPixelBufferLockFlags_ReadOnly = 1
pub const K_CV_PIXEL_BUFFER_LOCK_FLAGS_READ_ONLY: u64 = 1;

// ─────────────────────────── Function pointer types ───────────────────────────

// VideoToolbox
pub type FnVTDecompressionSessionCreate = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    video_format_description: CMVideoFormatDescriptionRef,
    video_decoder_specification: CFDictionaryRef,
    destination_image_buffer_attributes: CFDictionaryRef,
    output_callback: *const VTDecompressionOutputCallbackRecord,
    decomp_session_out: *mut VTDecompressionSessionRef,
) -> OSStatus;

pub type FnVTDecompressionSessionDecodeFrame = unsafe extern "C" fn(
    session: VTDecompressionSessionRef,
    sample_buffer: CMSampleBufferRef,
    decode_flags: u32,
    source_frame_ref_con: *mut c_void,
    info_flags_out: *mut u32,
) -> OSStatus;

pub type FnVTDecompressionSessionFinishDelayedFrames =
    unsafe extern "C" fn(session: VTDecompressionSessionRef) -> OSStatus;

pub type FnVTDecompressionSessionInvalidate =
    unsafe extern "C" fn(session: VTDecompressionSessionRef);

pub type FnVTCompressionSessionCreate = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    width: i32,
    height: i32,
    codec_type: u32,
    encoder_specification: CFDictionaryRef,
    source_image_buffer_attributes: CFDictionaryRef,
    compressed_data_allocator: CFAllocatorRef,
    output_callback: VTCompressionOutputCallback,
    output_callback_ref_con: *mut c_void,
    compression_session_out: *mut VTCompressionSessionRef,
) -> OSStatus;

pub type FnVTCompressionSessionEncodeFrame = unsafe extern "C" fn(
    session: VTCompressionSessionRef,
    image_buffer: CVImageBufferRef,
    presentation_time_stamp: CMTime,
    duration: CMTime,
    frame_properties: CFDictionaryRef,
    source_frame_ref_con: *mut c_void,
    info_flags_out: *mut u32,
) -> OSStatus;

pub type FnVTCompressionSessionCompleteFrames = unsafe extern "C" fn(
    session: VTCompressionSessionRef,
    complete_until_presentation_time_stamp: CMTime,
) -> OSStatus;

pub type FnVTCompressionSessionInvalidate = unsafe extern "C" fn(session: VTCompressionSessionRef);

pub type FnVTCompressionSessionPrepareToEncodeFrames =
    unsafe extern "C" fn(session: VTCompressionSessionRef) -> OSStatus;

pub type FnVTSessionSetProperty = unsafe extern "C" fn(
    session: *mut c_void,
    property_key: CFStringRef,
    property_value: CFTypeRef,
) -> OSStatus;

// CoreMedia
pub type FnCMSampleBufferCreateReady = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    data_buffer: CMBlockBufferRef,
    format_description: CMFormatDescriptionRef,
    num_samples: i64,
    num_sample_timing_entries: i64,
    sample_timing_array: *const CMSampleTimingInfo,
    num_sample_size_entries: i64,
    sample_size_array: *const usize,
    sample_buffer_out: *mut CMSampleBufferRef,
) -> OSStatus;

pub type FnCMSampleBufferGetDataBuffer =
    unsafe extern "C" fn(sample_buffer: CMSampleBufferRef) -> CMBlockBufferRef;

pub type FnCMBlockBufferCopyDataBytes = unsafe extern "C" fn(
    the_source_buffer: CMBlockBufferRef,
    offset_to_data: usize,
    data_length: usize,
    destination: *mut c_void,
) -> OSStatus;

pub type FnCMSampleBufferGetImageBuffer =
    unsafe extern "C" fn(sample_buffer: CMSampleBufferRef) -> CVImageBufferRef;

pub type FnCMBlockBufferCreateWithMemoryBlock = unsafe extern "C" fn(
    structure_allocator: CFAllocatorRef,
    memory_block: *mut c_void,
    block_length: usize,
    block_allocator: CFAllocatorRef,
    custom_block_source: *const c_void,
    offset_to_data: usize,
    data_length: usize,
    flags: u32,
    block_buffer_out: *mut CMBlockBufferRef,
) -> OSStatus;

pub type FnCMBlockBufferGetDataLength = unsafe extern "C" fn(the_buffer: CMBlockBufferRef) -> usize;

pub type FnCMVideoFormatDescriptionCreate = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    codec_type: u32,
    width: i32,
    height: i32,
    extensions: CFDictionaryRef,
    format_description_out: *mut CMVideoFormatDescriptionRef,
) -> OSStatus;

pub type FnCMVideoFormatDescriptionCreateFromH264ParameterSets = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    parameter_set_count: usize,
    parameter_set_pointers: *const *const u8,
    parameter_set_sizes: *const usize,
    nal_unit_header_length: i32,
    format_description_out: *mut CMVideoFormatDescriptionRef,
) -> OSStatus;

pub type FnCMVideoFormatDescriptionCreateFromHEVCParameterSets = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    parameter_set_count: usize,
    parameter_set_pointers: *const *const u8,
    parameter_set_sizes: *const usize,
    nal_unit_header_length: i32,
    extensions: CFDictionaryRef,
    format_description_out: *mut CMVideoFormatDescriptionRef,
) -> OSStatus;

pub type FnCMVideoFormatDescriptionGetH264ParameterSetAtIndex = unsafe extern "C" fn(
    video_desc: CMVideoFormatDescriptionRef,
    parameter_set_index: usize,
    parameter_set_pointer_out: *mut *const u8,
    parameter_set_size_out: *mut usize,
    parameter_set_count_out: *mut usize,
    nal_unit_header_length_out: *mut i32,
) -> OSStatus;

pub type FnCMVideoFormatDescriptionGetHEVCParameterSetAtIndex = unsafe extern "C" fn(
    video_desc: CMVideoFormatDescriptionRef,
    parameter_set_index: usize,
    parameter_set_pointer_out: *mut *const u8,
    parameter_set_size_out: *mut usize,
    parameter_set_count_out: *mut usize,
    nal_unit_header_length_out: *mut i32,
) -> OSStatus;

pub type FnCMSampleBufferGetFormatDescription =
    unsafe extern "C" fn(sample_buffer: CMSampleBufferRef) -> CMFormatDescriptionRef;

// CoreVideo
pub type FnCVPixelBufferCreateWithBytes = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    width: usize,
    height: usize,
    pixel_format_type: u32,
    base_address: *mut c_void,
    bytes_per_row: usize,
    release_callback: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    release_ref_con: *mut c_void,
    pixel_buffer_attributes: CFDictionaryRef,
    pixel_buffer_out: *mut CVPixelBufferRef,
) -> CVReturn;

pub type FnCVPixelBufferCreateWithPlanarBytes = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    width: usize,
    height: usize,
    pixel_format_type: u32,
    data_ptr: *mut c_void,
    data_size: usize,
    number_of_planes: usize,
    plane_base_address: *mut *mut c_void,
    plane_width: *const usize,
    plane_height: *const usize,
    plane_bytes_per_row: *const usize,
    release_callback: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    release_ref_con: *mut c_void,
    pixel_buffer_attributes: CFDictionaryRef,
    pixel_buffer_out: *mut CVPixelBufferRef,
) -> CVReturn;

pub type FnCVPixelBufferLockBaseAddress =
    unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef, lock_flags: u64) -> CVReturn;

pub type FnCVPixelBufferUnlockBaseAddress =
    unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef, unlock_flags: u64) -> CVReturn;

pub type FnCVPixelBufferGetBaseAddressOfPlane =
    unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> *mut c_void;

pub type FnCVPixelBufferGetBytesPerRowOfPlane =
    unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;

pub type FnCVPixelBufferGetHeightOfPlane =
    unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef, plane_index: usize) -> usize;

pub type FnCVPixelBufferGetWidth = unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef) -> usize;

pub type FnCVPixelBufferGetHeight = unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef) -> usize;

pub type FnCVPixelBufferGetPixelFormatType =
    unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef) -> u32;

pub type FnCVPixelBufferGetPlaneCount =
    unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef) -> usize;

pub type FnCVPixelBufferIsPlanar = unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef) -> u8;

pub type FnCVPixelBufferGetBaseAddress =
    unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef) -> *mut c_void;

pub type FnCVPixelBufferGetBytesPerRow =
    unsafe extern "C" fn(pixel_buffer: CVPixelBufferRef) -> usize;

// CoreFoundation
pub type FnCFDictionaryCreate = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    keys: *const *const c_void,
    values: *const *const c_void,
    num_values: i64,
    key_callbacks: *const c_void,
    value_callbacks: *const c_void,
) -> CFDictionaryRef;

pub type FnCFNumberCreate = unsafe extern "C" fn(
    allocator: CFAllocatorRef,
    the_type: i32,
    value_ptr: *const c_void,
) -> CFNumberRef;

pub type FnCFStringCreateWithCString =
    unsafe extern "C" fn(alloc: CFAllocatorRef, c_str: *const u8, encoding: u32) -> CFStringRef;

/// `CFDataCreate(allocator, bytes, length)` — copies `bytes[0..length]`
/// into a fresh `CFData`. Used to wrap the ESDS / VOL configuration blob
/// for `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms`.
pub type FnCFDataCreate =
    unsafe extern "C" fn(alloc: CFAllocatorRef, bytes: *const u8, length: i64) -> CFDataRef;

/// `CFArrayCreate(allocator, values, numValues, callBacks)` — copies the
/// `values` array of `numValues` `CFTypeRef` pointers into a fresh
/// `CFArray`. Used by the `kVTCompressionPropertyKey_DataRateLimits`
/// path, which takes `CFArray[CFNumber]` of alternating bytes-and-seconds
/// per Apple's `VTCompressionProperties.h`. The `callBacks` argument
/// must be `kCFTypeArrayCallBacks` (the exported `CFArrayCallBacks`
/// singleton for `CFType` elements); we resolve that as a data symbol
/// at vtable-load time.
pub type FnCFArrayCreate = unsafe extern "C" fn(
    alloc: CFAllocatorRef,
    values: *const *const c_void,
    num_values: i64,
    callbacks: *const c_void,
) -> CFArrayRef;

pub type FnCFRelease = unsafe extern "C" fn(cf: CFTypeRef);
pub type FnCFRetain = unsafe extern "C" fn(cf: CFTypeRef) -> CFTypeRef;

// ─────────────────────────── Framework struct ─────────────────────────────────

/// Handles to the four frameworks the VT bridge needs.
///
/// All four must load successfully — VideoToolbox depends transitively
/// on the other three, but `dlopen` doesn't pull them automatically
/// for us.
pub struct Framework {
    pub video_toolbox: Library,
    pub core_video: Library,
    pub core_media: Library,
    pub core_foundation: Library,
}

/// Resolved function pointers for all VT/CV/CM/CF operations.
///
/// Extracted once after framework load and cached. All fields are
/// `unsafe extern "C" fn(...)` pointer types — callers are responsible
/// for correct argument types and SAFETY invariants.
pub struct Vtable {
    // VT decode
    pub vt_decomp_create: FnVTDecompressionSessionCreate,
    pub vt_decomp_decode: FnVTDecompressionSessionDecodeFrame,
    pub vt_decomp_finish: FnVTDecompressionSessionFinishDelayedFrames,
    pub vt_decomp_invalidate: FnVTDecompressionSessionInvalidate,
    // VT encode
    pub vt_comp_create: FnVTCompressionSessionCreate,
    pub vt_comp_encode: FnVTCompressionSessionEncodeFrame,
    pub vt_comp_complete: FnVTCompressionSessionCompleteFrames,
    pub vt_comp_invalidate: FnVTCompressionSessionInvalidate,
    pub vt_comp_prepare: FnVTCompressionSessionPrepareToEncodeFrames,
    pub vt_session_set_property: FnVTSessionSetProperty,
    // CM
    pub cm_sample_create_ready: FnCMSampleBufferCreateReady,
    pub cm_sample_get_data_buffer: FnCMSampleBufferGetDataBuffer,
    pub cm_block_copy_data: FnCMBlockBufferCopyDataBytes,
    pub cm_sample_get_image_buffer: FnCMSampleBufferGetImageBuffer,
    pub cm_block_create_with_mem: FnCMBlockBufferCreateWithMemoryBlock,
    pub cm_block_get_data_length: FnCMBlockBufferGetDataLength,
    pub cm_video_fmt_create: FnCMVideoFormatDescriptionCreate,
    pub cm_fmt_from_h264_params: FnCMVideoFormatDescriptionCreateFromH264ParameterSets,
    pub cm_fmt_from_hevc_params: FnCMVideoFormatDescriptionCreateFromHEVCParameterSets,
    pub cm_fmt_h264_param_at_idx: FnCMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    pub cm_fmt_hevc_param_at_idx: FnCMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    pub cm_sample_get_format_desc: FnCMSampleBufferGetFormatDescription,
    // CV
    pub cv_pb_create_planar: FnCVPixelBufferCreateWithPlanarBytes,
    pub cv_pb_lock: FnCVPixelBufferLockBaseAddress,
    pub cv_pb_unlock: FnCVPixelBufferUnlockBaseAddress,
    pub cv_pb_get_base_of_plane: FnCVPixelBufferGetBaseAddressOfPlane,
    pub cv_pb_get_bpr_of_plane: FnCVPixelBufferGetBytesPerRowOfPlane,
    pub cv_pb_get_height_of_plane: FnCVPixelBufferGetHeightOfPlane,
    pub cv_pb_get_width: FnCVPixelBufferGetWidth,
    pub cv_pb_get_height: FnCVPixelBufferGetHeight,
    pub cv_pb_get_pixel_format: FnCVPixelBufferGetPixelFormatType,
    pub cv_pb_get_plane_count: FnCVPixelBufferGetPlaneCount,
    pub cv_pb_is_planar: FnCVPixelBufferIsPlanar,
    pub cv_pb_get_base: FnCVPixelBufferGetBaseAddress,
    pub cv_pb_get_bpr: FnCVPixelBufferGetBytesPerRow,
    // CF
    pub cf_dict_create: FnCFDictionaryCreate,
    pub cf_number_create: FnCFNumberCreate,
    pub cf_string_create: FnCFStringCreateWithCString,
    pub cf_data_create: FnCFDataCreate,
    pub cf_array_create: FnCFArrayCreate,
    /// `kCFTypeArrayCallBacks` — Apple's exported `CFArrayCallBacks`
    /// singleton for `CFType`-pointer element arrays. Resolved as a data
    /// symbol from `CoreFoundation` and passed to `CFArrayCreate` so the
    /// freshly-created array retains and releases its `CFNumberRef`
    /// elements correctly. Used by the `DataRateLimits` write path.
    pub cf_type_array_callbacks: *const c_void,
    pub cf_release: FnCFRelease,
    pub cf_retain: FnCFRetain,
    // Keep libraries alive
    _vt: Library,
    _cv: Library,
    _cm: Library,
    _cf: Library,
}

// SAFETY: `cf_type_array_callbacks` is a stable read-only data symbol
// exported by CoreFoundation (`kCFTypeArrayCallBacks`), valid for the
// process lifetime; no thread-affinity. The function pointers in the
// vtable are already `extern "C" fn` and therefore Send + Sync.
unsafe impl Send for Vtable {}
unsafe impl Sync for Vtable {}

/// Process-wide cache. `OnceLock` so concurrent first calls collapse
/// to a single load. Stored as `Result` so the dlopen failure surface
/// is visible to callers without a re-load attempt.
static VTABLE: OnceLock<Result<Vtable, String>> = OnceLock::new();

/// Get (or load) the fully-resolved vtable. Returns the cached `Err` if a
/// previous load attempt failed.
pub fn vtable() -> Result<&'static Vtable, &'static str> {
    VTABLE
        .get_or_init(load_vtable)
        .as_ref()
        .map_err(|s| s.as_str())
}

/// Legacy `framework()` helper — kept for the smoke test in the `sys` module.
/// Returns a thin wrapper that just confirms the libraries load.
static FRAMEWORK: OnceLock<Result<FrameworkSmoke, String>> = OnceLock::new();

pub struct FrameworkSmoke {
    pub video_toolbox: Library,
    pub core_video: Library,
    pub core_media: Library,
    pub core_foundation: Library,
}

pub fn framework() -> Result<&'static FrameworkSmoke, &'static str> {
    FRAMEWORK
        .get_or_init(load_smoke)
        .as_ref()
        .map_err(|s| s.as_str())
}

fn load_smoke() -> Result<FrameworkSmoke, String> {
    Ok(FrameworkSmoke {
        video_toolbox: open("/System/Library/Frameworks/VideoToolbox.framework/VideoToolbox")?,
        core_video: open("/System/Library/Frameworks/CoreVideo.framework/CoreVideo")?,
        core_media: open("/System/Library/Frameworks/CoreMedia.framework/CoreMedia")?,
        core_foundation: open(
            "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation",
        )?,
    })
}

fn load_vtable() -> Result<Vtable, String> {
    let vt = open("/System/Library/Frameworks/VideoToolbox.framework/VideoToolbox")?;
    let cv = open("/System/Library/Frameworks/CoreVideo.framework/CoreVideo")?;
    let cm = open("/System/Library/Frameworks/CoreMedia.framework/CoreMedia")?;
    let cf = open("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")?;

    macro_rules! sym {
        ($lib:expr, $name:expr, $ty:ty) => {{
            let s: libloading::Symbol<$ty> = unsafe {
                $lib.get(concat!($name, "\0").as_bytes())
                    .map_err(|e| format!("dlsym {}: {}", $name, e))?
            };
            *s
        }};
    }

    Ok(Vtable {
        vt_decomp_create: sym!(
            vt,
            "VTDecompressionSessionCreate",
            FnVTDecompressionSessionCreate
        ),
        vt_decomp_decode: sym!(
            vt,
            "VTDecompressionSessionDecodeFrame",
            FnVTDecompressionSessionDecodeFrame
        ),
        vt_decomp_finish: sym!(
            vt,
            "VTDecompressionSessionFinishDelayedFrames",
            FnVTDecompressionSessionFinishDelayedFrames
        ),
        vt_decomp_invalidate: sym!(
            vt,
            "VTDecompressionSessionInvalidate",
            FnVTDecompressionSessionInvalidate
        ),
        vt_comp_create: sym!(
            vt,
            "VTCompressionSessionCreate",
            FnVTCompressionSessionCreate
        ),
        vt_comp_encode: sym!(
            vt,
            "VTCompressionSessionEncodeFrame",
            FnVTCompressionSessionEncodeFrame
        ),
        vt_comp_complete: sym!(
            vt,
            "VTCompressionSessionCompleteFrames",
            FnVTCompressionSessionCompleteFrames
        ),
        vt_comp_invalidate: sym!(
            vt,
            "VTCompressionSessionInvalidate",
            FnVTCompressionSessionInvalidate
        ),
        vt_comp_prepare: sym!(
            vt,
            "VTCompressionSessionPrepareToEncodeFrames",
            FnVTCompressionSessionPrepareToEncodeFrames
        ),
        vt_session_set_property: sym!(vt, "VTSessionSetProperty", FnVTSessionSetProperty),
        cm_sample_create_ready: sym!(cm, "CMSampleBufferCreateReady", FnCMSampleBufferCreateReady),
        cm_sample_get_data_buffer: sym!(
            cm,
            "CMSampleBufferGetDataBuffer",
            FnCMSampleBufferGetDataBuffer
        ),
        cm_block_copy_data: sym!(
            cm,
            "CMBlockBufferCopyDataBytes",
            FnCMBlockBufferCopyDataBytes
        ),
        cm_sample_get_image_buffer: sym!(
            cm,
            "CMSampleBufferGetImageBuffer",
            FnCMSampleBufferGetImageBuffer
        ),
        cm_block_create_with_mem: sym!(
            cm,
            "CMBlockBufferCreateWithMemoryBlock",
            FnCMBlockBufferCreateWithMemoryBlock
        ),
        cm_block_get_data_length: sym!(
            cm,
            "CMBlockBufferGetDataLength",
            FnCMBlockBufferGetDataLength
        ),
        cm_video_fmt_create: sym!(
            cm,
            "CMVideoFormatDescriptionCreate",
            FnCMVideoFormatDescriptionCreate
        ),
        cm_fmt_from_h264_params: sym!(
            cm,
            "CMVideoFormatDescriptionCreateFromH264ParameterSets",
            FnCMVideoFormatDescriptionCreateFromH264ParameterSets
        ),
        cm_fmt_from_hevc_params: sym!(
            cm,
            "CMVideoFormatDescriptionCreateFromHEVCParameterSets",
            FnCMVideoFormatDescriptionCreateFromHEVCParameterSets
        ),
        cm_fmt_h264_param_at_idx: sym!(
            cm,
            "CMVideoFormatDescriptionGetH264ParameterSetAtIndex",
            FnCMVideoFormatDescriptionGetH264ParameterSetAtIndex
        ),
        cm_fmt_hevc_param_at_idx: sym!(
            cm,
            "CMVideoFormatDescriptionGetHEVCParameterSetAtIndex",
            FnCMVideoFormatDescriptionGetHEVCParameterSetAtIndex
        ),
        cm_sample_get_format_desc: sym!(
            cm,
            "CMSampleBufferGetFormatDescription",
            FnCMSampleBufferGetFormatDescription
        ),
        cv_pb_create_planar: sym!(
            cv,
            "CVPixelBufferCreateWithPlanarBytes",
            FnCVPixelBufferCreateWithPlanarBytes
        ),
        cv_pb_lock: sym!(
            cv,
            "CVPixelBufferLockBaseAddress",
            FnCVPixelBufferLockBaseAddress
        ),
        cv_pb_unlock: sym!(
            cv,
            "CVPixelBufferUnlockBaseAddress",
            FnCVPixelBufferUnlockBaseAddress
        ),
        cv_pb_get_base_of_plane: sym!(
            cv,
            "CVPixelBufferGetBaseAddressOfPlane",
            FnCVPixelBufferGetBaseAddressOfPlane
        ),
        cv_pb_get_bpr_of_plane: sym!(
            cv,
            "CVPixelBufferGetBytesPerRowOfPlane",
            FnCVPixelBufferGetBytesPerRowOfPlane
        ),
        cv_pb_get_height_of_plane: sym!(
            cv,
            "CVPixelBufferGetHeightOfPlane",
            FnCVPixelBufferGetHeightOfPlane
        ),
        cv_pb_get_width: sym!(cv, "CVPixelBufferGetWidth", FnCVPixelBufferGetWidth),
        cv_pb_get_height: sym!(cv, "CVPixelBufferGetHeight", FnCVPixelBufferGetHeight),
        cv_pb_get_pixel_format: sym!(
            cv,
            "CVPixelBufferGetPixelFormatType",
            FnCVPixelBufferGetPixelFormatType
        ),
        cv_pb_get_plane_count: sym!(
            cv,
            "CVPixelBufferGetPlaneCount",
            FnCVPixelBufferGetPlaneCount
        ),
        cv_pb_is_planar: sym!(cv, "CVPixelBufferIsPlanar", FnCVPixelBufferIsPlanar),
        cv_pb_get_base: sym!(
            cv,
            "CVPixelBufferGetBaseAddress",
            FnCVPixelBufferGetBaseAddress
        ),
        cv_pb_get_bpr: sym!(
            cv,
            "CVPixelBufferGetBytesPerRow",
            FnCVPixelBufferGetBytesPerRow
        ),
        cf_dict_create: sym!(cf, "CFDictionaryCreate", FnCFDictionaryCreate),
        cf_number_create: sym!(cf, "CFNumberCreate", FnCFNumberCreate),
        cf_string_create: sym!(cf, "CFStringCreateWithCString", FnCFStringCreateWithCString),
        cf_data_create: sym!(cf, "CFDataCreate", FnCFDataCreate),
        cf_array_create: sym!(cf, "CFArrayCreate", FnCFArrayCreate),
        // `kCFTypeArrayCallBacks` is a *data* symbol (a static
        // `CFArrayCallBacks` struct), not a function. We resolve it as a
        // raw pointer and pass the *pointer* (not the value) to
        // `CFArrayCreate` so the freshly-created array retains/releases
        // its `CFType` elements correctly.
        cf_type_array_callbacks: {
            let s: libloading::Symbol<*const c_void> = unsafe {
                cf.get(b"kCFTypeArrayCallBacks\0")
                    .map_err(|e| format!("dlsym kCFTypeArrayCallBacks: {e}"))?
            };
            // `Symbol<*const c_void>` derefs to the address the linker
            // stored — i.e. the address of the `CFArrayCallBacks` struct.
            *s
        },
        cf_release: sym!(cf, "CFRelease", FnCFRelease),
        cf_retain: sym!(cf, "CFRetain", FnCFRetain),
        _vt: vt,
        _cv: cv,
        _cm: cm,
        _cf: cf,
    })
}

#[cfg(target_os = "macos")]
fn open(path: &str) -> Result<Library, String> {
    // SAFETY: dlopen on a fixed system framework path with no init
    // callbacks; equivalent to a normal program startup load.
    unsafe { Library::new(path) }.map_err(|e| format!("dlopen {path}: {e}"))
}

#[cfg(target_os = "ios")]
fn open(_path: &str) -> Result<Library, String> {
    // On iOS the four system frameworks are link-loaded at process
    // start via build.rs `cargo:rustc-link-lib=framework=...`
    // directives. `os::unix::Library::this()` returns a handle equivalent
    // to `dlopen(NULL, ...)` — the host process's `RTLD_DEFAULT` —
    // and subsequent `Symbol::get(b"VT...")` calls resolve every
    // VT / CV / CM / CF symbol the vtable needs from the dyld shared
    // cache. The `_path` argument is unused on iOS (sandboxed apps
    // cannot reliably `dlopen` `/System/...` paths), but the four call
    // sites keep their string arguments for parity with the macOS branch.
    //
    // `os::unix::Library::this()` is safe to call — it maps the
    // already-loaded process image and is documented as the
    // `dlopen(NULL, ...)` equivalent. Resolving link-loaded framework
    // symbols from that handle is the iOS-conventional pattern. The
    // Library is materialised once on the first vtable load and
    // cached for the lifetime of the process.
    let lib_unix = libloading::os::unix::Library::this();
    Ok(Library::from(lib_unix))
}

// ─────────────────────────── CF helpers ───────────────────────────────────────

/// Create a CFString from a static Rust string via the vtable.
///
/// # Safety
/// Caller must call `cf_release` on the returned value when done.
pub unsafe fn cf_string(vt: &Vtable, s: &str) -> CFStringRef {
    let cstr = std::ffi::CString::new(s).unwrap();
    unsafe {
        (vt.cf_string_create)(
            std::ptr::null_mut(),
            cstr.as_ptr() as *const u8,
            K_CF_STRING_ENCODING_UTF8,
        )
    }
}

/// Create a CFNumber (i32) via the vtable.
///
/// # Safety
/// Caller must call `cf_release` on the returned value when done.
pub unsafe fn cf_number_i32(vt: &Vtable, v: i32) -> CFNumberRef {
    unsafe {
        (vt.cf_number_create)(
            std::ptr::null_mut(),
            K_CF_NUMBER_SI_NT32_TYPE,
            &v as *const i32 as *const _,
        )
    }
}

/// Create a CFNumber (Float32) via the vtable. Used to wrap the
/// `kVTCompressionPropertyKey_Quality` CFNumber, documented by Apple
/// as a Float in `[0.0, 1.0]` where higher values request higher
/// quality.
///
/// # Safety
/// Caller must call `cf_release` on the returned value when done.
pub unsafe fn cf_number_f32(vt: &Vtable, v: f32) -> CFNumberRef {
    unsafe {
        (vt.cf_number_create)(
            std::ptr::null_mut(),
            K_CF_NUMBER_FLOAT_32_TYPE,
            &v as *const f32 as *const _,
        )
    }
}

/// Create a CFNumber (Float64) via the vtable. Used to wrap the
/// `kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration` value
/// (CFNumber\<seconds\>) and the `kVTCompressionPropertyKey_ExpectedFrameRate`
/// value (CFNumber, frames-per-second). Apple's `CFNumber.h` documents
/// `kCFNumberFloat64Type = 6` as a 64-bit IEEE-754 double; CoreFoundation
/// copies the value at the pointer at call time so the caller's `v` does
/// not need to outlive the call.
///
/// # Safety
/// Caller must call `cf_release` on the returned value when done.
pub unsafe fn cf_number_f64(vt: &Vtable, v: f64) -> CFNumberRef {
    unsafe {
        (vt.cf_number_create)(
            std::ptr::null_mut(),
            K_CF_NUMBER_FLOAT_64_TYPE,
            &v as *const f64 as *const _,
        )
    }
}

/// Create a `CFData` containing a copy of `bytes`.
///
/// Used to wrap the MPEG-4 Part 2 ESDS configuration blob (or any other
/// format-description-extension atom payload) into the form
/// `CMVideoFormatDescriptionCreate` accepts under its `extensions`
/// dictionary.
///
/// # Safety
/// Caller must call `cf_release` on the returned value when done. `bytes`
/// must be a valid byte slice; CoreFoundation copies the buffer so the
/// slice need not outlive the returned `CFData`.
pub unsafe fn cf_data(vt: &Vtable, bytes: &[u8]) -> CFDataRef {
    unsafe { (vt.cf_data_create)(std::ptr::null_mut(), bytes.as_ptr(), bytes.len() as i64) }
}

/// Create a `CFArray` of `CFType` elements (typically `CFNumber`).
///
/// Used by the `kVTCompressionPropertyKey_DataRateLimits` write path,
/// which per Apple's `VTCompressionProperties.h` takes a
/// `CFArray[CFNumber]` of alternating bytes-and-seconds entries
/// (`[bytes, seconds, bytes, seconds, ...]`).
///
/// The new array retains each element via `kCFTypeArrayCallBacks`, so
/// the caller must `cf_release` every element pointer it created
/// independently of the array itself (the array's retain pairs with the
/// release the array performs on its own destruction).
///
/// # Safety
/// `elements` must be a valid slice of `CFTypeRef`-compatible pointers
/// (each non-NULL and owned by the caller). Caller must call
/// `cf_release` on the returned array when done.
pub unsafe fn cf_array(vt: &Vtable, elements: &[CFTypeRef]) -> CFArrayRef {
    let values: Vec<*const c_void> = elements.iter().map(|&p| p as *const c_void).collect();
    unsafe {
        (vt.cf_array_create)(
            std::ptr::null_mut(),
            values.as_ptr(),
            elements.len() as i64,
            vt.cf_type_array_callbacks,
        )
    }
}

/// Create an empty CFDictionary (no entries) via the vtable.
///
/// # Safety
/// Caller must call `cf_release` on the returned value when done.
pub unsafe fn cf_empty_dict(vt: &Vtable) -> CFDictionaryRef {
    // kCFTypeDictionaryKeyCallBacks and kCFTypeDictionaryValueCallBacks
    // live in CoreFoundation as exported data symbols. We use NULL which
    // CoreFoundation accepts for an empty (0-entry) dictionary.
    unsafe {
        (vt.cf_dict_create)(
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: every framework on this Mac loads cleanly.
    #[test]
    fn frameworks_load() {
        let fw = framework().expect("framework load");
        // Verify a stable VT entry point resolves so we know the
        // dyld cache served the right binary.
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.video_toolbox
                .get(b"VTDecompressionSessionCreate\0")
                .expect("VTDecompressionSessionCreate symbol")
        };
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.core_foundation
                .get(b"CFRetain\0")
                .expect("CFRetain symbol")
        };
    }

    /// Verify the full vtable resolves all symbols.
    #[test]
    fn vtable_resolves() {
        vtable().expect("vtable load");
    }

    /// `CMTime::make` produces a valid time; `CMTime::invalid()` is the
    /// all-zero `kCMTimeInvalid` struct (flags bit 0 clear). Per
    /// CoreMedia's `CMTime.h`, `kCMTimeFlags_Valid` "must be set, or the
    /// CMTime is considered invalid" — a sentinel *value* (e.g.
    /// `i64::MIN`) with the Valid flag set is NOT an invalid time.
    #[test]
    fn cmtime_validity_flags() {
        let t = CMTime::make(42, 1_000_000);
        assert!(t.is_valid());
        assert_eq!(t.value, 42);
        assert_eq!(t.timescale, 1_000_000);
        assert_eq!(t.flags, K_CM_TIME_FLAGS_VALID);
        assert_eq!(t.epoch, 0);

        let inv = CMTime::invalid();
        assert!(!inv.is_valid());
        assert_eq!(inv.value, 0);
        assert_eq!(inv.timescale, 0);
        assert_eq!(inv.flags, 0);
        assert_eq!(inv.epoch, 0);

        assert!(CMTime::zero().is_valid());
    }

    /// The default sample-timing template carries an *invalid* decode
    /// timestamp (unknown DTS), not a "valid" `i64::MIN` sentinel.
    #[test]
    fn sample_timing_dts_is_invalid_time() {
        let timing = CMSampleTimingInfo::zero();
        assert!(timing.duration.is_valid());
        assert!(timing.presentation_time_stamp.is_valid());
        assert!(!timing.decode_time_stamp.is_valid());
    }
}
