//! VT decoder/encoder for "blob" codecs (one frame = one self-contained payload).
//!
//! H.264 and HEVC stream parameter sets out-of-band and decode frames built
//! from one or more NAL units. JPEG and ProRes are simpler: each compressed
//! frame is a self-contained byte blob, and the format description is built
//! from `CMVideoFormatDescriptionCreate(width, height, codecType)` — no
//! parameter-set extraction is involved.
//!
//! This module factors out the common decode + encode pipeline behind a
//! generic codec-type tag so JPEG (`'jpeg'`), the six ProRes fourccs
//! (`apco / apcs / apcn / apch / ap4h / ap4x`), and MPEG-2 video (`'mp2v'`,
//! decode-only) share a single `VTDecompressionSession` /
//! `VTCompressionSession` driver.
//!
//! By default the decoder accepts whole-frame `Packet`s; the encoder
//! produces whole-frame `Packet`s. Annex-B start-code handling that
//! H.264/HEVC need is absent here — frames are byte-for-byte what VT
//! consumed/emitted. MPEG-2 is the exception: its input is an *elementary*
//! stream, so the decoder uses a `FrameSplit::Mpeg2Es` framer to carve
//! per-picture access units before submission.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Decoder, Encoder, Error, Frame, Packet, PixelFormat, Result,
    TimeBase, VideoFrame, VideoPlane,
};

use crate::sys::{
    self, cf_number_i32, cf_string, CMSampleTimingInfo, CMTime,
    K_CV_PIXEL_FORMAT_420_YPCBCRi8_BI_PLANAR_VIDEO_RANGE, K_CV_PIXEL_BUFFER_LOCK_FLAGS_READ_ONLY,
    K_OS_STATUS_NO_ERROR,
};

// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange = '420v'
const K_CV_PIXEL_FORMAT_NV12: u32 = 0x34323076;

// ─────────────────────────── libc shim ────────────────────────────────────────

unsafe fn libc_malloc(size: usize) -> *mut c_void {
    extern "C" {
        fn malloc(size: usize) -> *mut c_void;
    }
    unsafe { malloc(size) }
}

// ─────────────────────────── Callback state (decode) ─────────────────────────

struct DecCallbackState {
    frames: VecDeque<VideoFrame>,
    error: Option<String>,
}

impl DecCallbackState {
    fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            frames: VecDeque::new(),
            error: None,
        }))
    }
}

unsafe extern "C" fn dec_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: u32,
    image_buffer: sys::CVImageBufferRef,
) {
    let state_ptr = output_callback_ref_con as *const Mutex<DecCallbackState>;
    let state = unsafe { &*state_ptr };
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    if status != K_OS_STATUS_NO_ERROR {
        guard.error = Some(format!("VT blob-decode callback OSStatus {status}"));
        return;
    }
    if image_buffer.is_null() {
        return;
    }

    let vt = match sys::vtable() {
        Ok(v) => v,
        Err(e) => {
            guard.error = Some(format!("vtable in blob callback: {e}"));
            return;
        }
    };

    let ret = unsafe { (vt.cv_pb_lock)(image_buffer, K_CV_PIXEL_BUFFER_LOCK_FLAGS_READ_ONLY) };
    if ret != 0 {
        guard.error = Some(format!("CVPixelBufferLockBaseAddress: {ret}"));
        return;
    }

    let width = unsafe { (vt.cv_pb_get_width)(image_buffer) };
    let height = unsafe { (vt.cv_pb_get_height)(image_buffer) };
    let pixel_fmt = unsafe { (vt.cv_pb_get_pixel_format)(image_buffer) };

    let frame = decode_pixel_buffer(vt, image_buffer, width, height, pixel_fmt);

    unsafe { (vt.cv_pb_unlock)(image_buffer, 0) };

    match frame {
        Ok(f) => guard.frames.push_back(f),
        Err(e) => guard.error = Some(e),
    }
}

/// Convert a `CVPixelBuffer` (in one of several supported pixel formats)
/// into a planar I420 `VideoFrame`. Handles 8-bit biplanar NV12 (`'420v'`,
/// `'420f'`), 8-bit packed 4:2:2 (`'2vuy'` UYVY, `'yuvs'` YUY2), and 16-bit
/// 4:2:2 (`'sv22'` biplanar, `'v216'` packed). Returns an error string for
/// unsupported formats; the caller logs it.
fn decode_pixel_buffer(
    vt: &sys::Vtable,
    image_buffer: sys::CVImageBufferRef,
    width: usize,
    height: usize,
    pixel_fmt: u32,
) -> std::result::Result<VideoFrame, String> {
    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);

    match pixel_fmt {
        // '420v' (kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange) or
        // '420f' (kCVPixelFormatType_420YpCbCr8BiPlanarFullRange):
        // biplanar Y + interleaved UV, 8 bit.
        0x34323076 | 0x34323066 => Ok(decode_nv12(
            vt,
            image_buffer,
            width,
            height,
            chroma_w,
            chroma_h,
        )),
        // '2vuy' (kCVPixelFormatType_422YpCbCr8) packed UYVY 4:2:2.
        0x32767579 => Ok(decode_uyvy_to_i420(
            vt,
            image_buffer,
            width,
            height,
            chroma_w,
            chroma_h,
        )),
        // 'yuvs' (kCVPixelFormatType_422YpCbCr8_yuvs) packed YUY2 4:2:2.
        0x79757673 => Ok(decode_yuy2_to_i420(
            vt,
            image_buffer,
            width,
            height,
            chroma_w,
            chroma_h,
        )),
        // 'sv22' (kCVPixelFormatType_422YpCbCr16BiPlanarVideoRange):
        // biplanar 4:2:2 with 16-bit container per sample (10-12 bit
        // value left-shifted; high byte = 8-bit video-range proxy).
        // ProRes 422 decodes to this by default on Apple Silicon.
        0x73763232 => Ok(decode_biplanar_16bit_422_to_i420(
            vt,
            image_buffer,
            width,
            height,
            chroma_w,
            chroma_h,
        )),
        // 'v216' (kCVPixelFormatType_422YpCbCr16): packed 4:2:2 with
        // each component as little-endian 16-bit. Sample order per
        // 2-pixel block: Cb0 Y0 Cr0 Y1 (8 bytes). ProRes 422 decodes to
        // this on the Apple-hosted macos-latest x86_64 runner.
        0x76323136 => Ok(decode_v216_to_i420(
            vt,
            image_buffer,
            width,
            height,
            chroma_w,
            chroma_h,
        )),
        other => Err(format!(
            "unsupported CVPixelBuffer format 0x{other:08x} (decoded {width}x{height})"
        )),
    }
}

/// Convert packed `'v216'` (Component Y'CbCr 16-bit 4:2:2, packed
/// `[Cb0 Y0 Cr0 Y1]` little-endian per 2-pixel block) into planar I420.
/// Container holds a 10..16-bit value left-justified; we take the high
/// byte as an 8-bit video-range proxy.
fn decode_v216_to_i420(
    vt: &sys::Vtable,
    image_buffer: sys::CVImageBufferRef,
    width: usize,
    height: usize,
    chroma_w: usize,
    chroma_h: usize,
) -> VideoFrame {
    let base = unsafe { (vt.cv_pb_get_base)(image_buffer) } as *const u8;
    let bpr = unsafe { (vt.cv_pb_get_bpr)(image_buffer) };

    let mut y_data = vec![0u8; width * height];
    let mut cb_422 = vec![0u16; chroma_w * height];
    let mut cr_422 = vec![0u16; chroma_w * height];

    // 8 bytes per 2-pixel block (4 components × 2 bytes).
    if !base.is_null() {
        for row in 0..height {
            let row_ptr = unsafe { base.add(row * bpr) };
            // Bytes per row should be ≥ width * 4, but defensively clamp
            // in case of stride padding.
            let blocks = (bpr / 8).min(chroma_w);
            for cx in 0..blocks {
                let off = cx * 8;
                let cb_lo = unsafe { *row_ptr.add(off) };
                let cb_hi = unsafe { *row_ptr.add(off + 1) };
                let y0_lo = unsafe { *row_ptr.add(off + 2) };
                let y0_hi = unsafe { *row_ptr.add(off + 3) };
                let cr_lo = unsafe { *row_ptr.add(off + 4) };
                let cr_hi = unsafe { *row_ptr.add(off + 5) };
                let y1_lo = unsafe { *row_ptr.add(off + 6) };
                let y1_hi = unsafe { *row_ptr.add(off + 7) };
                let cb = ((cb_hi as u16) << 8 | cb_lo as u16) >> 8;
                let cr = ((cr_hi as u16) << 8 | cr_lo as u16) >> 8;
                let y0 = ((y0_hi as u16) << 8 | y0_lo as u16) >> 8;
                let y1 = ((y1_hi as u16) << 8 | y1_lo as u16) >> 8;
                let px = cx * 2;
                if px < width {
                    y_data[row * width + px] = y0 as u8;
                }
                if px + 1 < width {
                    y_data[row * width + px + 1] = y1 as u8;
                }
                cb_422[row * chroma_w + cx] = cb;
                cr_422[row * chroma_w + cx] = cr;
            }
        }
    }

    let mut u_data = vec![0u8; chroma_w * chroma_h];
    let mut v_data = vec![0u8; chroma_w * chroma_h];
    for cy in 0..chroma_h {
        let r0 = (cy * 2).min(height.saturating_sub(1));
        let r1 = (cy * 2 + 1).min(height.saturating_sub(1));
        for cx in 0..chroma_w {
            let u = (cb_422[r0 * chroma_w + cx] + cb_422[r1 * chroma_w + cx]).div_ceil(2);
            let v = (cr_422[r0 * chroma_w + cx] + cr_422[r1 * chroma_w + cx]).div_ceil(2);
            u_data[cy * chroma_w + cx] = u as u8;
            v_data[cy * chroma_w + cx] = v as u8;
        }
    }

    VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: width,
                data: y_data,
            },
            VideoPlane {
                stride: chroma_w,
                data: u_data,
            },
            VideoPlane {
                stride: chroma_w,
                data: v_data,
            },
        ],
    }
}

/// Convert biplanar 16-bit 4:2:2 (`'sv22'`, ProRes-default on Apple
/// Silicon) into planar I420 8-bit. The 16-bit container holds a
/// `[4096, 60160]` video-range value for luma, so the high byte is the
/// 8-bit video-range proxy. Chroma is vertically averaged 2:1 to land
/// in I420.
fn decode_biplanar_16bit_422_to_i420(
    vt: &sys::Vtable,
    image_buffer: sys::CVImageBufferRef,
    width: usize,
    height: usize,
    chroma_w: usize,
    chroma_h: usize,
) -> VideoFrame {
    let y_ptr = unsafe { (vt.cv_pb_get_base_of_plane)(image_buffer, 0) } as *const u8;
    let y_stride = unsafe { (vt.cv_pb_get_bpr_of_plane)(image_buffer, 0) };
    let y_height = unsafe { (vt.cv_pb_get_height_of_plane)(image_buffer, 0) };
    let cbcr_ptr = unsafe { (vt.cv_pb_get_base_of_plane)(image_buffer, 1) } as *const u8;
    let cbcr_stride = unsafe { (vt.cv_pb_get_bpr_of_plane)(image_buffer, 1) };
    let cbcr_height = unsafe { (vt.cv_pb_get_height_of_plane)(image_buffer, 1) };

    let mut y_data = vec![0u8; width * height];
    // Accumulate chroma at 4:2:2 (chroma_w × height) first, then average
    // row pairs down to 4:2:0.
    let mut cb_422 = vec![0u16; chroma_w * height];
    let mut cr_422 = vec![0u16; chroma_w * height];

    // Y plane: each sample is little-endian u16, take the high byte.
    if !y_ptr.is_null() {
        for row in 0..y_height.min(height) {
            let row_ptr = unsafe { y_ptr.add(row * y_stride) };
            let max_pix = (y_stride / 2).min(width);
            for col in 0..max_pix {
                let lo = unsafe { *row_ptr.add(col * 2) };
                let hi = unsafe { *row_ptr.add(col * 2 + 1) };
                // little-endian 16-bit; high byte already approximates the
                // 8-bit video-range value.
                let sample = (hi as u16) << 8 | lo as u16;
                y_data[row * width + col] = (sample >> 8) as u8;
            }
        }
    }

    // CbCr plane: interleaved Cb,Cr pairs, each 16-bit.
    // chroma_h here equals `height` for 4:2:2.
    if !cbcr_ptr.is_null() {
        let rows = cbcr_height.min(height);
        for row in 0..rows {
            let row_ptr = unsafe { cbcr_ptr.add(row * cbcr_stride) };
            let pairs = (cbcr_stride / 4).min(chroma_w);
            for cx in 0..pairs {
                let cb_lo = unsafe { *row_ptr.add(cx * 4) };
                let cb_hi = unsafe { *row_ptr.add(cx * 4 + 1) };
                let cr_lo = unsafe { *row_ptr.add(cx * 4 + 2) };
                let cr_hi = unsafe { *row_ptr.add(cx * 4 + 3) };
                let cb = ((cb_hi as u16) << 8 | cb_lo as u16) >> 8;
                let cr = ((cr_hi as u16) << 8 | cr_lo as u16) >> 8;
                cb_422[row * chroma_w + cx] = cb;
                cr_422[row * chroma_w + cx] = cr;
            }
        }
    }

    // 4:2:2 → 4:2:0: average chroma rows pairwise.
    let mut u_data = vec![0u8; chroma_w * chroma_h];
    let mut v_data = vec![0u8; chroma_w * chroma_h];
    for cy in 0..chroma_h {
        let r0 = (cy * 2).min(height.saturating_sub(1));
        let r1 = (cy * 2 + 1).min(height.saturating_sub(1));
        for cx in 0..chroma_w {
            let u = (cb_422[r0 * chroma_w + cx] + cb_422[r1 * chroma_w + cx]).div_ceil(2);
            let v = (cr_422[r0 * chroma_w + cx] + cr_422[r1 * chroma_w + cx]).div_ceil(2);
            u_data[cy * chroma_w + cx] = u as u8;
            v_data[cy * chroma_w + cx] = v as u8;
        }
    }

    VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: width,
                data: y_data,
            },
            VideoPlane {
                stride: chroma_w,
                data: u_data,
            },
            VideoPlane {
                stride: chroma_w,
                data: v_data,
            },
        ],
    }
}

fn decode_nv12(
    vt: &sys::Vtable,
    image_buffer: sys::CVImageBufferRef,
    width: usize,
    height: usize,
    chroma_w: usize,
    chroma_h: usize,
) -> VideoFrame {
    let y_ptr = unsafe { (vt.cv_pb_get_base_of_plane)(image_buffer, 0) } as *const u8;
    let y_stride = unsafe { (vt.cv_pb_get_bpr_of_plane)(image_buffer, 0) };
    let y_height = unsafe { (vt.cv_pb_get_height_of_plane)(image_buffer, 0) };
    let uv_ptr = unsafe { (vt.cv_pb_get_base_of_plane)(image_buffer, 1) } as *const u8;
    let uv_stride = unsafe { (vt.cv_pb_get_bpr_of_plane)(image_buffer, 1) };
    let uv_height = unsafe { (vt.cv_pb_get_height_of_plane)(image_buffer, 1) };

    let mut y_data = vec![0u8; width * height];
    let mut u_data = vec![0u8; chroma_w * chroma_h];
    let mut v_data = vec![0u8; chroma_w * chroma_h];

    if !y_ptr.is_null() {
        for row in 0..y_height.min(height) {
            let row_len = width.min(y_stride);
            let src = unsafe { std::slice::from_raw_parts(y_ptr.add(row * y_stride), row_len) };
            let dst = row * width;
            y_data[dst..dst + row_len].copy_from_slice(src);
        }
    }
    if !uv_ptr.is_null() {
        for row in 0..uv_height.min(chroma_h) {
            let row_len = (chroma_w * 2).min(uv_stride);
            let src = unsafe { std::slice::from_raw_parts(uv_ptr.add(row * uv_stride), row_len) };
            let dst = row * chroma_w;
            for col in 0..chroma_w {
                u_data[dst + col] = if col * 2 < row_len { src[col * 2] } else { 128 };
                v_data[dst + col] = if col * 2 + 1 < row_len {
                    src[col * 2 + 1]
                } else {
                    128
                };
            }
        }
    }

    VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: width,
                data: y_data,
            },
            VideoPlane {
                stride: chroma_w,
                data: u_data,
            },
            VideoPlane {
                stride: chroma_w,
                data: v_data,
            },
        ],
    }
}

/// Convert packed UYVY (`'2vuy'`, 4:2:2 8-bit, sample order U Y V Y per
/// 2 horizontal pixels) into planar I420 (4:2:0). Vertical chroma is
/// 2:1 subsampled by averaging row pairs.
fn decode_uyvy_to_i420(
    vt: &sys::Vtable,
    image_buffer: sys::CVImageBufferRef,
    width: usize,
    height: usize,
    chroma_w: usize,
    chroma_h: usize,
) -> VideoFrame {
    decode_packed_422_to_i420(vt, image_buffer, width, height, chroma_w, chroma_h, true)
}

/// Convert packed YUY2 (`'yuvs'`, sample order Y U Y V).
fn decode_yuy2_to_i420(
    vt: &sys::Vtable,
    image_buffer: sys::CVImageBufferRef,
    width: usize,
    height: usize,
    chroma_w: usize,
    chroma_h: usize,
) -> VideoFrame {
    decode_packed_422_to_i420(vt, image_buffer, width, height, chroma_w, chroma_h, false)
}

fn decode_packed_422_to_i420(
    vt: &sys::Vtable,
    image_buffer: sys::CVImageBufferRef,
    width: usize,
    height: usize,
    chroma_w: usize,
    chroma_h: usize,
    is_uyvy: bool,
) -> VideoFrame {
    // Packed 4:2:2 buffers are non-planar — CVPixelBufferGetBaseAddress
    // gives the single-plane pointer.
    let base = unsafe { (vt.cv_pb_get_base)(image_buffer) } as *const u8;
    let bpr = unsafe { (vt.cv_pb_get_bpr)(image_buffer) };

    let mut y_data = vec![0u8; width * height];
    // Sum chroma into 4:2:2 then average vertically to 4:2:0.
    let mut u_422 = vec![0u16; chroma_w * height];
    let mut v_422 = vec![0u16; chroma_w * height];

    if !base.is_null() {
        for row in 0..height {
            let row_ptr = unsafe { base.add(row * bpr) };
            let row_bytes = bpr.min(width * 2);
            let src = unsafe { std::slice::from_raw_parts(row_ptr, row_bytes) };
            let mut x = 0usize;
            while x + 4 <= row_bytes && x / 2 < width {
                let (u, y0, v, y1) = if is_uyvy {
                    (src[x], src[x + 1], src[x + 2], src[x + 3])
                } else {
                    (src[x + 1], src[x], src[x + 3], src[x + 2])
                };
                let px = x / 2;
                if px < width {
                    y_data[row * width + px] = y0;
                }
                if px + 1 < width {
                    y_data[row * width + px + 1] = y1;
                }
                let cx = px / 2;
                if cx < chroma_w {
                    u_422[row * chroma_w + cx] = u as u16;
                    v_422[row * chroma_w + cx] = v as u16;
                }
                x += 4;
            }
        }
    }

    let mut u_data = vec![0u8; chroma_w * chroma_h];
    let mut v_data = vec![0u8; chroma_w * chroma_h];
    for cy in 0..chroma_h {
        let r0 = (cy * 2).min(height.saturating_sub(1));
        let r1 = (cy * 2 + 1).min(height.saturating_sub(1));
        for cx in 0..chroma_w {
            let u = (u_422[r0 * chroma_w + cx] + u_422[r1 * chroma_w + cx]).div_ceil(2);
            let v = (v_422[r0 * chroma_w + cx] + v_422[r1 * chroma_w + cx]).div_ceil(2);
            u_data[cy * chroma_w + cx] = u as u8;
            v_data[cy * chroma_w + cx] = v as u8;
        }
    }

    VideoFrame {
        pts: None,
        planes: vec![
            VideoPlane {
                stride: width,
                data: y_data,
            },
            VideoPlane {
                stride: chroma_w,
                data: u_data,
            },
            VideoPlane {
                stride: chroma_w,
                data: v_data,
            },
        ],
    }
}

// ─────────────────────────── Frame splitting ─────────────────────────────────

/// How a `BlobDecoder` carves submitted `Packet`s into VT access units.
///
/// JPEG and ProRes are container-framed: each `Packet` is already exactly
/// one self-contained compressed frame, so the bytes pass straight through.
/// An MPEG-2 *elementary* stream is not pre-framed — a packet may carry one
/// picture, several pictures, or a sequence/GOP header followed by pictures.
/// Splitting an elementary stream into per-picture access units is intrinsic
/// bitstream framing (the codec's job, not a container's), so the splitter
/// lives in the codec bridge.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FrameSplit {
    /// One `Packet` == one VT access unit (JPEG, ProRes).
    Whole,
    /// MPEG-2 elementary stream: split on picture start codes, attaching any
    /// preceding sequence/GOP/extension headers to the following picture.
    Mpeg2Es,
}

/// Split an MPEG-2 elementary-stream buffer into per-picture access units.
///
/// MPEG-2 start codes are `00 00 01 xx`. The picture start code is
/// `00 00 01 00`. Each access unit we emit is "everything from one
/// picture-start-code boundary up to (but not including) the next picture
/// start code", with any leading sequence header (`b3`), GOP header (`b8`),
/// or extension (`b5`) bytes that precede the first picture attached to it.
/// VideoToolbox accepts a sequence-header-prefixed picture as a complete
/// MPEG-2 access unit.
fn split_mpeg2_access_units(buf: &[u8]) -> Vec<&[u8]> {
    // Collect byte offsets of every picture start code (00 00 01 00).
    let mut picture_starts: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i + 4 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && buf[i + 3] == 0 {
            picture_starts.push(i);
            i += 4;
        } else {
            i += 1;
        }
    }

    if picture_starts.is_empty() {
        // No picture start code at all — hand the whole buffer to VT and let
        // it decide. (Defensive: shouldn't happen for a valid ES.)
        return if buf.is_empty() {
            Vec::new()
        } else {
            vec![buf]
        };
    }

    let mut units: Vec<&[u8]> = Vec::new();
    for (idx, &start) in picture_starts.iter().enumerate() {
        // For the first picture, include any leading sequence/GOP/extension
        // headers (everything from offset 0). VT needs the sequence header
        // to size the decoder; carrying it on the first picture is the
        // standard MPEG-2 access-unit shape.
        let unit_start = if idx == 0 { 0 } else { start };
        let unit_end = picture_starts.get(idx + 1).copied().unwrap_or(buf.len());
        if unit_end > unit_start {
            units.push(&buf[unit_start..unit_end]);
        }
    }
    units
}

// ─────────────────────────── Decoder ─────────────────────────────────────────

/// Blob-style VTDecompressionSession decoder.
///
/// Used for any codec whose format description can be built from just
/// `(codec_type, width, height)` and whose frames are whole-payload
/// CMBlockBuffers (JPEG, ProRes) or per-picture access units carved from an
/// elementary stream (MPEG-2).
pub struct BlobDecoder {
    codec_id: CodecId,
    codec_type: u32,
    width: usize,
    height: usize,
    framer: FrameSplit,
    session: sys::VTDecompressionSessionRef,
    fmt_desc: sys::CMVideoFormatDescriptionRef,
    state: Arc<Mutex<DecCallbackState>>,
    output_queue: VecDeque<VideoFrame>,
    pts_counter: i64,
    flushed: bool,
}

// SAFETY: VTDecompressionSession is documented thread-safe; we never share
// the raw pointer across threads concurrently.
unsafe impl Send for BlobDecoder {}

impl BlobDecoder {
    pub fn make(
        codec_id: &str,
        codec_type: u32,
        params: &CodecParameters,
    ) -> Result<Box<dyn Decoder>> {
        Self::make_with_framer(codec_id, codec_type, FrameSplit::Whole, params)
    }

    pub fn make_with_framer(
        codec_id: &str,
        codec_type: u32,
        framer: FrameSplit,
        params: &CodecParameters,
    ) -> Result<Box<dyn Decoder>> {
        sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;
        let width = params.width.unwrap_or(0) as usize;
        let height = params.height.unwrap_or(0) as usize;
        if width == 0 || height == 0 {
            return Err(Error::invalid(
                "blob decoder requires width/height in CodecParameters",
            ));
        }
        Ok(Box::new(BlobDecoder {
            codec_id: CodecId::new(codec_id),
            codec_type,
            width,
            height,
            framer,
            session: std::ptr::null_mut(),
            fmt_desc: std::ptr::null_mut(),
            state: DecCallbackState::new(),
            output_queue: VecDeque::new(),
            pts_counter: 0,
            flushed: false,
        }))
    }

    fn ensure_session(&mut self) -> Result<()> {
        if !self.session.is_null() {
            return Ok(());
        }
        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;

        // Build format description from (codec_type, width, height).
        let mut fmt_desc: sys::CMVideoFormatDescriptionRef = std::ptr::null_mut();
        let st = unsafe {
            (vt.cm_video_fmt_create)(
                std::ptr::null_mut(),
                self.codec_type,
                self.width as i32,
                self.height as i32,
                std::ptr::null_mut(),
                &mut fmt_desc,
            )
        };
        if st != K_OS_STATUS_NO_ERROR {
            return Err(Error::other(format!(
                "CMVideoFormatDescriptionCreate (codec 0x{:08x}): {st}",
                self.codec_type
            )));
        }

        // Destination attributes: NV12 ('420v') so the callback gets a
        // predictable layout to convert to I420.
        let pixel_fmt_val = K_CV_PIXEL_FORMAT_420_YPCBCRi8_BI_PLANAR_VIDEO_RANGE as i32;
        let pixel_fmt_num = unsafe { cf_number_i32(vt, pixel_fmt_val) };
        let pf_key = unsafe { cf_string(vt, "CVPixelBufferPixelFormatTypeKey") };

        let keys: [*const c_void; 1] = [pf_key as *const c_void];
        let vals: [*const c_void; 1] = [pixel_fmt_num as *const c_void];

        let dest_attrs = unsafe {
            (vt.cf_dict_create)(
                std::ptr::null_mut(),
                keys.as_ptr(),
                vals.as_ptr(),
                1,
                std::ptr::null(),
                std::ptr::null(),
            )
        };

        let state_raw = Arc::as_ptr(&self.state) as *mut c_void;
        let record = sys::VTDecompressionOutputCallbackRecord {
            decomp_output_callback: dec_callback,
            decomp_output_ref_con: state_raw,
        };

        let mut session = std::ptr::null_mut();
        let status = unsafe {
            (vt.vt_decomp_create)(
                std::ptr::null_mut(),
                fmt_desc,
                std::ptr::null_mut(),
                dest_attrs,
                &record,
                &mut session,
            )
        };

        unsafe {
            (vt.cf_release)(dest_attrs);
            (vt.cf_release)(pixel_fmt_num);
            (vt.cf_release)(pf_key);
        }

        if status != K_OS_STATUS_NO_ERROR {
            unsafe { (vt.cf_release)(fmt_desc) };
            return Err(Error::other(format!(
                "VTDecompressionSessionCreate (codec 0x{:08x}): {status}",
                self.codec_type
            )));
        }

        self.session = session;
        // The session retains fmt_desc; we keep our own ref too so we can
        // hand it to subsequent CMSampleBuffer creates.
        self.fmt_desc = fmt_desc;
        Ok(())
    }

    fn submit_frame(&mut self, frame_bytes: &[u8], pts: Option<i64>) -> Result<()> {
        if frame_bytes.is_empty() {
            return Ok(());
        }
        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;

        // Copy frame into a CMBlockBuffer the way the H.264 path does it.
        let data_copy = unsafe {
            let p = libc_malloc(frame_bytes.len());
            if p.is_null() {
                return Err(Error::other("malloc for CMBlockBuffer data failed"));
            }
            std::ptr::copy_nonoverlapping(frame_bytes.as_ptr(), p as *mut u8, frame_bytes.len());
            p
        };

        let mut block_buf: sys::CMBlockBufferRef = std::ptr::null_mut();
        let status = unsafe {
            (vt.cm_block_create_with_mem)(
                std::ptr::null_mut(),
                data_copy,
                frame_bytes.len(),
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                frame_bytes.len(),
                0,
                &mut block_buf,
            )
        };
        if status != K_OS_STATUS_NO_ERROR {
            return Err(Error::other(format!(
                "CMBlockBufferCreateWithMemoryBlock: {status}"
            )));
        }

        let pts_eff = pts.unwrap_or(self.pts_counter);
        self.pts_counter += 1;
        let timing = CMSampleTimingInfo {
            duration: CMTime::make(1, 30),
            presentation_time_stamp: CMTime::make(pts_eff, 1_000_000),
            decode_time_stamp: CMTime::make(i64::MIN, 1),
        };
        let sample_size = frame_bytes.len();

        let mut sample_buf: sys::CMSampleBufferRef = std::ptr::null_mut();
        let status = unsafe {
            (vt.cm_sample_create_ready)(
                std::ptr::null_mut(),
                block_buf,
                self.fmt_desc,
                1,
                1,
                &timing,
                1,
                &sample_size,
                &mut sample_buf,
            )
        };
        unsafe { (vt.cf_release)(block_buf) };
        if status != K_OS_STATUS_NO_ERROR {
            return Err(Error::other(format!("CMSampleBufferCreateReady: {status}")));
        }

        let dec_status = unsafe {
            (vt.vt_decomp_decode)(
                self.session,
                sample_buf,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        unsafe { (vt.cf_release)(sample_buf) };
        if dec_status != K_OS_STATUS_NO_ERROR {
            return Err(Error::other(format!(
                "VTDecompressionSessionDecodeFrame: {dec_status}"
            )));
        }
        unsafe { (vt.vt_decomp_finish)(self.session) };
        Ok(())
    }

    fn pull_frames(&mut self) {
        if let Ok(mut g) = self.state.lock() {
            while let Some(f) = g.frames.pop_front() {
                self.output_queue.push_back(f);
            }
        }
    }
}

impl Drop for BlobDecoder {
    fn drop(&mut self) {
        if let Ok(vt) = sys::vtable() {
            if !self.session.is_null() {
                unsafe { (vt.vt_decomp_invalidate)(self.session) };
            }
            if !self.fmt_desc.is_null() {
                unsafe { (vt.cf_release)(self.fmt_desc) };
            }
        }
    }
}

impl Decoder for BlobDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.flushed = false;

        // Bubble up any error the callback recorded.
        if let Some(e) = self
            .state
            .lock()
            .ok()
            .and_then(|g| g.error.as_ref().map(|s| Error::other(s.clone())))
        {
            return Err(e);
        }

        self.ensure_session()?;
        match self.framer {
            FrameSplit::Whole => {
                self.submit_frame(&packet.data, packet.pts)?;
            }
            FrameSplit::Mpeg2Es => {
                // Carve the elementary stream into per-picture access units.
                // Only the first access unit inherits the packet's PTS; the
                // rest get sequential synthetic timestamps so VT keeps a
                // monotone presentation timeline.
                let units = split_mpeg2_access_units(&packet.data);
                for (idx, unit) in units.iter().enumerate() {
                    let pts = if idx == 0 { packet.pts } else { None };
                    self.submit_frame(unit, pts)?;
                }
            }
        }
        self.pull_frames();
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(f) = self.output_queue.pop_front() {
            return Ok(Frame::Video(f));
        }
        Err(if self.flushed {
            Error::Eof
        } else {
            Error::NeedMore
        })
    }

    fn flush(&mut self) -> Result<()> {
        if !self.session.is_null() {
            if let Ok(vt) = sys::vtable() {
                unsafe { (vt.vt_decomp_finish)(self.session) };
            }
        }
        self.pull_frames();
        self.flushed = true;
        Ok(())
    }
}

// ─────────────────────────── Callback state (encode) ─────────────────────────

struct EncCallbackState {
    packets: VecDeque<Vec<u8>>,
    error: Option<String>,
}

impl EncCallbackState {
    fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            packets: VecDeque::new(),
            error: None,
        }))
    }
}

unsafe extern "C" fn enc_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: u32,
    sample_buffer: sys::CMSampleBufferRef,
) {
    let state_ptr = output_callback_ref_con as *const Mutex<EncCallbackState>;
    let state = unsafe { &*state_ptr };
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    if status != K_OS_STATUS_NO_ERROR {
        guard.error = Some(format!("VT blob-encode callback OSStatus {status}"));
        return;
    }
    if sample_buffer.is_null() {
        return;
    }

    let vt = match sys::vtable() {
        Ok(v) => v,
        Err(e) => {
            guard.error = Some(format!("vtable in blob enc callback: {e}"));
            return;
        }
    };

    let block_buf = unsafe { (vt.cm_sample_get_data_buffer)(sample_buffer) };
    if block_buf.is_null() {
        guard.error = Some("CMSampleBufferGetDataBuffer returned null".to_string());
        return;
    }
    let total_len = unsafe { (vt.cm_block_get_data_length)(block_buf) };
    let mut data = vec![0u8; total_len];
    let st = unsafe {
        (vt.cm_block_copy_data)(block_buf, 0, total_len, data.as_mut_ptr() as *mut c_void)
    };
    if st != K_OS_STATUS_NO_ERROR {
        guard.error = Some(format!("CMBlockBufferCopyDataBytes: {st}"));
        return;
    }
    // No NAL conversion — JPEG/ProRes are already self-contained frames.
    guard.packets.push_back(data);
}

// ─────────────────────────── Encoder ─────────────────────────────────────────

pub struct BlobEncoder {
    codec_id: CodecId,
    session: sys::VTCompressionSessionRef,
    state: Arc<Mutex<EncCallbackState>>,
    output_queue: VecDeque<Packet>,
    output_params: CodecParameters,
    pts_counter: i64,
    width: usize,
    height: usize,
}

// SAFETY: VTCompressionSessionRef is documented thread-safe.
unsafe impl Send for BlobEncoder {}

impl BlobEncoder {
    pub fn make(
        codec_id: &str,
        codec_type: u32,
        params: &CodecParameters,
    ) -> Result<Box<dyn Encoder>> {
        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;

        let width = params.width.unwrap_or(320) as usize;
        let height = params.height.unwrap_or(240) as usize;

        let state = EncCallbackState::new();
        let state_raw = Arc::into_raw(Arc::clone(&state)) as *mut c_void;

        let mut session: sys::VTCompressionSessionRef = std::ptr::null_mut();
        let status = unsafe {
            (vt.vt_comp_create)(
                std::ptr::null_mut(),
                width as i32,
                height as i32,
                codec_type,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                enc_callback,
                state_raw,
                &mut session,
            )
        };

        if status != K_OS_STATUS_NO_ERROR {
            // Reclaim the leaked Arc.
            let _ = unsafe { Arc::from_raw(state_raw as *const Mutex<EncCallbackState>) };
            return Err(Error::other(format!(
                "VTCompressionSessionCreate (codec 0x{codec_type:08x}): OSStatus {status}"
            )));
        }

        // RealTime + AllowFrameReordering=false keep the test deterministic.
        let bool_false = unsafe { cf_number_i32(vt, 0) };
        let reorder_key = unsafe { cf_string(vt, "AllowFrameReordering") };
        unsafe {
            (vt.vt_session_set_property)(session, reorder_key, bool_false);
            (vt.cf_release)(reorder_key);
            (vt.cf_release)(bool_false);
        }
        let bool_true = unsafe { cf_number_i32(vt, 1) };
        let rt_key = unsafe { cf_string(vt, "RealTime") };
        unsafe {
            (vt.vt_session_set_property)(session, rt_key, bool_true);
            (vt.cf_release)(rt_key);
            (vt.cf_release)(bool_true);
        }

        // Prepare (non-fatal on older macOS).
        let _ = unsafe { (vt.vt_comp_prepare)(session) };

        let mut output_params = CodecParameters::video(CodecId::new(codec_id));
        output_params.width = Some(width as u32);
        output_params.height = Some(height as u32);
        output_params.pixel_format = Some(PixelFormat::Yuv420P);
        output_params.frame_rate = params.frame_rate;
        output_params.bit_rate = params.bit_rate;

        Ok(Box::new(BlobEncoder {
            codec_id: CodecId::new(codec_id),
            session,
            state,
            output_queue: VecDeque::new(),
            output_params,
            pts_counter: 0,
            width,
            height,
        }))
    }

    fn frame_to_pixel_buffer(
        &self,
        vt: &sys::Vtable,
        frame: &VideoFrame,
    ) -> Result<sys::CVPixelBufferRef> {
        if frame.planes.len() < 3 {
            return Err(Error::invalid("expected I420 frame with 3 planes"));
        }

        let y_plane = &frame.planes[0];
        let u_plane = &frame.planes[1];
        let v_plane = &frame.planes[2];

        let width = self.width;
        let height = self.height;
        let chroma_w = width.div_ceil(2);
        let chroma_h = height.div_ceil(2);

        let y_len = y_plane.stride * height;
        let uv_len = chroma_w * 2 * chroma_h;

        let mut y_data: Vec<u8> = vec![0u8; y_len];
        let mut uv_data: Vec<u8> = vec![0u8; uv_len];

        // Copy Y (possibly re-stride to width).
        let y_rows = y_plane
            .data
            .len()
            .checked_div(y_plane.stride)
            .map(|r| height.min(r))
            .unwrap_or(0);
        for row in 0..y_rows {
            let src_start = row * y_plane.stride;
            let dst_start = row * width;
            let copy_len = width.min(y_plane.stride);
            if src_start + copy_len <= y_plane.data.len() && dst_start + copy_len <= y_len {
                y_data[dst_start..dst_start + copy_len]
                    .copy_from_slice(&y_plane.data[src_start..src_start + copy_len]);
            }
        }

        // Interleave U + V → UV.
        for row in 0..chroma_h {
            let u_src = row * u_plane.stride;
            let v_src = row * v_plane.stride;
            let uv_dst = row * chroma_w * 2;
            for col in 0..chroma_w {
                let u_val = if u_src + col < u_plane.data.len() {
                    u_plane.data[u_src + col]
                } else {
                    128
                };
                let v_val = if v_src + col < v_plane.data.len() {
                    v_plane.data[v_src + col]
                } else {
                    128
                };
                uv_data[uv_dst + col * 2] = u_val;
                uv_data[uv_dst + col * 2 + 1] = v_val;
            }
        }

        let mut y_boxed = y_data.into_boxed_slice();
        let mut uv_boxed = uv_data.into_boxed_slice();

        let mut plane_ptrs: [*mut c_void; 2] = [
            y_boxed.as_mut_ptr() as *mut c_void,
            uv_boxed.as_mut_ptr() as *mut c_void,
        ];
        let plane_widths: [usize; 2] = [width, chroma_w];
        let plane_heights: [usize; 2] = [height, chroma_h];
        let plane_bpr: [usize; 2] = [width, chroma_w * 2];

        struct PlaneBoxes {
            _y: Box<[u8]>,
            _uv: Box<[u8]>,
        }
        let boxes = Box::new(PlaneBoxes {
            _y: y_boxed,
            _uv: uv_boxed,
        });
        let boxes_raw = Box::into_raw(boxes) as *mut c_void;

        unsafe extern "C" fn release_planes(
            _release_ref_con: *mut c_void,
            data_ptr: *const c_void,
        ) {
            let _ = data_ptr;
        }

        let mut pixel_buf: sys::CVPixelBufferRef = std::ptr::null_mut();
        let ret = unsafe {
            (vt.cv_pb_create_planar)(
                std::ptr::null_mut(),
                width,
                height,
                K_CV_PIXEL_FORMAT_NV12,
                std::ptr::null_mut(),
                0,
                2,
                plane_ptrs.as_mut_ptr(),
                plane_widths.as_ptr(),
                plane_heights.as_ptr(),
                plane_bpr.as_ptr(),
                Some(release_planes),
                boxes_raw,
                std::ptr::null_mut(),
                &mut pixel_buf,
            )
        };

        if ret != 0 {
            // Reclaim our box; the release callback won't fire.
            let _ = unsafe { Box::from_raw(boxes_raw as *mut PlaneBoxes) };
            return Err(Error::other(format!(
                "CVPixelBufferCreateWithPlanarBytes: CVReturn {ret}"
            )));
        }
        Ok(pixel_buf)
    }
}

impl Drop for BlobEncoder {
    fn drop(&mut self) {
        if self.session.is_null() {
            return;
        }
        if let Ok(vt) = sys::vtable() {
            unsafe { (vt.vt_comp_invalidate)(self.session) };
        }
    }
}

impl Encoder for BlobEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_params(&self) -> &CodecParameters {
        &self.output_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf = match frame {
            Frame::Video(v) => v,
            _ => return Err(Error::invalid("expected Video frame")),
        };

        let pts = vf.pts.unwrap_or(self.pts_counter);
        self.pts_counter += 1;

        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;
        let pixel_buf = self.frame_to_pixel_buffer(vt, vf)?;

        let pts_time = CMTime::make(pts, 1_000_000);
        let dur_time = CMTime::make(1, 30);

        let status = unsafe {
            (vt.vt_comp_encode)(
                self.session,
                pixel_buf,
                pts_time,
                dur_time,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        unsafe { (vt.cf_release)(pixel_buf) };
        if status != K_OS_STATUS_NO_ERROR {
            return Err(Error::other(format!(
                "VTCompressionSessionEncodeFrame: {status}"
            )));
        }

        let complete_status =
            unsafe { (vt.vt_comp_complete)(self.session, CMTime::make(i64::MAX, 1)) };
        if complete_status != K_OS_STATUS_NO_ERROR {
            return Err(Error::other(format!(
                "VTCompressionSessionCompleteFrames: {complete_status}"
            )));
        }

        let mut guard = self
            .state
            .lock()
            .map_err(|_| Error::other("lock poisoned"))?;
        if let Some(ref e) = guard.error {
            return Err(Error::other(e.clone()));
        }
        while let Some(data) = guard.packets.pop_front() {
            let pkt = Packet::new(0, TimeBase::new(1, 1_000_000), data).with_pts(pts);
            self.output_queue.push_back(pkt);
        }
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(pkt) = self.output_queue.pop_front() {
            return Ok(pkt);
        }
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        if self.session.is_null() {
            return Ok(());
        }
        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;
        let status = unsafe { (vt.vt_comp_complete)(self.session, CMTime::make(i64::MAX, 1)) };
        if status != K_OS_STATUS_NO_ERROR {
            return Err(Error::other(format!(
                "VTCompressionSessionCompleteFrames (flush): {status}"
            )));
        }
        let mut guard = self
            .state
            .lock()
            .map_err(|_| Error::other("lock poisoned"))?;
        while let Some(data) = guard.packets.pop_front() {
            let pkt = Packet::new(0, TimeBase::new(1, 1_000_000), data);
            self.output_queue.push_back(pkt);
        }
        Ok(())
    }
}

// ─────────────────────────── Codec-type constants ────────────────────────────

/// kCMVideoCodecType_JPEG = 'jpeg' (0x6A706567).
pub const K_CM_VIDEO_CODEC_TYPE_JPEG: u32 = 0x6A706567;
/// kCMVideoCodecType_AppleProRes422 = 'apcn' (0x6170636E).
pub const K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422: u32 = 0x6170636E;
/// kCMVideoCodecType_AppleProRes422HQ = 'apch' (0x61706368).
pub const K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_HQ: u32 = 0x61706368;
/// kCMVideoCodecType_AppleProRes422LT = 'apcs' (0x61706373).
pub const K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_LT: u32 = 0x61706373;
/// kCMVideoCodecType_AppleProRes422Proxy = 'apco' (0x6170636F).
pub const K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_PROXY: u32 = 0x6170636F;
/// kCMVideoCodecType_AppleProRes4444 = 'ap4h' (0x61703468).
pub const K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_4444: u32 = 0x61703468;
/// kCMVideoCodecType_AppleProRes4444XQ = 'ap4x' (0x61703478).
pub const K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_4444_XQ: u32 = 0x61703478;
/// kCMVideoCodecType_MPEG2Video = 'mp2v' (0x6D703276).
pub const K_CM_VIDEO_CODEC_TYPE_MPEG2_VIDEO: u32 = 0x6D703276;
/// kCMVideoCodecType_VP9 = 'vp09' (0x76703039). Documented in Apple's
/// CoreMedia headers; hardware decode lands on M1+ Apple Silicon, with
/// software fallback on Intel Macs that lack the dedicated VP9 IP.
/// Decode-only (VideoToolbox exposes no VP9 compression session).
pub const K_CM_VIDEO_CODEC_TYPE_VP9: u32 = 0x76703039;

// ─────────────────────────── Public factories ────────────────────────────────

pub fn make_jpeg_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    BlobDecoder::make("mjpeg", K_CM_VIDEO_CODEC_TYPE_JPEG, params)
}

pub fn make_jpeg_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    BlobEncoder::make("mjpeg", K_CM_VIDEO_CODEC_TYPE_JPEG, params)
}

pub fn make_prores_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    // Default-decode as ProRes 422 (apcn) — the format description carries the
    // explicit type and VT internally dispatches to the right ProRes flavour
    // once it sees the frame header. Container demuxers can pass a different
    // fourcc via `CodecParameters::tag` in a future round.
    BlobDecoder::make("prores", K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422, params)
}

pub fn make_prores_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    // Default-encode as ProRes 422 (apcn). Profile-selection via params.tag
    // is a future-round item.
    BlobEncoder::make("prores", K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422, params)
}

/// MPEG-2 video decoder via VideoToolbox.
///
/// Decode-only: VideoToolbox exposes a hardware/SW MPEG-2 *decoder*
/// (`kCMVideoCodecType_MPEG2Video`) but no MPEG-2 encoder, so there is no
/// matching `make_mpeg2_encoder`. Input is an MPEG-2 elementary stream;
/// the `FrameSplit::Mpeg2Es` framer carves it into per-picture access units
/// before handing each to a `VTDecompressionSession`.
pub fn make_mpeg2_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    BlobDecoder::make_with_framer(
        "mpeg2video",
        K_CM_VIDEO_CODEC_TYPE_MPEG2_VIDEO,
        FrameSplit::Mpeg2Es,
        params,
    )
}

/// VP9 video decoder via VideoToolbox.
///
/// Decode-only: VideoToolbox exposes a VP9 *decoder*
/// (`kCMVideoCodecType_VP9` = `'vp09'`) but no VP9 compression session,
/// so there is no matching `make_vp9_encoder`. Hardware decode is wired
/// on M1+ Apple Silicon; older Intel Macs that lack the dedicated VP9 IP
/// either fall back to a software path inside VT or return a non-zero
/// `OSStatus` at session creation (in which case the registry retries the
/// next-priority impl, typically the pure-Rust VP9 decoder).
///
/// Framing: VP9 has no Annex-B / picture-start-code mechanism — frames are
/// container-framed (IVF / Matroska / MP4), so each demuxed `Packet` is
/// already exactly one VP9 superframe / frame and goes through unchanged.
/// `FrameSplit::Whole` is therefore correct here.
pub fn make_vp9_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    BlobDecoder::make("vp9", K_CM_VIDEO_CODEC_TYPE_VP9, params)
}

#[cfg(test)]
mod tests {
    use super::split_mpeg2_access_units;

    // Start codes: B3 = sequence header, B8 = GOP, 00 = picture, B5 = ext.
    const SEQ: &[u8] = &[0x00, 0x00, 0x01, 0xB3, 0xAA];
    const GOP: &[u8] = &[0x00, 0x00, 0x01, 0xB8, 0xBB];
    const PIC: &[u8] = &[0x00, 0x00, 0x01, 0x00, 0xCC];
    const SLICE: &[u8] = &[0x00, 0x00, 0x01, 0x01, 0xDD];

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.iter().flat_map(|p| p.iter().copied()).collect()
    }

    #[test]
    fn single_picture_with_seq_header() {
        // SEQ + PIC + SLICE → one access unit covering the whole buffer.
        let buf = cat(&[SEQ, PIC, SLICE]);
        let units = split_mpeg2_access_units(&buf);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0], &buf[..]);
    }

    #[test]
    fn two_pictures_first_keeps_headers() {
        // SEQ + GOP + PIC1 + SLICE + PIC2 + SLICE → two access units; the
        // first inherits the leading sequence/GOP headers, the second starts
        // at its own picture start code.
        let pic1 = cat(&[SEQ, GOP, PIC, SLICE]);
        let pic2 = cat(&[PIC, SLICE]);
        let buf = cat(&[&pic1, &pic2]);
        let units = split_mpeg2_access_units(&buf);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0], &pic1[..]);
        assert_eq!(units[1], &pic2[..]);
    }

    #[test]
    fn no_picture_start_code_returns_whole() {
        // A buffer with only a sequence header (no picture) is handed through
        // intact rather than dropped.
        let buf = cat(&[SEQ]);
        let units = split_mpeg2_access_units(&buf);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0], &buf[..]);
    }

    #[test]
    fn empty_buffer_yields_nothing() {
        assert!(split_mpeg2_access_units(&[]).is_empty());
    }
}
