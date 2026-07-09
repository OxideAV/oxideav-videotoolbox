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

use crate::encoder::{
    frame_duration_us, parse_constant_bit_rate, parse_data_rate_limits, parse_keyframe_interval,
    parse_keyframe_interval_duration, resolve_expected_frame_rate, vt_error,
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
    presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    let state_ptr = output_callback_ref_con as *const Mutex<DecCallbackState>;
    let state = unsafe { &*state_ptr };
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    if status != K_OS_STATUS_NO_ERROR {
        guard.error = Some(format!(
            "VT blob-decode callback: OSStatus {}",
            sys::describe_os_status(status)
        ));
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
        guard.error = Some(format!(
            "CVPixelBufferLockBaseAddress: {}",
            sys::describe_os_status(ret)
        ));
        return;
    }

    let width = unsafe { (vt.cv_pb_get_width)(image_buffer) };
    let height = unsafe { (vt.cv_pb_get_height)(image_buffer) };
    let pixel_fmt = unsafe { (vt.cv_pb_get_pixel_format)(image_buffer) };

    let frame = decode_pixel_buffer(vt, image_buffer, width, height, pixel_fmt);

    unsafe { (vt.cv_pb_unlock)(image_buffer, 0) };

    match frame {
        Ok(mut f) => {
            // Recover the presentation timestamp VT hands back for this
            // frame. `submit_frame` wraps `packet.pts` (or a sequential
            // decode-order counter when the packet carried none) in a
            // timescale-1 000 000 CMTime, and VT returns that same time
            // here in presentation order, so `value` is the caller's own
            // PTS number.
            f.pts = presentation_time_stamp
                .is_valid()
                .then_some(presentation_time_stamp.value);
            guard.frames.push_back(f);
        }
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
    /// MPEG-4 Part 2 elementary stream: split on VOP (Video Object Plane)
    /// start codes, attaching any preceding VOS / Visual Object / VO / VOL /
    /// GOV / user-data headers to the following VOP. Per ISO/IEC 14496-2,
    /// start codes are `00 00 01 xx` and the VOP start code is `xx = B6`.
    Mpeg4PartTwoEs,
    /// AV1 temporal-unit `Packet`s — submitted verbatim (one temporal unit
    /// per packet) like [`FrameSplit::Whole`], but with an additional
    /// sniffer that extracts the AV1 Sequence Header OBU from the first
    /// packet and wraps it in an `av1C` `AV1CodecConfigurationRecord` for
    /// supply via `SampleDescriptionExtensionAtoms`. VT on some hosts
    /// requires the Sequence Header out-of-band; supplying av1C here lets
    /// those hosts open the session even when the bitstream alone wouldn't
    /// have been sufficient (analogous to the MPEG-4 Part 2 ESDS path).
    Av1Whole,
    /// VVC (H.266) Annex-B elementary stream: split on AUD/PH/VCL boundaries
    /// (see [`split_vvc_access_units`]) into per-access-unit payloads, with
    /// leading non-VCL NAL units (DCI / OPI / VPS / SPS / PPS / PREFIX_APS)
    /// attached to the first access unit so the configuration travels with
    /// it. The first packet's configuration prefix is additionally wrapped
    /// in a `VvcDecoderConfigurationRecord` (per ISO/IEC 14496-15 §11.2.4.2)
    /// and supplied to VT via `SampleDescriptionExtensionAtoms = { "vvcC":
    /// CFData }`, analogous to the MPEG-4 Part 2 ESDS and AV1 av1C paths.
    VvcEs,
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

/// Split an MPEG-4 Part 2 elementary-stream buffer into per-VOP access units.
///
/// Per ISO/IEC 14496-2, start codes are `00 00 01 xx` and the VOP (Video
/// Object Plane) start code is `xx = B6`. Other key codes that can precede a
/// VOP and need to ride along on the first access unit:
///
/// * `B0` Visual Object Sequence (VOS) start
/// * `B1` VOS end
/// * `B5` Visual Object start
/// * `00..1F` Video Object start (VO)
/// * `20..2F` Video Object Layer start (VOL) — carries width/height/profile
/// * `B3` Group of VOP (GOV) start
/// * `B2` user data
///
/// VideoToolbox needs the VOL (or an equivalent extradata blob) to size the
/// decoder, so we attach every leading header byte to the first VOP exactly
/// as `split_mpeg2_access_units` does for sequence headers.
fn split_mpeg4_part_two_access_units(buf: &[u8]) -> Vec<&[u8]> {
    // Collect byte offsets of every VOP start code (00 00 01 B6).
    let mut vop_starts: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i + 4 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && buf[i + 3] == 0xB6 {
            vop_starts.push(i);
            i += 4;
        } else {
            i += 1;
        }
    }

    if vop_starts.is_empty() {
        return if buf.is_empty() {
            Vec::new()
        } else {
            vec![buf]
        };
    }

    let mut units: Vec<&[u8]> = Vec::new();
    for (idx, &start) in vop_starts.iter().enumerate() {
        // First VOP inherits every leading header byte so VT can size the
        // decoder from the VOL embedded in the stream.
        let unit_start = if idx == 0 { 0 } else { start };
        let unit_end = vop_starts.get(idx + 1).copied().unwrap_or(buf.len());
        if unit_end > unit_start {
            units.push(&buf[unit_start..unit_end]);
        }
    }
    units
}

/// Extract the MPEG-4 Part 2 configuration prefix (VOS / Visual Object / VO /
/// VOL / optionally GOV / user-data) from the leading bytes of an elementary
/// stream — everything up to (but not including) the first VOP start code
/// (`00 00 01 B6`).
///
/// Returns `None` if no VOP start code is found, or if the buffer begins with
/// a VOP (no configuration to extract). The returned slice is suitable as the
/// `DecoderSpecificInfo` payload of an MPEG-4 Part 2 ESDS configuration.
///
/// Per ISO/IEC 14496-2, the configuration headers a hardware decoder needs
/// are the VOS (`B0`) and at minimum one VOL (`20..2F`); GOV (`B3`),
/// user-data (`B2`), and the Visual Object (`B5`) headers are commonly
/// included in the same prefix and ride along.
pub fn extract_mpeg4_part_two_vol(buf: &[u8]) -> Option<&[u8]> {
    let mut i = 0usize;
    while i + 4 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && buf[i + 3] == 0xB6 {
            return if i == 0 { None } else { Some(&buf[..i]) };
        }
        i += 1;
    }
    None
}

/// Append a 4-byte BER length (always 4-byte form so the resulting blob is a
/// stable length per ISO/IEC 14496-1).
fn append_ber_length(out: &mut Vec<u8>, mut value: u32) {
    let mut bytes = [0u8; 4];
    for i in (0..4).rev() {
        bytes[i] = (value & 0x7F) as u8;
        value >>= 7;
    }
    for b in &mut bytes[..3] {
        *b |= 0x80;
    }
    out.extend_from_slice(&bytes);
}

/// Wrap an MPEG-4 Part 2 VOL configuration blob in a complete `esds` atom
/// payload (the inner bytes that go inside the ISO BMFF `esds` box) per
/// ISO/IEC 14496-1 §7.2.6 + ISO/IEC 14496-14 §5.6.
///
/// Structure:
///
/// * 4 bytes: FullBox version (`0`) + flags (`0`).
/// * `ES_Descriptor` (tag `0x03`)
///   * `ES_ID` (2 bytes BE) + flags (1 byte) — both zero (no OCR, no URL,
///     no dependsOn).
///   * `DecoderConfigDescriptor` (tag `0x04`)
///     * `ObjectTypeIndication` = `0x20` (MPEG-4 Visual / Part 2).
///     * `streamType<<2 | upStream | reserved` =
///       `(0x04<<2) | 0 | 1` = `0x11` (`streamType = 4` is VisualStream).
///     * `bufferSizeDB` (3 bytes BE) = `0`.
///     * `maxBitrate` (4 bytes BE) = `0`.
///     * `avgBitrate` (4 bytes BE) = `0`.
///     * `DecoderSpecificInfo` (tag `0x05`)
///       * VOL bytes (the elementary-stream prefix passed in).
///   * `SLConfigDescriptor` (tag `0x06`)
///     * 1 byte `predefined` = `0x02` (mp4-file SL config — VT accepts it).
///
/// VideoToolbox's MPEG-4 Part 2 decoder picks this up via
/// `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms`
/// keyed by the four-character atom name `"esds"`.
pub fn build_mpeg4_part_two_esds(vol: &[u8]) -> Vec<u8> {
    // DecoderSpecificInfo (tag 0x05): length = vol.len()
    let mut dsi = Vec::with_capacity(5 + vol.len());
    dsi.push(0x05);
    append_ber_length(&mut dsi, vol.len() as u32);
    dsi.extend_from_slice(vol);

    // DecoderConfigDescriptor (tag 0x04): 13 bytes header + DSI
    let mut dcd = Vec::with_capacity(5 + 13 + dsi.len());
    dcd.push(0x04);
    let dcd_payload_len = 13 + dsi.len() as u32;
    append_ber_length(&mut dcd, dcd_payload_len);
    dcd.push(0x20); // ObjectTypeIndication: MPEG-4 Visual (Part 2)
    dcd.push((0x04 << 2) | 0x01); // streamType=4 (VisualStream), upStream=0, reserved=1
    dcd.extend_from_slice(&[0, 0, 0]); // bufferSizeDB (24-bit)
    dcd.extend_from_slice(&[0, 0, 0, 0]); // maxBitrate
    dcd.extend_from_slice(&[0, 0, 0, 0]); // avgBitrate
    dcd.extend_from_slice(&dsi);

    // SLConfigDescriptor (tag 0x06): 1 byte predefined=2 (mp4 file)
    let mut slc = Vec::with_capacity(6);
    slc.push(0x06);
    append_ber_length(&mut slc, 1);
    slc.push(0x02);

    // ES_Descriptor (tag 0x03): 3-byte header + DCD + SLC
    let mut esd = Vec::with_capacity(5 + 3 + dcd.len() + slc.len());
    esd.push(0x03);
    let esd_payload_len = 3 + dcd.len() as u32 + slc.len() as u32;
    append_ber_length(&mut esd, esd_payload_len);
    esd.extend_from_slice(&[0, 0, 0]); // ES_ID (2 bytes) + flags (1 byte)
    esd.extend_from_slice(&dcd);
    esd.extend_from_slice(&slc);

    // esds FullBox payload: 4 bytes version/flags + ES_Descriptor.
    let mut esds = Vec::with_capacity(4 + esd.len());
    esds.extend_from_slice(&[0, 0, 0, 0]);
    esds.extend_from_slice(&esd);
    esds
}

// ─────────────────────────── AV1 av1C helpers ────────────────────────────────

/// `OBU_SEQUENCE_HEADER` — AV1 spec §6.2.2 obu_type = 1.
pub(crate) const AV1_OBU_SEQUENCE_HEADER: u8 = 1;

/// Read a uleb128-encoded value from `buf[off..]`, returning
/// `(value, bytes_consumed)`. The AV1 low-overhead bitstream format uses
/// uleb128 for `obu_size` (per AV1 spec §4.10.5 / §5.3.1). Returns `None`
/// if the buffer ends mid-value or the encoded value would overflow 32
/// bits (the spec caps obu_size at 32 bits, see §4.10.5).
fn read_uleb128(buf: &[u8], off: usize) -> Option<(u32, usize)> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    let mut i = off;
    // Maximum of 8 continuation bytes (each carrying 7 value bits) covers
    // 56 bits; we additionally bound the final value at u32 because the
    // AV1 spec caps obu_size at 2^32 - 1.
    for _ in 0..8 {
        if i >= buf.len() {
            return None;
        }
        let b = buf[i];
        i += 1;
        value |= ((b & 0x7F) as u64) << shift;
        if value > u32::MAX as u64 {
            return None;
        }
        if (b & 0x80) == 0 {
            return Some((value as u32, i - off));
        }
        shift += 7;
    }
    None
}

/// Locate the first OBU of type `target_obu_type` in `buf` (the bytes of
/// one AV1 low-overhead-bitstream temporal unit, per spec §5.2). Returns
/// the slice covering the full OBU (header byte(s) + uleb128 size field +
/// payload), or `None` if no matching OBU is found or the buffer is
/// malformed.
///
/// AV1's low-overhead bitstream format (the shape produced by IVF /
/// Matroska / MP4 demuxers, per the AV1 ISOBMFF binding spec §2.4) sets
/// `obu_has_size_field = 1` on every OBU. The OBU layout per spec §5.3.2
/// is therefore: 1-byte header, optional 1-byte extension header (when
/// `obu_extension_flag = 1`), uleb128 `obu_size`, then `obu_size` payload
/// bytes.
fn find_av1_obu(buf: &[u8], target_obu_type: u8) -> Option<&[u8]> {
    let mut i = 0usize;
    while i < buf.len() {
        let header = buf[i];
        // obu_forbidden_bit must be 0 (AV1 spec §6.2.2). If not, the buffer
        // is not a valid OBU stream — bail out.
        if (header & 0x80) != 0 {
            return None;
        }
        let obu_type = (header >> 3) & 0x0F;
        let extension_flag = (header & 0x04) != 0;
        let has_size_field = (header & 0x02) != 0;
        // Low-overhead bitstream requires has_size_field = 1 (spec §5.2).
        // Without it we can't walk the buffer safely; the round-8 path
        // (no extension atom) takes over.
        if !has_size_field {
            return None;
        }
        let mut cursor = i + 1;
        if extension_flag {
            if cursor >= buf.len() {
                return None;
            }
            cursor += 1;
        }
        let (obu_size, consumed) = read_uleb128(buf, cursor)?;
        cursor += consumed;
        let payload_end = cursor.checked_add(obu_size as usize)?;
        if payload_end > buf.len() {
            return None;
        }
        if obu_type == target_obu_type {
            return Some(&buf[i..payload_end]);
        }
        i = payload_end;
    }
    None
}

/// Extract the AV1 Sequence Header OBU (full bytes including the 1-or-2
/// byte header, uleb128 size field, and payload) from `buf`, which is the
/// bytes of an AV1 low-overhead-bitstream temporal unit.
///
/// Returns the OBU slice exactly as it appears in the input — suitable for
/// inclusion verbatim in the `configOBUs` field of an
/// `AV1CodecConfigurationRecord` (see AV1 ISOBMFF binding spec §2.3.4,
/// which states `configOBUs SHALL contain at most one Sequence Header OBU
/// and if present, it SHALL be the first OBU`).
///
/// Returns `None` if no Sequence Header OBU is present in `buf`, or if the
/// OBU framing is invalid (forbidden-bit set, `obu_has_size_field = 0`,
/// truncated uleb128, payload exceeds buffer).
pub fn extract_av1_sequence_header_obu(buf: &[u8]) -> Option<&[u8]> {
    find_av1_obu(buf, AV1_OBU_SEQUENCE_HEADER)
}

/// Parsed subset of the AV1 Sequence Header OBU fields needed for
/// `AV1CodecConfigurationRecord` (per AV1 ISOBMFF binding spec §2.3.4).
///
/// The Sequence Header OBU bit syntax is laid out in AV1 spec §5.5.1
/// (general sequence header) + §5.5.2 (color config). This struct holds
/// only the fields the av1C record requires; the rest of the sequence
/// header travels along verbatim inside `configOBUs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1SeqHeaderFields {
    pub seq_profile: u8,
    pub seq_level_idx_0: u8,
    pub seq_tier_0: u8,
    pub high_bitdepth: u8,
    pub twelve_bit: u8,
    pub monochrome: u8,
    pub chroma_subsampling_x: u8,
    pub chroma_subsampling_y: u8,
    pub chroma_sample_position: u8,
}

impl Av1SeqHeaderFields {
    /// Conservative defaults used when the Sequence Header OBU payload
    /// can't be fully parsed: 8-bit 4:2:0 main-profile colour layout. The
    /// `configOBUs` field still carries the Sequence Header verbatim so a
    /// fully-spec-compliant consumer re-derives the precise values.
    pub fn defaults() -> Self {
        Self {
            seq_profile: 0,
            seq_level_idx_0: 0,
            seq_tier_0: 0,
            high_bitdepth: 0,
            twelve_bit: 0,
            monochrome: 0,
            chroma_subsampling_x: 1,
            chroma_subsampling_y: 1,
            chroma_sample_position: 0,
        }
    }
}

/// Strict MSB-first bit reader over a byte slice. Returns `None` on
/// exhaustion. Used only to walk the Sequence Header OBU payload to
/// recover the fields needed for `AV1CodecConfigurationRecord` —
/// arithmetic coding and entropy stuff are out of scope here.
struct BitReader<'a> {
    buf: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, bit_pos: 0 }
    }

    fn read(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte_idx = self.bit_pos >> 3;
            if byte_idx >= self.buf.len() {
                return None;
            }
            let bit_idx = 7 - (self.bit_pos & 7);
            let b = (self.buf[byte_idx] >> bit_idx) & 1;
            v = (v << 1) | b as u32;
            self.bit_pos += 1;
        }
        Some(v)
    }
}

/// Parse the subset of Sequence Header OBU fields needed for the av1C
/// record (per AV1 spec §5.5.1 + §5.5.2). `payload` is the OBU payload
/// (the bytes after the OBU header and uleb128 size field).
///
/// Returns `Av1SeqHeaderFields::defaults()` if the payload is too short to
/// reach a field — av1C will still be built with the Sequence Header OBU
/// in `configOBUs`, which is the authoritative source per the binding
/// spec §2.3.4.
pub fn parse_av1_seq_header_fields(payload: &[u8]) -> Av1SeqHeaderFields {
    let mut out = Av1SeqHeaderFields::defaults();
    let mut br = BitReader::new(payload);

    let Some(sp) = br.read(3) else {
        return out;
    };
    out.seq_profile = sp as u8;
    let Some(_still_picture) = br.read(1) else {
        return out;
    };
    let Some(reduced) = br.read(1) else {
        return out;
    };

    if reduced == 1 {
        // reduced_still_picture_header path: only seq_level_idx[0] is
        // signalled; seq_tier[0] = 0 by definition (spec §5.5.1).
        let Some(lvl) = br.read(5) else { return out };
        out.seq_level_idx_0 = lvl as u8;
        out.seq_tier_0 = 0;
    } else {
        // Full path: skip the optional timing/decoder-model and operating-
        // point structures we don't need. We only need operating point 0.
        let Some(timing_present) = br.read(1) else {
            return out;
        };
        let mut decoder_model_present = 0u32;
        if timing_present == 1 {
            // timing_info(): num_units_in_display_tick u(32) +
            // time_scale u(32) + equal_picture_interval u(1); if set, then
            // uvlc num_ticks_per_picture_minus_1. We don't read uvlc so
            // bail to defaults if it's set (rare for live VT input).
            let Some(_num_units) = br.read(32) else {
                return out;
            };
            let Some(_time_scale) = br.read(32) else {
                return out;
            };
            let Some(equal_pic) = br.read(1) else {
                return out;
            };
            if equal_pic == 1 {
                // uvlc encoding — give up and rely on configOBUs verbatim.
                return out;
            }
            let Some(dmp) = br.read(1) else { return out };
            decoder_model_present = dmp;
            if decoder_model_present == 1 {
                // decoder_model_info(): 5 + 32 + 10 + 5 = 52 bits. Skip.
                if br.read(52).is_none() {
                    return out;
                }
            }
        }
        let Some(initial_display_present) = br.read(1) else {
            return out;
        };
        let Some(op_cnt_minus_1) = br.read(5) else {
            return out;
        };

        for i in 0..=op_cnt_minus_1 {
            // operating_point_idc[i] u(12)
            let Some(_op_idc) = br.read(12) else {
                return out;
            };
            let Some(lvl) = br.read(5) else { return out };
            let mut tier = 0u32;
            if lvl > 7 {
                let Some(t) = br.read(1) else { return out };
                tier = t;
            }
            if i == 0 {
                out.seq_level_idx_0 = lvl as u8;
                out.seq_tier_0 = tier as u8;
            }
            if decoder_model_present == 1 {
                let Some(dm_for_op) = br.read(1) else {
                    return out;
                };
                if dm_for_op == 1 {
                    // operating_parameters_info(i): 2 * (bitrate_minus_1
                    // uvlc) + buffer_size_minus_1 uvlc + ...; we don't
                    // walk uvlc — give up.
                    return out;
                }
            }
            if initial_display_present == 1 {
                let Some(idp_for_op) = br.read(1) else {
                    return out;
                };
                if idp_for_op == 1 {
                    let Some(_idd_minus_1) = br.read(4) else {
                        return out;
                    };
                }
            }
        }
    }

    // Skip to color_config: walk past frame size / id / superblock /
    // filter-intra / intra-edge-filter / [non-reduced flags] / superres /
    // cdef / restoration. We only need the bits up to and including
    // color_config; if any read fails we fall back to defaults.
    let Some(fwb) = br.read(4) else { return out };
    let Some(fhb) = br.read(4) else { return out };
    let n_w = fwb + 1;
    let n_h = fhb + 1;
    if br.read(n_w).is_none() {
        return out;
    }
    if br.read(n_h).is_none() {
        return out;
    }

    let mut frame_id_present = 0u32;
    if reduced == 0 {
        let Some(fid) = br.read(1) else { return out };
        frame_id_present = fid;
    }
    if frame_id_present == 1 {
        if br.read(4).is_none() {
            return out;
        }
        if br.read(3).is_none() {
            return out;
        }
    }
    // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter.
    if br.read(3).is_none() {
        return out;
    }
    if reduced == 0 {
        // enable_interintra_compound, enable_masked_compound,
        // enable_warped_motion, enable_dual_filter, enable_order_hint.
        let Some(flags5) = br.read(5) else { return out };
        let enable_order_hint = flags5 & 1;
        if enable_order_hint == 1 {
            // enable_jnt_comp + enable_ref_frame_mvs.
            if br.read(2).is_none() {
                return out;
            }
        }
        let Some(seq_choose_sct) = br.read(1) else {
            return out;
        };
        let mut seq_force_sct = 2u32; // SELECT_SCREEN_CONTENT_TOOLS = 2.
        if seq_choose_sct == 0 {
            let Some(s) = br.read(1) else { return out };
            seq_force_sct = s;
        }
        if seq_force_sct > 0 {
            let Some(seq_choose_imv) = br.read(1) else {
                return out;
            };
            if seq_choose_imv == 0 && br.read(1).is_none() {
                return out;
            }
        }
        if enable_order_hint == 1 && br.read(3).is_none() {
            return out;
        }
    }
    // enable_superres, enable_cdef, enable_restoration.
    if br.read(3).is_none() {
        return out;
    }

    // color_config (spec §5.5.2).
    let Some(hbd) = br.read(1) else { return out };
    out.high_bitdepth = hbd as u8;
    if out.seq_profile == 2 && out.high_bitdepth == 1 {
        let Some(tb) = br.read(1) else { return out };
        out.twelve_bit = tb as u8;
    }
    if out.seq_profile == 1 {
        out.monochrome = 0;
    } else {
        let Some(mc) = br.read(1) else { return out };
        out.monochrome = mc as u8;
    }
    let Some(cdp) = br.read(1) else { return out };
    if cdp == 1 {
        // color_primaries(8) + transfer_characteristics(8) +
        // matrix_coefficients(8).
        if br.read(24).is_none() {
            return out;
        }
    }
    if out.monochrome == 1 {
        if br.read(1).is_none() {
            // color_range
            return out;
        }
        out.chroma_subsampling_x = 1;
        out.chroma_subsampling_y = 1;
        out.chroma_sample_position = 0;
        return out;
    }
    // color_range path: subsampling depends on seq_profile + BitDepth.
    if br.read(1).is_none() {
        return out;
    }
    let bit_depth = if out.seq_profile == 2 && out.high_bitdepth == 1 {
        if out.twelve_bit == 1 {
            12
        } else {
            10
        }
    } else if out.high_bitdepth == 1 {
        10
    } else {
        8
    };
    if out.seq_profile == 0 {
        out.chroma_subsampling_x = 1;
        out.chroma_subsampling_y = 1;
    } else if out.seq_profile == 1 {
        out.chroma_subsampling_x = 0;
        out.chroma_subsampling_y = 0;
    } else if bit_depth == 12 {
        let Some(sx) = br.read(1) else { return out };
        out.chroma_subsampling_x = sx as u8;
        if out.chroma_subsampling_x == 1 {
            let Some(sy) = br.read(1) else { return out };
            out.chroma_subsampling_y = sy as u8;
        } else {
            out.chroma_subsampling_y = 0;
        }
    } else {
        out.chroma_subsampling_x = 1;
        out.chroma_subsampling_y = 0;
    }
    if out.chroma_subsampling_x == 1 && out.chroma_subsampling_y == 1 {
        if let Some(csp) = br.read(2) {
            out.chroma_sample_position = csp as u8;
        }
    }
    out
}

/// Build an `AV1CodecConfigurationRecord` from a Sequence Header OBU,
/// per the AV1 ISOBMFF binding spec §2.3.3.
///
/// The byte layout (all big-endian, bit-packed from the MSB):
///
/// * Byte 0: `marker(1) = 1`, `version(7) = 1` → `0x81`.
/// * Byte 1: `seq_profile(3)`, `seq_level_idx_0(5)`.
/// * Byte 2: `seq_tier_0(1)`, `high_bitdepth(1)`, `twelve_bit(1)`,
///   `monochrome(1)`, `chroma_subsampling_x(1)`,
///   `chroma_subsampling_y(1)`, `chroma_sample_position(2)`.
/// * Byte 3: `reserved(3) = 0`, `initial_presentation_delay_present(1) = 0`,
///   `reserved(4) = 0`.
/// * Bytes 4..: `configOBUs[]` — the Sequence Header OBU verbatim.
///
/// We always set `initial_presentation_delay_present = 0` because our
/// extension-atom path produces a record valid for the entire decode
/// stream and the spec leaves the `initial_display_delay_minus_1[0]`
/// signalling inside the Sequence Header OBU.
///
/// VideoToolbox picks this record up via
/// `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms` keyed
/// by the four-character atom name `"av1C"`.
pub fn build_av1c_config_record(sequence_header_obu: &[u8]) -> Vec<u8> {
    // Try to extract the Sequence Header payload — i.e. strip the OBU
    // header + uleb128 size field — so we can pull the seq_profile /
    // level / tier / colour-config fields per the binding spec. If
    // anything looks malformed we fall back to the defaults; the
    // configOBUs field always carries the OBU verbatim so consumers can
    // re-derive everything.
    let fields = sequence_header_payload(sequence_header_obu)
        .map(parse_av1_seq_header_fields)
        .unwrap_or_else(Av1SeqHeaderFields::defaults);

    let mut out = Vec::with_capacity(4 + sequence_header_obu.len());
    // Byte 0: marker = 1 in bit 7, version = 1 in bits 6..0.
    out.push(0x81);
    // Byte 1: seq_profile in bits 7..5, seq_level_idx_0 in bits 4..0.
    out.push(((fields.seq_profile & 0x07) << 5) | (fields.seq_level_idx_0 & 0x1F));
    // Byte 2: seq_tier_0 bit 7, high_bitdepth bit 6, twelve_bit bit 5,
    // monochrome bit 4, chroma_subsampling_x bit 3,
    // chroma_subsampling_y bit 2, chroma_sample_position bits 1..0.
    let mut b2 = 0u8;
    b2 |= (fields.seq_tier_0 & 1) << 7;
    b2 |= (fields.high_bitdepth & 1) << 6;
    b2 |= (fields.twelve_bit & 1) << 5;
    b2 |= (fields.monochrome & 1) << 4;
    b2 |= (fields.chroma_subsampling_x & 1) << 3;
    b2 |= (fields.chroma_subsampling_y & 1) << 2;
    b2 |= fields.chroma_sample_position & 0x03;
    out.push(b2);
    // Byte 3: reserved(3)=0, initial_presentation_delay_present(1)=0,
    // reserved(4)=0.
    out.push(0);
    // configOBUs: Sequence Header OBU verbatim.
    out.extend_from_slice(sequence_header_obu);
    out
}

/// Strip the OBU header + uleb128 size field off a Sequence Header OBU,
/// returning the payload slice. Mirrors `find_av1_obu`'s framing.
fn sequence_header_payload(obu: &[u8]) -> Option<&[u8]> {
    if obu.is_empty() {
        return None;
    }
    let header = obu[0];
    if (header & 0x80) != 0 {
        return None;
    }
    let extension_flag = (header & 0x04) != 0;
    let has_size_field = (header & 0x02) != 0;
    if !has_size_field {
        return None;
    }
    let mut cursor = 1usize;
    if extension_flag {
        if cursor >= obu.len() {
            return None;
        }
        cursor += 1;
    }
    let (obu_size, consumed) = read_uleb128(obu, cursor)?;
    cursor += consumed;
    let end = cursor.checked_add(obu_size as usize)?;
    if end > obu.len() {
        return None;
    }
    Some(&obu[cursor..end])
}

// ─────────────────────────── VVC (H.266) helpers ─────────────────────────────
//
// All VVC byte-stream / NAL-unit-type / configuration-record layout below is
// derived from:
//
// * Rec. ITU-T H.266 (V4) (01/2026) — `docs/video/h266/T-REC-H.266-202601-I.pdf`.
//   - §7.3.1.2 NAL unit header (2 bytes: `forbidden_zero_bit(1)` +
//     `nuh_reserved_zero_bit(1)` + `nuh_layer_id(6)` + `nal_unit_type(5)` +
//     `nuh_temporal_id_plus1(3)`).
//   - Table 5 — NAL unit type codes (TRAIL_NUT = 0, IDR_W_RADL = 7,
//     IDR_N_LP = 8, CRA_NUT = 9, GDR_NUT = 10, OPI_NUT = 12, DCI_NUT = 13,
//     VPS_NUT = 14, SPS_NUT = 15, PPS_NUT = 16, PREFIX_APS_NUT = 17,
//     SUFFIX_APS_NUT = 18, PH_NUT = 19, AUD_NUT = 20, …).
//   - Annex B — byte stream format: each NAL unit is preceded by a 3-byte
//     start code prefix `0x00 0x00 0x01`, optionally preceded by one or
//     more leading/trailing/zero-bytes (so 4-byte `0x00 0x00 0x00 0x01`
//     also marks a NAL boundary).
//
// * ISO/IEC 14496-15:2024 §11.2.4.2 — `VvcDecoderConfigurationRecord`:
//   `docs/container/isobmff/ISO_IEC_14496-15-2024.pdf`. The minimal record
//   we build sets `ptl_present_flag = 0` (PTL re-extracted by the decoder
//   from the SPS) and arrays it in the recommended order DCI, OPI, VPS,
//   SPS, PPS, PREFIX_APS, with `LengthSizeMinusOne = 3` (4-byte length
//   prefix) so VT picks up the NAL units via length-prefix framing inside
//   the format-description extension atom.

/// Selected `nal_unit_type` codes from H.266 Table 5 used by the
/// access-unit splitter and the vvcC configuration-record builder. Each
/// constant matches the 5-bit value found in bits 7..3 of the second NAL
/// header byte (i.e. `(byte1 >> 3) & 0x1F`).
pub const VVC_NUT_TRAIL: u8 = 0;
pub const VVC_NUT_STSA: u8 = 1;
pub const VVC_NUT_RADL: u8 = 2;
pub const VVC_NUT_RASL: u8 = 3;
pub const VVC_NUT_IDR_W_RADL: u8 = 7;
pub const VVC_NUT_IDR_N_LP: u8 = 8;
pub const VVC_NUT_CRA: u8 = 9;
pub const VVC_NUT_GDR: u8 = 10;
pub const VVC_NUT_OPI: u8 = 12;
pub const VVC_NUT_DCI: u8 = 13;
pub const VVC_NUT_VPS: u8 = 14;
pub const VVC_NUT_SPS: u8 = 15;
pub const VVC_NUT_PPS: u8 = 16;
pub const VVC_NUT_PREFIX_APS: u8 = 17;
pub const VVC_NUT_SUFFIX_APS: u8 = 18;
pub const VVC_NUT_PH: u8 = 19;
pub const VVC_NUT_AUD: u8 = 20;
pub const VVC_NUT_EOS: u8 = 21;
pub const VVC_NUT_EOB: u8 = 22;
pub const VVC_NUT_PREFIX_SEI: u8 = 23;
pub const VVC_NUT_SUFFIX_SEI: u8 = 24;
pub const VVC_NUT_FD: u8 = 25;

/// Return `true` for a VCL NAL unit type (0..11, the slice-carrying types
/// per H.266 Table 5). Used to distinguish VCL boundaries from parameter
/// set boundaries when carving access units.
pub fn vvc_is_vcl_nut(nut: u8) -> bool {
    nut <= 11
}

/// Decode the 5-bit `nal_unit_type` from a 2-byte VVC NAL unit header.
/// Returns `None` if `header` is shorter than 2 bytes or if the byte 0
/// `forbidden_zero_bit` / `nuh_reserved_zero_bit` are set (both must be 0
/// per H.266 §7.4.2.2 — non-zero indicates non-VVC data or a corrupted
/// stream).
pub fn vvc_nal_unit_type(header: &[u8]) -> Option<u8> {
    if header.len() < 2 {
        return None;
    }
    // Byte 0 layout: forbidden_zero_bit(1) | nuh_reserved_zero_bit(1) |
    // nuh_layer_id(6). The two top bits must both be 0.
    if (header[0] & 0xC0) != 0 {
        return None;
    }
    // Byte 1 layout: nal_unit_type(5) | nuh_temporal_id_plus1(3).
    Some((header[1] >> 3) & 0x1F)
}

/// Walk a VVC Annex-B byte stream and return a vector of `(offset, length)`
/// pairs where each entry covers a single NAL unit's bytes — *not*
/// including the preceding start code prefix.
///
/// Start codes per H.266 Annex B are `0x00 0x00 0x01` (3 bytes) optionally
/// preceded by a `zero_byte = 0x00` (the 4-byte form `0x00 0x00 0x00 0x01`).
/// The walker recognises both forms. Bytes between NAL units that aren't
/// part of a start code (rare leading/trailing zero bytes) are dropped.
///
/// Returns an empty vector for an empty input or for input that contains no
/// start codes at all.
pub fn split_vvc_nal_units(buf: &[u8]) -> Vec<(usize, usize)> {
    let mut nal_starts: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            nal_starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(nal_starts.len());
    for (idx, &start) in nal_starts.iter().enumerate() {
        let mut end = nal_starts.get(idx + 1).copied().unwrap_or(buf.len());
        // The next NAL's start-code prefix can be either 3 or 4 bytes; back
        // up `end` past any preceding `zero_byte` (and past the start code
        // prefix itself, which is `start - 3` for the next NAL).
        if let Some(&next_start) = nal_starts.get(idx + 1) {
            // next_start points to the byte AFTER the start code prefix, so
            // the prefix occupies bytes `next_start - 3 .. next_start`. The
            // optional preceding zero_byte sits at `next_start - 4` if the
            // 4-byte form is in use.
            end = next_start.saturating_sub(3);
            if end > start && buf[end - 1] == 0 {
                end -= 1;
            }
        }
        if end > start {
            out.push((start, end - start));
        }
    }
    out
}

/// Extract every NAL unit of a given `nal_unit_type` from a VVC Annex-B
/// byte stream. Each returned slice covers the NAL unit's bytes (header +
/// RBSP) with no start code prefix. Useful for harvesting VPS / SPS / PPS
/// to populate a `VvcDecoderConfigurationRecord` array.
pub fn extract_vvc_nals_of_type(buf: &[u8], nut: u8) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::new();
    for (off, len) in split_vvc_nal_units(buf) {
        let nal = &buf[off..off + len];
        if let Some(t) = vvc_nal_unit_type(nal) {
            if t == nut {
                out.push(nal);
            }
        }
    }
    out
}

/// Extract the VVC configuration prefix — the leading non-VCL NAL units
/// (DCI, OPI, VPS, SPS, PPS, prefix APS) — from a VVC Annex-B byte stream,
/// stopping at the first VCL NAL unit boundary.
///
/// Returns the slice of `buf` from offset 0 up to (but not including) the
/// start code prefix of the first VCL NAL unit. Returns `None` if `buf`
/// starts with a VCL NAL unit (no configuration to extract) or contains
/// no NAL units at all.
///
/// The slice is suitable as the bitstream prefix that would be supplied to
/// `build_vvc_decoder_config_record` after this function pulls the parameter
/// sets via `extract_vvc_nals_of_type`.
pub fn extract_vvc_config_prefix(buf: &[u8]) -> Option<&[u8]> {
    // Walk the start codes; for each one inspect the NAL unit type and stop
    // when we see a VCL boundary. Return `Some` of the slice from offset 0
    // up to the start of the start code prefix of the first VCL NAL.
    let mut i = 0usize;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            // Start code at [i..i+3]; NAL starts at i+3. Check VCL.
            let nal_off = i + 3;
            if nal_off + 2 <= buf.len() {
                if let Some(nut) = vvc_nal_unit_type(&buf[nal_off..nal_off + 2]) {
                    if vvc_is_vcl_nut(nut) {
                        // Back up past an optional preceding zero_byte (the
                        // 4-byte start code form `00 00 00 01`).
                        let mut prefix_end = i;
                        if prefix_end > 0 && buf[prefix_end - 1] == 0 {
                            prefix_end -= 1;
                        }
                        return if prefix_end == 0 {
                            None
                        } else {
                            Some(&buf[..prefix_end])
                        };
                    }
                }
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    None
}

/// Build an array entry for a `VvcDecoderConfigurationRecord`:
/// `array_completeness(1) | reserved(2)=0 | NAL_unit_type(5)` byte,
/// followed by (when `nut` is neither DCI_NUT nor OPI_NUT) a `u16` count of
/// NAL units, then for each NAL unit a `u16 nal_unit_length` and the NAL
/// unit bytes themselves.
///
/// `array_completeness` is set to `0` (NAL units of the indicated type
/// could also appear in samples) since we don't enforce in-band/out-of-band
/// exclusivity in this round.
fn build_vvc_array(nut: u8, nals: &[&[u8]]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    // First byte: array_completeness(1) | reserved(2)=0 | NAL_unit_type(5).
    out.push(nut & 0x1F);
    if nut != VVC_NUT_DCI && nut != VVC_NUT_OPI {
        let count = nals.len().min(u16::MAX as usize) as u16;
        out.extend_from_slice(&count.to_be_bytes());
    }
    for nal in nals {
        let len = nal.len().min(u16::MAX as usize) as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&nal[..len as usize]);
    }
    out
}

/// Build a minimal `VvcDecoderConfigurationRecord` (vvcC payload, per
/// ISO/IEC 14496-15 §11.2.4.2.2) from the parameter-set NAL units found in
/// `prefix` (a VVC Annex-B byte stream).
///
/// Layout (`ptl_present_flag = 0` form, which lets VT re-extract PTL from
/// the SPS):
///
/// * Byte 0: `reserved(5) = 0b11111` | `LengthSizeMinusOne(2) = 3` |
///   `ptl_present_flag(1) = 0` → `0b11111110` = `0xFE`. The
///   `LengthSizeMinusOne = 3` selects the standard 4-byte length prefix for
///   the NAL units VT will see in the bitstream payload going through
///   `submit_frame`.
/// * Byte 1: `num_of_arrays`.
/// * Bytes 2..: array entries in the order recommended by the spec —
///   DCI, OPI, VPS, SPS, PPS, prefix APS.
///
/// Arrays that have no NAL units in `prefix` are omitted (so a stream with
/// only VPS+SPS+PPS produces a 3-array record).
///
/// VideoToolbox picks this record up via
/// `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms` keyed by
/// the four-character atom name `"vvcC"`.
pub fn build_vvc_decoder_config_record(prefix: &[u8]) -> Vec<u8> {
    // Harvest each parameter-set NAL unit type the spec's array order
    // recommends. Each list's elements are slices into `prefix` (no start
    // codes, just the NAL bytes themselves).
    let dci = extract_vvc_nals_of_type(prefix, VVC_NUT_DCI);
    let opi = extract_vvc_nals_of_type(prefix, VVC_NUT_OPI);
    let vps = extract_vvc_nals_of_type(prefix, VVC_NUT_VPS);
    let sps = extract_vvc_nals_of_type(prefix, VVC_NUT_SPS);
    let pps = extract_vvc_nals_of_type(prefix, VVC_NUT_PPS);
    let prefix_aps = extract_vvc_nals_of_type(prefix, VVC_NUT_PREFIX_APS);

    // Build each array body (skipping empty arrays).
    let arrays: Vec<(u8, &[&[u8]])> = [
        (VVC_NUT_DCI, dci.as_slice()),
        (VVC_NUT_OPI, opi.as_slice()),
        (VVC_NUT_VPS, vps.as_slice()),
        (VVC_NUT_SPS, sps.as_slice()),
        (VVC_NUT_PPS, pps.as_slice()),
        (VVC_NUT_PREFIX_APS, prefix_aps.as_slice()),
    ]
    .into_iter()
    .filter(|(_, n)| !n.is_empty())
    .collect();

    let num_of_arrays = arrays.len().min(u8::MAX as usize) as u8;

    let mut out: Vec<u8> = Vec::new();
    // Byte 0: reserved(5)=0b11111 | LengthSizeMinusOne(2)=3 | ptl_present(1)=0.
    out.push(0b1111_1110);
    // Byte 1: num_of_arrays.
    out.push(num_of_arrays);
    for (nut, nals) in arrays {
        out.extend_from_slice(&build_vvc_array(nut, nals));
    }
    out
}

/// Split a VVC Annex-B elementary-stream buffer into per-access-unit
/// payloads.
///
/// H.266 §7.4.2.4 specifies that an access unit (AU) begins at an AUD NAL
/// unit when one is present, or otherwise at the first VCL NAL unit of a
/// new picture (signalled by the picture header `PH_NUT` immediately
/// preceding the first VCL slice, or by a slice-header context change for
/// streams that omit PH_NUT). For VT submission, splitting on either of:
///
///   * an `AUD_NUT` (= 20), or
///   * a `PH_NUT` (= 19), or
///   * a transition from non-VCL to VCL when no PH_NUT has been seen in the
///     current pending unit
///
/// yields per-picture access units valid as VT `CMSampleBuffer` payloads.
/// All leading parameter-set NAL units (DCI / OPI / VPS / SPS / PPS /
/// PREFIX_APS) preceding the first VCL boundary are attached to the first
/// access unit so the VOL / sequence configuration travels with it.
///
/// If no start codes are present in `buf`, the buffer is returned as a
/// single access unit (defensive: shouldn't happen for a valid VVC
/// elementary stream).
pub fn split_vvc_access_units(buf: &[u8]) -> Vec<&[u8]> {
    // Collect (start_code_prefix_start, nal_unit_type) for every NAL.
    // start_code_prefix_start is the byte index of the first `0x00` of the
    // start code prefix (3- or 4-byte form), so slicing
    // `buf[prefix_start..next_prefix_start]` produces a slice whose first
    // bytes are the start code prefix itself — exactly what VT wants.
    let mut nal_info: Vec<(usize, u8)> = Vec::new();
    let mut i = 0usize;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let nal_off = i + 3;
            // Determine the prefix start (back up one if preceded by 0x00).
            let prefix_start = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
            // Decode nal_unit_type from the 2-byte header that follows the
            // start code prefix.
            let nut = if nal_off + 2 <= buf.len() {
                vvc_nal_unit_type(&buf[nal_off..nal_off + 2]).unwrap_or(0xFF)
            } else {
                0xFF
            };
            nal_info.push((prefix_start, nut));
            i = nal_off; // continue scanning from the byte after start code
        } else {
            i += 1;
        }
    }

    if nal_info.is_empty() {
        return if buf.is_empty() {
            Vec::new()
        } else {
            vec![buf]
        };
    }

    // Determine the offset at which each access unit begins. AU 0 starts
    // at offset 0 and absorbs every leading non-VCL NAL plus the first
    // picture header (PH_NUT) / AUD and the first VCL. Subsequent AU
    // boundaries fire at the next *occurrence* of:
    //   - an AUD_NUT,
    //   - a PH_NUT, or
    //   - a VCL NAL when no PH_NUT was seen since the last boundary.
    //
    // The "saw_*" flags track state since the most recent boundary; the
    // first PH/AUD does not re-open AU 0, it merely arms the state so that
    // the *next* PH/AUD/VCL-after-VCL fires.
    let mut au_starts: Vec<usize> = vec![0];
    let mut saw_vcl_in_current = false;
    let mut saw_ph_in_current = false;
    let mut saw_aud_in_current = false;
    for (idx, &(prefix_start, nut)) in nal_info.iter().enumerate() {
        if idx == 0 {
            // AU 0 always starts at NAL 0 — record the boundary's flags
            // based on its type so a same-type NAL later opens AU 1.
            match nut {
                VVC_NUT_PH => saw_ph_in_current = true,
                VVC_NUT_AUD => saw_aud_in_current = true,
                t if vvc_is_vcl_nut(t) => saw_vcl_in_current = true,
                _ => {}
            }
            continue;
        }
        let start_new = match nut {
            VVC_NUT_AUD if saw_aud_in_current || saw_vcl_in_current || saw_ph_in_current => true,
            VVC_NUT_PH if saw_ph_in_current || saw_vcl_in_current => true,
            t if vvc_is_vcl_nut(t) && !saw_ph_in_current && saw_vcl_in_current => true,
            _ => false,
        };
        if start_new {
            au_starts.push(prefix_start);
            saw_vcl_in_current = false;
            saw_ph_in_current = false;
            saw_aud_in_current = false;
        }
        match nut {
            VVC_NUT_PH => saw_ph_in_current = true,
            VVC_NUT_AUD => saw_aud_in_current = true,
            t if vvc_is_vcl_nut(t) => saw_vcl_in_current = true,
            _ => {}
        }
    }

    // AU 0's prefix_start is always 0 (so the leading parameter sets ride
    // with the first picture). For each AU, the next AU's prefix_start
    // bounds the slice end; for the final AU, the end is buf.len().
    let mut out: Vec<&[u8]> = Vec::with_capacity(au_starts.len());
    for (idx, &start) in au_starts.iter().enumerate() {
        let begin = if idx == 0 { 0 } else { start };
        let end = au_starts.get(idx + 1).copied().unwrap_or(buf.len());
        if end > begin {
            out.push(&buf[begin..end]);
        }
    }
    out
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
    /// Optional configuration-record atom supplied to VT via
    /// `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms`.
    /// The pair is `(atom_key, payload)` where `atom_key` is the
    /// four-character atom name (`"esds"` for MPEG-4 Part 2, `"av1C"`
    /// for AV1) and `payload` is the raw atom bytes. Set lazily on the
    /// first packet by the relevant framer (`Mpeg4PartTwoEs` sniffs the
    /// VOL prefix; `Av1Whole` sniffs the Sequence Header OBU). Other
    /// framers leave the field at `None` and the bare
    /// `(codec_type, width, height)` path covers them.
    extradata: Option<(&'static str, Vec<u8>)>,
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
            extradata: None,
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

        // Build the optional extensions dictionary. When `extradata` is
        // present (MPEG-4 Part 2 ESDS or AV1 av1C after the first packet has
        // been seen), wrap the bytes in
        // `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms =
        // { atom_key: CFData }`. Otherwise pass NULL — the blob path
        // `(codec_type, width, height)` covers JPEG / ProRes / MPEG-2 / VP9.
        let mut extensions: sys::CFDictionaryRef = std::ptr::null_mut();
        let mut ext_inner_dict: sys::CFDictionaryRef = std::ptr::null_mut();
        let mut ext_inner_key: sys::CFStringRef = std::ptr::null_mut();
        let mut ext_inner_val: sys::CFDataRef = std::ptr::null_mut();
        let mut ext_outer_key: sys::CFStringRef = std::ptr::null_mut();
        if let Some((atom_key, atom_bytes)) = &self.extradata {
            unsafe {
                ext_inner_val = sys::cf_data(vt, atom_bytes);
                ext_inner_key = sys::cf_string(vt, atom_key);
                let inner_keys: [*const c_void; 1] = [ext_inner_key as *const c_void];
                let inner_vals: [*const c_void; 1] = [ext_inner_val as *const c_void];
                ext_inner_dict = (vt.cf_dict_create)(
                    std::ptr::null_mut(),
                    inner_keys.as_ptr(),
                    inner_vals.as_ptr(),
                    1,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                ext_outer_key = sys::cf_string(vt, "SampleDescriptionExtensionAtoms");
                let outer_keys: [*const c_void; 1] = [ext_outer_key as *const c_void];
                let outer_vals: [*const c_void; 1] = [ext_inner_dict as *const c_void];
                extensions = (vt.cf_dict_create)(
                    std::ptr::null_mut(),
                    outer_keys.as_ptr(),
                    outer_vals.as_ptr(),
                    1,
                    std::ptr::null(),
                    std::ptr::null(),
                );
            }
        }

        // Build format description from (codec_type, width, height) with the
        // optional extension dictionary attached. VT consumes the dictionary
        // by copying it into the resulting CMVideoFormatDescription; we
        // release our refs immediately after the call returns.
        let mut fmt_desc: sys::CMVideoFormatDescriptionRef = std::ptr::null_mut();
        let st = unsafe {
            (vt.cm_video_fmt_create)(
                std::ptr::null_mut(),
                self.codec_type,
                self.width as i32,
                self.height as i32,
                extensions,
                &mut fmt_desc,
            )
        };
        unsafe {
            if !extensions.is_null() {
                (vt.cf_release)(extensions);
            }
            if !ext_outer_key.is_null() {
                (vt.cf_release)(ext_outer_key);
            }
            if !ext_inner_dict.is_null() {
                (vt.cf_release)(ext_inner_dict);
            }
            if !ext_inner_key.is_null() {
                (vt.cf_release)(ext_inner_key);
            }
            if !ext_inner_val.is_null() {
                (vt.cf_release)(ext_inner_val);
            }
        }
        if st != K_OS_STATUS_NO_ERROR {
            return Err(vt_error(
                &format!(
                    "CMVideoFormatDescriptionCreate (codec 0x{:08x})",
                    self.codec_type
                ),
                st,
            ));
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
            return Err(vt_error(
                &format!(
                    "VTDecompressionSessionCreate (codec 0x{:08x})",
                    self.codec_type
                ),
                status,
            ));
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
            return Err(vt_error("CMBlockBufferCreateWithMemoryBlock", status));
        }

        let pts_eff = pts.unwrap_or(self.pts_counter);
        self.pts_counter += 1;
        let timing = CMSampleTimingInfo {
            duration: CMTime::make(1, 30),
            presentation_time_stamp: CMTime::make(pts_eff, 1_000_000),
            decode_time_stamp: CMTime::invalid(),
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
            return Err(vt_error("CMSampleBufferCreateReady", status));
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
            return Err(vt_error("VTDecompressionSessionDecodeFrame", dec_status));
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
                // Per VTDecompressionSession.h: invalidate to tear the
                // session down, then CFRelease the object reference —
                // sessions are CF objects and invalidating alone leaks
                // them.
                unsafe {
                    (vt.vt_decomp_invalidate)(self.session);
                    (vt.cf_release)(self.session);
                }
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

        // Before the session exists, sniff the codec's configuration record
        // out of the first packet so it can be supplied to VT as a
        // SampleDescriptionExtensionAtoms entry. Two paths today:
        //
        // * MPEG-4 Part 2: VT enforces VOL-via-extension-atoms on some hosts.
        //   Extract the VOL prefix (bytes before the first VOP start code)
        //   and wrap it in an ESDS atom.
        // * AV1: VT on some hosts enforces the Sequence Header OBU be
        //   supplied via av1C extension atoms rather than extracted from the
        //   first temporal unit. Extract the Sequence Header OBU and wrap it
        //   in an AV1CodecConfigurationRecord (av1C atom).
        //
        // Both paths share the `extradata: Option<(atom_key, bytes)>` field;
        // `ensure_session` plumbs whichever is present into the
        // SampleDescriptionExtensionAtoms dictionary.
        if self.session.is_null() && self.extradata.is_none() {
            match self.framer {
                FrameSplit::Mpeg4PartTwoEs => {
                    if let Some(vol) = extract_mpeg4_part_two_vol(&packet.data) {
                        if !vol.is_empty() {
                            self.extradata = Some(("esds", build_mpeg4_part_two_esds(vol)));
                        }
                    }
                }
                FrameSplit::Av1Whole => {
                    if let Some(sh) = extract_av1_sequence_header_obu(&packet.data) {
                        if !sh.is_empty() {
                            self.extradata = Some(("av1C", build_av1c_config_record(sh)));
                        }
                    }
                }
                FrameSplit::VvcEs => {
                    if let Some(prefix) = extract_vvc_config_prefix(&packet.data) {
                        if !prefix.is_empty() {
                            self.extradata =
                                Some(("vvcC", build_vvc_decoder_config_record(prefix)));
                        }
                    }
                }
                _ => {}
            }
        }

        self.ensure_session()?;
        match self.framer {
            FrameSplit::Whole | FrameSplit::Av1Whole => {
                // AV1 packets are container-framed: one temporal unit per
                // `Packet`. Submit verbatim, exactly like the `Whole` path
                // used by JPEG / ProRes / VP9.
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
            FrameSplit::Mpeg4PartTwoEs => {
                // Carve the elementary stream into per-VOP access units (see
                // `split_mpeg4_part_two_access_units`). PTS handling matches
                // the MPEG-2 path.
                let units = split_mpeg4_part_two_access_units(&packet.data);
                for (idx, unit) in units.iter().enumerate() {
                    let pts = if idx == 0 { packet.pts } else { None };
                    self.submit_frame(unit, pts)?;
                }
            }
            FrameSplit::VvcEs => {
                // Carve the VVC Annex-B elementary stream into per-access-
                // unit payloads (see `split_vvc_access_units`). PTS handling
                // matches the MPEG-2 / MPEG-4 Part 2 paths.
                let units = split_vvc_access_units(&packet.data);
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
        guard.error = Some(format!(
            "VT blob-encode callback: OSStatus {}",
            sys::describe_os_status(status)
        ));
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
        guard.error = Some(format!(
            "CMBlockBufferCopyDataBytes: {}",
            sys::describe_os_status(st)
        ));
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
    /// Per-frame duration in the output time base (µs), derived from the
    /// caller's frame rate. `None` when no cadence is known.
    frame_duration_us: Option<i64>,
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
            return Err(vt_error(
                &format!("VTCompressionSessionCreate (codec 0x{codec_type:08x})"),
                status,
            ));
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

        // AverageBitRate — caller-supplied `bit_rate` (bits per second) is
        // forwarded as a CFNumber-i32 to `kVTCompressionPropertyKey_AverageBitRate`.
        // VT's JPEG encoder honours it (rate-capped quality); VT's ProRes
        // encoder does not (ProRes is fixed-CBR per profile) and silently
        // ignores the property. Failure to set is non-fatal.
        if let Some(bps) = params.bit_rate {
            let clamped = bps.min(i32::MAX as u64) as i32;
            let br_val = unsafe { cf_number_i32(vt, clamped) };
            let br_key = unsafe { cf_string(vt, "AverageBitRate") };
            unsafe {
                (vt.vt_session_set_property)(session, br_key, br_val);
                (vt.cf_release)(br_key);
                (vt.cf_release)(br_val);
            }
        }

        // Quality — `options["quality"]` as a Float32 in `[0.0, 1.0]`.
        // The MJPEG encoder uses this as its primary knob (it maps onto
        // the JPEG quality scale that drives the standard quant tables).
        // The ProRes encoder accepts the property but treats it as a
        // hint; profile selection (via codec type) is the main lever.
        if let Some(q_raw) = params.options.get("quality") {
            if let Ok(q) = q_raw.parse::<f32>() {
                if q.is_finite() && (0.0..=1.0).contains(&q) {
                    let q_val = unsafe { sys::cf_number_f32(vt, q) };
                    let q_key = unsafe { cf_string(vt, "Quality") };
                    unsafe {
                        (vt.vt_session_set_property)(session, q_key, q_val);
                        (vt.cf_release)(q_key);
                        (vt.cf_release)(q_val);
                    }
                }
            }
        }

        // MaxKeyFrameInterval — `options["keyframe_interval"]` (frames).
        // Per `VTCompressionProperties.h` the property is CFNumber<int>
        // and accepts 0 as "no forced cadence". MJPEG (intra-only) sees
        // every frame as a keyframe regardless of the property; ProRes
        // is also intra-only, so VT silently ignores the property here.
        // The plumbing is identical to the H.264 / HEVC paths in
        // `encoder.rs` to keep the bridge surface uniform.
        if let Some(kfi_raw) = params.options.get("keyframe_interval") {
            if let Some(kfi) = parse_keyframe_interval(kfi_raw) {
                let kfi_val = unsafe { cf_number_i32(vt, kfi) };
                let kfi_key = unsafe { cf_string(vt, "MaxKeyFrameInterval") };
                unsafe {
                    (vt.vt_session_set_property)(session, kfi_key, kfi_val);
                    (vt.cf_release)(kfi_key);
                    (vt.cf_release)(kfi_val);
                }
            }
        }

        // MaxKeyFrameIntervalDuration — `options["keyframe_interval_duration"]`
        // (seconds, CFNumber<Float64>). Same SDK property as the H.264 /
        // HEVC paths; MJPEG / ProRes are intra-only and ignore it but VT
        // accepts the property unconditionally.
        if let Some(kfd_raw) = params.options.get("keyframe_interval_duration") {
            if let Some(kfd) = parse_keyframe_interval_duration(kfd_raw) {
                let kfd_val = unsafe { sys::cf_number_f64(vt, kfd) };
                let kfd_key = unsafe { cf_string(vt, "MaxKeyFrameIntervalDuration") };
                unsafe {
                    (vt.vt_session_set_property)(session, kfd_key, kfd_val);
                    (vt.cf_release)(kfd_key);
                    (vt.cf_release)(kfd_val);
                }
            }
        }

        // ExpectedFrameRate — `options["expected_frame_rate"]` (Float64
        // fps) or, when absent, derived from `params.frame_rate`. Same
        // SDK property as the H.264 / HEVC paths; the MJPEG encoder uses
        // it to budget the rate-controller and the ProRes encoder treats
        // it as a no-op (ProRes is CBR per profile).
        if let Some(efr) = resolve_expected_frame_rate(params) {
            let efr_val = unsafe { sys::cf_number_f64(vt, efr) };
            let efr_key = unsafe { cf_string(vt, "ExpectedFrameRate") };
            unsafe {
                (vt.vt_session_set_property)(session, efr_key, efr_val);
                (vt.cf_release)(efr_key);
                (vt.cf_release)(efr_val);
            }
        }

        // DataRateLimits — `options["data_rate_limits"]` parsed as a
        // comma-separated list of `bytes:seconds` pairs (1–2 segments).
        // Same SDK property as the H.264 / HEVC paths in `encoder.rs`;
        // MJPEG honours the hard cap on its rate-controlled output and
        // ProRes ignores it (ProRes is fixed-CBR per profile). The
        // plumbing is uniform across all encoder paths to keep the
        // bridge surface minimal.
        if let Some(drl_raw) = params.options.get("data_rate_limits") {
            if let Some(segments) = parse_data_rate_limits(drl_raw) {
                let mut elements: Vec<sys::CFTypeRef> = Vec::with_capacity(segments.len() * 2);
                for seg in &segments {
                    elements.push(unsafe { cf_number_i32(vt, seg.bytes) });
                    elements.push(unsafe { sys::cf_number_f64(vt, seg.seconds) });
                }
                let arr = unsafe { sys::cf_array(vt, &elements) };
                let drl_key = unsafe { cf_string(vt, "DataRateLimits") };
                unsafe {
                    (vt.vt_session_set_property)(session, drl_key, arr);
                    (vt.cf_release)(drl_key);
                    (vt.cf_release)(arr);
                    for e in elements {
                        (vt.cf_release)(e);
                    }
                }
            }
        }

        // ConstantBitRate — `options["constant_bit_rate"]` (CFNumber
        // bits-per-second, macOS 13.0+). Same SDK property as the
        // H.264 / HEVC paths; MJPEG accepts CBR on macOS 13+ where the
        // encoder supports it (and falls back to its default rate
        // controller otherwise via the non-fatal failure path); ProRes
        // returns `kVTPropertyNotSupportedErr` since the profile dictates
        // its CBR target. Failure is non-fatal, matching every other
        // knob in this list.
        if let Some(cbr_raw) = params.options.get("constant_bit_rate") {
            if let Some(cbr) = parse_constant_bit_rate(cbr_raw) {
                let cbr_val = unsafe { cf_number_i32(vt, cbr) };
                let cbr_key = unsafe { cf_string(vt, "ConstantBitRate") };
                unsafe {
                    (vt.vt_session_set_property)(session, cbr_key, cbr_val);
                    (vt.cf_release)(cbr_key);
                    (vt.cf_release)(cbr_val);
                }
            }
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
            frame_duration_us: frame_duration_us(params),
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
            return Err(vt_error("CVPixelBufferCreateWithPlanarBytes", ret));
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
            // Per VTCompressionSession.h: invalidate to tear the session
            // down, then CFRelease the object reference — sessions are CF
            // objects and invalidating alone leaks them.
            unsafe {
                (vt.vt_comp_invalidate)(self.session);
                (vt.cf_release)(self.session);
            }
            // Balance the `Arc::into_raw(Arc::clone(&state))` handed to
            // `VTCompressionSessionCreate` as the callback refcon (see
            // `VtEncoder::drop` for the reasoning).
            let _ = unsafe { Arc::from_raw(Arc::as_ptr(&self.state)) };
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
        // Frame duration from the caller's cadence; nominal 1/30 s when
        // no frame rate is known.
        let dur_time = match self.frame_duration_us {
            Some(us) => CMTime::make(us, 1_000_000),
            None => CMTime::make(1, 30),
        };

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
            return Err(vt_error("VTCompressionSessionEncodeFrame", status));
        }

        let complete_status =
            unsafe { (vt.vt_comp_complete)(self.session, CMTime::make(i64::MAX, 1)) };
        if complete_status != K_OS_STATUS_NO_ERROR {
            return Err(vt_error(
                "VTCompressionSessionCompleteFrames",
                complete_status,
            ));
        }

        let mut guard = self
            .state
            .lock()
            .map_err(|_| Error::other("lock poisoned"))?;
        if let Some(ref e) = guard.error {
            return Err(Error::other(e.clone()));
        }
        while let Some(data) = guard.packets.pop_front() {
            // MJPEG and ProRes (the only codecs behind BlobEncoder) are
            // intra-only: every access unit is a sync sample, so every
            // packet is a keyframe and DTS mirrors PTS.
            let mut pkt = Packet::new(0, TimeBase::new(1, 1_000_000), data)
                .with_pts(pts)
                .with_dts(pts)
                .with_keyframe(true);
            if let Some(dur) = self.frame_duration_us {
                pkt = pkt.with_duration(dur);
            }
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
            return Err(vt_error(
                "VTCompressionSessionCompleteFrames (flush)",
                status,
            ));
        }
        let mut guard = self
            .state
            .lock()
            .map_err(|_| Error::other("lock poisoned"))?;
        while let Some(data) = guard.packets.pop_front() {
            let mut pkt = Packet::new(0, TimeBase::new(1, 1_000_000), data).with_keyframe(true);
            if let Some(dur) = self.frame_duration_us {
                pkt = pkt.with_duration(dur);
            }
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
/// kCMVideoCodecType_MPEG4Video = 'mp4v' (0x6D703476). Documented in
/// Apple's CoreMedia headers; this is MPEG-4 Part 2 (Visual / ASP / SP),
/// distinct from MPEG-4 Part 10 (H.264 — `'avc1'`). Decode-only here:
/// VideoToolbox exposes an MPEG-4 Part 2 *decoder* (used historically for
/// DivX / Xvid playback) but no MPEG-4 Part 2 compression session, so the
/// crate registers only a decoder.
pub const K_CM_VIDEO_CODEC_TYPE_MPEG4_VIDEO: u32 = 0x6D703476;
/// kCMVideoCodecType_AV1 = 'av01' (0x61763031). Documented in Apple's
/// CoreMedia headers as the AV1 codec-type identifier (matches the
/// `av01` sample-entry fourcc defined by the AV1 ISOBMFF mapping at
/// `docs/container/mpeg4/av1-isobmff/`). Hardware decode is gated to
/// Apple Silicon M3+ chips; on older hardware VideoToolbox falls back
/// to its internal software AV1 decoder where available, and returns a
/// non-zero `OSStatus` at session creation when it isn't (the
/// registry's SW fallback to `oxideav-av1` covers that case).
/// **Decode-only here** — round 8 wires the decoder; an encoder
/// factory is a future-round item (VT exposes a `'av01'` *compression*
/// session on macOS 14+ for hosts with the M3+ hardware encoder, but
/// the encode path needs its own callback/pixel-buffer wiring).
pub const K_CM_VIDEO_CODEC_TYPE_AV1: u32 = 0x61763031;
/// kCMVideoCodecType_VVC = 'vvc1' (0x76_76_63_31). Per Apple's
/// CoreMedia headers and ISO/IEC 14496-15 §11.x (the VVC sample-entry
/// fourcc for in-band parameter sets). VideoToolbox first exposes a VVC
/// *decoder* on macOS 26+ (Apple Silicon M3+ for hardware decode);
/// older OS versions either fall back to a VT-internal software path
/// where available or return a non-zero `OSStatus` at session creation
/// (the registry's SW fallback to the pure-Rust `oxideav-h266` decoder
/// covers that case). VVC compression sessions are not yet exposed by
/// VideoToolbox at the time of this round, so the crate registers
/// decode-only.
pub const K_CM_VIDEO_CODEC_TYPE_VVC: u32 = 0x7676_6331;

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
    // Profile selection: caller picks a flavour by setting
    // `CodecParameters::tag = Some(CodecTag::fourcc(b"apch"))` (HQ),
    // `"apco"` (Proxy), `"apcs"` (LT), `"apcn"` (422), `"ap4h"` (4444),
    // or `"ap4x"` (4444 XQ). Unset / unrecognised fourccs default to
    // ProRes 422 (`'apcn'`) — the most common deliverable. The format
    // description's codec-type drives VT's internal flavour selection;
    // pure-bitrate knobs do not apply (each ProRes profile is fixed-CBR).
    let codec_type = prores_codec_type_for_tag(params.tag.as_ref())
        .unwrap_or(K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422);
    BlobEncoder::make("prores", codec_type, params)
}

/// Map a `CodecTag::Fourcc` to the matching `kCMVideoCodecType_AppleProRes*`
/// constant. Returns `None` when the tag isn't a ProRes fourcc (the caller
/// then falls back to the default ProRes 422 codec type).
pub fn prores_codec_type_for_tag(tag: Option<&oxideav_core::CodecTag>) -> Option<u32> {
    let fcc = match tag? {
        oxideav_core::CodecTag::Fourcc(f) => f,
        _ => return None,
    };
    // CodecTag::fourcc() upper-cases ASCII letters at construction, so we
    // match upper-case bytes here. The Apple constants themselves are the
    // historical lower-case-ish fourccs (`'apcn'` etc.) — they are decoded
    // through the equality of the four BE bytes either way.
    match fcc {
        b"APCO" => Some(K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_PROXY),
        b"APCS" => Some(K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_LT),
        b"APCN" => Some(K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422),
        b"APCH" => Some(K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_HQ),
        b"AP4H" => Some(K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_4444),
        b"AP4X" => Some(K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_4444_XQ),
        _ => None,
    }
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

/// MPEG-4 Part 2 (Visual / ASP / SP) video decoder via VideoToolbox.
///
/// Decode-only: VideoToolbox exposes an MPEG-4 Part 2 *decoder*
/// (`kCMVideoCodecType_MPEG4Video` = `'mp4v'`) — historically used for
/// DivX / Xvid playback on macOS — but no MPEG-4 Part 2 compression
/// session, so there is no matching `make_mpeg4_part_two_encoder`.
///
/// Input is an MPEG-4 Part 2 elementary stream (no container framing). The
/// `FrameSplit::Mpeg4PartTwoEs` framer splits the buffer on VOP start codes
/// (`00 00 01 B6`) into per-VOP access units, attaching any leading VOS /
/// Visual Object / VO / VOL / GOV headers to the first VOP so the embedded
/// VOL travels with it.
///
/// Codec id: `CodecId::new("mpeg4")` (matching the workspace's MPEG-4
/// Part 2 software codec). Note this is **not** H.264 — H.264 is MPEG-4
/// Part 10 and uses `kCMVideoCodecType_H264` (`'avc1'`).
///
/// ## VOL→ESDS extension-atom path (round 7)
///
/// VideoToolbox's MPEG-4 Part 2 decoder enforces that the VOL configuration
/// be supplied via the format description extensions (the ESDS
/// `DecoderSpecificInfo` / `kCMFormatDescriptionExtension_*` keys), *not*
/// extracted from the elementary stream as it would be for MPEG-2. The
/// round-7 path closes that gap: on the first packet, `BlobDecoder` calls
/// [`extract_mpeg4_part_two_vol`] to harvest the configuration prefix
/// (everything from offset 0 up to but not including the first VOP start
/// code `00 00 01 B6`), wraps the bytes in a complete ESDS descriptor via
/// [`build_mpeg4_part_two_esds`], and supplies the resulting blob to
/// `CMVideoFormatDescriptionCreate` under the
/// `SampleDescriptionExtensionAtoms` → `"esds"` key.
///
/// On hosts where the bitstream prefix alone would have been sufficient,
/// the extra extension atom is harmless. On hosts that require the ESDS
/// shape, this is the difference between hardware decode and a
/// `kVTVideoDecoderBadDataErr` fallback to the pure-Rust impl.
///
/// If the first packet has no configuration prefix to extract (e.g. a VOP
/// start code at offset 0, or no VOP start code at all), the extractor
/// returns `None` and the decoder reverts to the round-6 plain
/// `(codec_type, width, height)` path. The pure-Rust MPEG-4 Part 2 decoder
/// remains in the registry as a lower-priority fallback for any host
/// where session creation still fails.
pub fn make_mpeg4_part_two_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    BlobDecoder::make_with_framer(
        "mpeg4",
        K_CM_VIDEO_CODEC_TYPE_MPEG4_VIDEO,
        FrameSplit::Mpeg4PartTwoEs,
        params,
    )
}

/// AV1 video decoder via VideoToolbox.
///
/// Decode-only here: VideoToolbox exposes an AV1 *decoder*
/// (`kCMVideoCodecType_AV1` = `'av01'`) on macOS 11+, with hardware
/// acceleration gated to Apple Silicon M3+ chips. On older hardware
/// (Intel Macs, M1 / M2) VideoToolbox falls back to its internal
/// software AV1 path on macOS versions where that path exists, or
/// returns a non-zero `OSStatus` at session creation otherwise. The
/// registry's SW fallback to the pure-Rust `oxideav-av1` decoder
/// covers the latter case.
///
/// ## Framing
///
/// AV1 access units are container-framed in IVF / Matroska / MP4 /
/// WebM / RTP. Each `Packet` carries one AV1 temporal unit (one or
/// more OBUs that together compose a single decoded frame) end-to-end,
/// so `FrameSplit::Whole` is correct here — there is no in-codec
/// access-unit splitter analogous to MPEG-2's or MPEG-4 Part 2's start
/// code carve. (AV1 OBUs do have `obu_size` fields, but the demuxer
/// has already produced exactly one temporal unit per `Packet`.)
///
/// ## Configuration record (av1C) — round 10 path
///
/// AV1 in MP4 / Matroska / WebM carries an `av1C` configuration record
/// (per the AV1 ISO Base Media File Format Binding Specification §2.3)
/// whose body is the [`AV1CodecConfigurationRecord`] — a 4-byte fixed
/// header (`marker`, `version`, `seq_profile`, `seq_level_idx_0`,
/// `seq_tier_0`, colour-config flags, `initial_presentation_delay_*`)
/// followed by a `configOBUs[]` field that SHALL contain at most one
/// Sequence Header OBU as the first OBU.
///
/// On hosts where VT requires the Sequence Header out-of-band, supplying
/// the av1C blob via `kCMFormatDescriptionExtension_SampleDescription`
/// `ExtensionAtoms = { "av1C": CFData }` is the same pattern wired in
/// round 7 for MPEG-4 Part 2's ESDS. The round-10 path closes that gap:
/// on the first packet, [`BlobDecoder`] calls
/// [`extract_av1_sequence_header_obu`] to harvest the OBU, builds the
/// av1C record via [`build_av1c_config_record`] (with the bit-fields
/// parsed by [`parse_av1_seq_header_fields`]; the OBU itself is included
/// verbatim in `configOBUs`), and supplies the blob to
/// `CMVideoFormatDescriptionCreate`.
///
/// If the first packet has no Sequence Header OBU (e.g. mid-stream
/// random-access entry where the decoder context already exists) the
/// extractor returns `None` and the decoder reverts to the round-8 plain
/// `(codec_type, width, height)` path. The pure-Rust `oxideav-av1`
/// decoder remains in the registry as a lower-priority fallback for any
/// host where session creation still fails.
///
/// ## Codec id
///
/// `CodecId::new("av1")`, matching the workspace's pure-Rust `oxideav-av1`
/// codec id.
pub fn make_av1_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    BlobDecoder::make_with_framer(
        "av1",
        K_CM_VIDEO_CODEC_TYPE_AV1,
        FrameSplit::Av1Whole,
        params,
    )
}

/// VVC (H.266) video decoder via VideoToolbox.
///
/// Decode-only: VideoToolbox first exposes a VVC *decoder*
/// (`kCMVideoCodecType_VVC` = `'vvc1'`) on macOS 26+ for Apple Silicon
/// M3+ hardware, with VT-internal software fallback on older OS versions
/// where it exists; no VVC compression session is exposed at the time of
/// this round, so there is no matching `make_vvc_encoder`.
///
/// ## Framing
///
/// VVC input is an Annex-B elementary stream (per H.266 Annex B). The
/// `FrameSplit::VvcEs` framer splits the buffer on AUD / PH / VCL
/// boundaries (see [`split_vvc_access_units`]) into per-access-unit
/// payloads, attaching any leading DCI / OPI / VPS / SPS / PPS / PREFIX_APS
/// NAL units to the first access unit so the configuration travels with it.
///
/// ## Configuration record (vvcC) — extension-atom path
///
/// On hosts where VT requires the parameter sets out-of-band rather than
/// extracted from the bitstream prefix, `BlobDecoder` calls
/// [`extract_vvc_config_prefix`] on the first packet to harvest the
/// leading non-VCL NAL units, wraps them in a `VvcDecoderConfigurationRecord`
/// via [`build_vvc_decoder_config_record`] (per ISO/IEC 14496-15
/// §11.2.4.2.2 — `ptl_present_flag = 0` form, `LengthSizeMinusOne = 3`),
/// and supplies the blob to `CMVideoFormatDescriptionCreate` via
/// `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms = {
/// "vvcC": CFData }`. This is the same pattern wired in round 7 for
/// MPEG-4 Part 2's ESDS and round 10 for AV1's av1C.
///
/// If the first packet contains no leading non-VCL NAL units (e.g. a
/// mid-stream random-access entry where the decoder context already
/// exists), [`extract_vvc_config_prefix`] returns `None` and the decoder
/// reverts to the bare `(codec_type, width, height)` path. The pure-Rust
/// `oxideav-h266` decoder remains in the registry as a lower-priority
/// fallback for any host where session creation still fails.
///
/// ## Codec id
///
/// `CodecId::new("h266")`, matching the workspace's pure-Rust
/// `oxideav-h266` codec id.
pub fn make_vvc_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    BlobDecoder::make_with_framer("h266", K_CM_VIDEO_CODEC_TYPE_VVC, FrameSplit::VvcEs, params)
}

#[cfg(test)]
mod tests {
    use super::{
        build_av1c_config_record, build_mpeg4_part_two_esds, build_vvc_decoder_config_record,
        extract_av1_sequence_header_obu, extract_mpeg4_part_two_vol, extract_vvc_config_prefix,
        extract_vvc_nals_of_type, parse_av1_seq_header_fields, split_mpeg2_access_units,
        split_mpeg4_part_two_access_units, split_vvc_access_units, split_vvc_nal_units,
        vvc_is_vcl_nut, vvc_nal_unit_type, Av1SeqHeaderFields, VVC_NUT_AUD, VVC_NUT_DCI,
        VVC_NUT_IDR_W_RADL, VVC_NUT_OPI, VVC_NUT_PH, VVC_NUT_PPS, VVC_NUT_PREFIX_APS, VVC_NUT_SPS,
        VVC_NUT_TRAIL, VVC_NUT_VPS,
    };

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

    // ── MPEG-4 Part 2 splitter ───────────────────────────────────────────────

    // MPEG-4 Part 2 start codes (ISO/IEC 14496-2):
    //   B0 = VOS (Visual Object Sequence), B5 = Visual Object, 01..1F = VO,
    //   20..2F = VOL (Video Object Layer), B3 = GOV (Group of VOP), B6 = VOP,
    //   B2 = user data.
    const VOS: &[u8] = &[0x00, 0x00, 0x01, 0xB0, 0xAA]; // VOS start + profile byte
    const VOB: &[u8] = &[0x00, 0x00, 0x01, 0xB5, 0xBB]; // Visual Object start
    const VOL: &[u8] = &[0x00, 0x00, 0x01, 0x20, 0xCC]; // VOL (one of 20..2F)
    const GOV: &[u8] = &[0x00, 0x00, 0x01, 0xB3, 0xDD]; // GOV start
    const VOP: &[u8] = &[0x00, 0x00, 0x01, 0xB6, 0xEE]; // VOP start
    const M4_SLICE: &[u8] = &[0x00, 0x00, 0x01, 0x01, 0xFF];

    #[test]
    fn mpeg4_single_vop_with_headers() {
        // VOS + VOB + VOL + GOV + VOP + slice → one access unit covering all.
        let buf = cat(&[VOS, VOB, VOL, GOV, VOP, M4_SLICE]);
        let units = split_mpeg4_part_two_access_units(&buf);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0], &buf[..]);
    }

    #[test]
    fn mpeg4_two_vops_first_keeps_headers() {
        // VOS + VOL + VOP1 + slice + VOP2 + slice → two access units; the
        // first inherits the leading VOS / VOL headers.
        let vop1 = cat(&[VOS, VOL, VOP, M4_SLICE]);
        let vop2 = cat(&[VOP, M4_SLICE]);
        let buf = cat(&[&vop1, &vop2]);
        let units = split_mpeg4_part_two_access_units(&buf);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0], &vop1[..]);
        assert_eq!(units[1], &vop2[..]);
    }

    #[test]
    fn mpeg4_no_vop_start_code_returns_whole() {
        // A buffer with only VOS + VOL (no VOP) is handed through intact.
        let buf = cat(&[VOS, VOL]);
        let units = split_mpeg4_part_two_access_units(&buf);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0], &buf[..]);
    }

    #[test]
    fn mpeg4_empty_buffer_yields_nothing() {
        assert!(split_mpeg4_part_two_access_units(&[]).is_empty());
    }

    #[test]
    fn mpeg4_does_not_confuse_other_start_codes() {
        // GOV (B3) and VOS (B0) are not VOP starts — only B6 is. A buffer
        // with leading GOV+VOS but no VOP must return the whole buffer (no
        // VOP found path), not split mid-stream on the non-VOP codes.
        let buf = cat(&[GOV, VOS, &[0x11, 0x22]]);
        let units = split_mpeg4_part_two_access_units(&buf);
        assert_eq!(units.len(), 1, "non-VOP start codes must not trigger split");
        assert_eq!(units[0], &buf[..]);
    }

    // ── MPEG-4 Part 2 VOL extraction ─────────────────────────────────────────

    #[test]
    fn mpeg4_extract_vol_returns_prefix_before_vop() {
        // VOS + VOL + VOP + slice → VOL extraction returns VOS + VOL only,
        // dropping the VOP and everything after it.
        let prefix = cat(&[VOS, VOL]);
        let buf = cat(&[&prefix, VOP, M4_SLICE]);
        let vol = extract_mpeg4_part_two_vol(&buf).expect("vol present");
        assert_eq!(vol, &prefix[..]);
    }

    #[test]
    fn mpeg4_extract_vol_includes_gov_user_data() {
        // VOS + VOL + GOV + VOP → VOL extraction returns VOS + VOL + GOV.
        let prefix = cat(&[VOS, VOL, GOV]);
        let buf = cat(&[&prefix, VOP, M4_SLICE]);
        let vol = extract_mpeg4_part_two_vol(&buf).expect("vol present");
        assert_eq!(vol, &prefix[..]);
    }

    #[test]
    fn mpeg4_extract_vol_none_when_no_vop() {
        // A buffer with only the headers (no VOP start) has no extraction
        // boundary — return None and let the caller skip the ESDS path.
        let buf = cat(&[VOS, VOL]);
        assert!(extract_mpeg4_part_two_vol(&buf).is_none());
    }

    #[test]
    fn mpeg4_extract_vol_none_when_starts_with_vop() {
        // A buffer that opens with a VOP start code has no preceding
        // configuration to extract.
        let buf = cat(&[VOP, M4_SLICE]);
        assert!(extract_mpeg4_part_two_vol(&buf).is_none());
    }

    #[test]
    fn mpeg4_extract_vol_empty_buffer() {
        assert!(extract_mpeg4_part_two_vol(&[]).is_none());
    }

    // ── MPEG-4 Part 2 ESDS construction ──────────────────────────────────────

    /// Decode the 4-byte BER length form `build_mpeg4_part_two_esds` always
    /// emits (always 4 bytes for stable parsing).
    fn read_ber_length_4(buf: &[u8]) -> u32 {
        let mut v = 0u32;
        for b in &buf[..4] {
            v = (v << 7) | (b & 0x7F) as u32;
        }
        v
    }

    #[test]
    fn esds_has_full_box_header() {
        // 4-byte version/flags prefix = 0.
        let esds = build_mpeg4_part_two_esds(&[0xAA, 0xBB]);
        assert!(esds.len() >= 4);
        assert_eq!(&esds[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn esds_es_descriptor_tag_0x03() {
        // Byte 4 (right after the FullBox header) is the ES_Descriptor tag.
        let esds = build_mpeg4_part_two_esds(&[0xAA]);
        assert_eq!(esds[4], 0x03);
        // Bytes 5..9 are the BER length; bytes 9..12 are ES_ID(2) + flags(1)
        // (all zero in our build).
        assert_eq!(&esds[9..12], &[0, 0, 0]);
    }

    #[test]
    fn esds_decoder_config_descriptor_tag_and_oti() {
        // After the ES_Descriptor's 3-byte ES_ID+flags, the next descriptor
        // is the DecoderConfigDescriptor (tag 0x04). Then 1 byte ObjectType
        // (0x20 = MPEG-4 Visual) and 1 byte streamType<<2|upStream|reserved
        // = (4<<2)|0|1 = 0x11.
        let esds = build_mpeg4_part_two_esds(&[0xAA]);
        // FullBox(4) + ESD tag(1) + ESD len(4) + ES_ID+flags(3) = 12
        let dcd_tag_pos =
            4 /* FullBox */ + 1 /* ESD tag */ + 4 /* ESD len */ + 3 /* ES_ID+flags */;
        assert_eq!(esds[dcd_tag_pos], 0x04);
        let dcd_len_pos = dcd_tag_pos + 1;
        let _dcd_len = read_ber_length_4(&esds[dcd_len_pos..dcd_len_pos + 4]);
        let oti_pos = dcd_len_pos + 4;
        assert_eq!(esds[oti_pos], 0x20, "ObjectTypeIndication = MPEG-4 Visual");
        assert_eq!(
            esds[oti_pos + 1],
            0x11,
            "streamType=VisualStream + reserved bit"
        );
    }

    #[test]
    fn esds_decoder_specific_info_carries_vol() {
        // Inside DecoderConfigDescriptor at the 13-byte fixed header offset,
        // the DecoderSpecificInfo (tag 0x05) contains the VOL bytes verbatim.
        let vol: &[u8] = &[0x00, 0x00, 0x01, 0x20, 0xAA, 0xBB, 0xCC];
        let esds = build_mpeg4_part_two_esds(vol);
        let dsi_tag_pos =
            4 /* FullBox */ + 1 /* ESD tag */ + 4 /* ESD len */ + 3 /* ES_ID+flags */
            + 1 /* DCD tag */ + 4 /* DCD len */ + 13 /* DCD fixed */;
        assert_eq!(esds[dsi_tag_pos], 0x05, "DecoderSpecificInfo tag");
        let dsi_len = read_ber_length_4(&esds[dsi_tag_pos + 1..dsi_tag_pos + 5]);
        assert_eq!(dsi_len as usize, vol.len());
        let dsi_payload_pos = dsi_tag_pos + 5;
        assert_eq!(&esds[dsi_payload_pos..dsi_payload_pos + vol.len()], vol);
    }

    #[test]
    fn esds_sl_config_descriptor_predefined_2() {
        // The SLConfigDescriptor (tag 0x06) sits after the DCD; its 1-byte
        // payload is `predefined = 2` (mp4 file SL config).
        let esds = build_mpeg4_part_two_esds(&[0xAA]);
        // SLC sits at the end; find tag 0x06 from the back.
        let slc_pos = esds
            .iter()
            .rposition(|&b| b == 0x06)
            .expect("SLConfigDescriptor tag present");
        let slc_payload = esds[slc_pos + 5];
        assert_eq!(slc_payload, 0x02);
    }

    // ── AV1 codec-type identifier ────────────────────────────────────────────

    /// `kCMVideoCodecType_AV1` must match the four-character code `'av01'`
    /// (the same fourcc carried in the AV1 ISOBMFF `av01` sample entry).
    /// The constant is `0x61763031` = `b'a' b'v' b'0' b'1'`.
    #[test]
    fn av1_codec_type_is_av01_fourcc() {
        let expected = u32::from_be_bytes(*b"av01");
        assert_eq!(super::K_CM_VIDEO_CODEC_TYPE_AV1, expected);
        assert_eq!(super::K_CM_VIDEO_CODEC_TYPE_AV1, 0x6176_3031);
    }

    // ── ProRes profile selection (round 9) ────────────────────────────────

    /// Every ProRes codec-type constant must equal its documented fourcc.
    /// `'apco'` = ProRes Proxy, `'apcs'` = LT, `'apcn'` = 422,
    /// `'apch'` = HQ, `'ap4h'` = 4444, `'ap4x'` = 4444 XQ.
    #[test]
    fn prores_codec_type_constants_match_fourcc() {
        assert_eq!(
            super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_PROXY,
            u32::from_be_bytes(*b"apco")
        );
        assert_eq!(
            super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_LT,
            u32::from_be_bytes(*b"apcs")
        );
        assert_eq!(
            super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422,
            u32::from_be_bytes(*b"apcn")
        );
        assert_eq!(
            super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_HQ,
            u32::from_be_bytes(*b"apch")
        );
        assert_eq!(
            super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_4444,
            u32::from_be_bytes(*b"ap4h")
        );
        assert_eq!(
            super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_4444_XQ,
            u32::from_be_bytes(*b"ap4x")
        );
    }

    /// `prores_codec_type_for_tag` must dispatch every ProRes fourcc to
    /// the matching `kCMVideoCodecType_AppleProRes*` constant.
    #[test]
    fn prores_tag_dispatch_each_fourcc() {
        use oxideav_core::CodecTag;
        for (tag_bytes, expected) in [
            (b"apco", super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_PROXY),
            (b"apcs", super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_LT),
            (b"apcn", super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422),
            (b"apch", super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_422_HQ),
            (b"ap4h", super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_4444),
            (b"ap4x", super::K_CM_VIDEO_CODEC_TYPE_APPLE_PRORES_4444_XQ),
        ] {
            let tag = CodecTag::fourcc(tag_bytes);
            let got = super::prores_codec_type_for_tag(Some(&tag));
            assert_eq!(
                got,
                Some(expected),
                "tag {:?} → expected codec type 0x{expected:08x}",
                std::str::from_utf8(tag_bytes).unwrap()
            );
        }
    }

    /// Unknown / non-fourcc tags must return `None` so the factory falls
    /// back to its default (ProRes 422).
    #[test]
    fn prores_tag_dispatch_falls_back_on_unknown() {
        use oxideav_core::CodecTag;
        assert_eq!(super::prores_codec_type_for_tag(None), None);
        // Non-ProRes fourcc.
        let xvid = CodecTag::fourcc(b"xvid");
        assert_eq!(super::prores_codec_type_for_tag(Some(&xvid)), None);
        // Non-fourcc tag variant.
        let mkv = CodecTag::matroska("V_PRORES");
        assert_eq!(super::prores_codec_type_for_tag(Some(&mkv)), None);
    }

    // ── AV1 av1C extension-atom path (round 10) ──────────────────────────────

    /// Build one OBU header byte with `(obu_type, ext_flag, has_size_field)`
    /// per AV1 spec §5.3.2. obu_forbidden_bit and obu_reserved_1bit are 0.
    fn av1_obu_header(obu_type: u8, ext_flag: bool, has_size_field: bool) -> u8 {
        let mut b = (obu_type & 0x0F) << 3;
        if ext_flag {
            b |= 0x04;
        }
        if has_size_field {
            b |= 0x02;
        }
        b
    }

    /// Encode a uleb128 value to bytes (canonical, terminating byte has
    /// MSB clear). Used to build synthetic OBU streams for the tests.
    fn av1_uleb128(mut value: u32) -> Vec<u8> {
        if value == 0 {
            return vec![0];
        }
        let mut out = Vec::new();
        while value > 0 {
            let mut b = (value & 0x7F) as u8;
            value >>= 7;
            if value > 0 {
                b |= 0x80;
            }
            out.push(b);
        }
        out
    }

    /// Build a synthetic AV1 temporal unit of `[Temporal Delimiter] +
    /// [Sequence Header obu_type=1 with given payload] + [Frame Header
    /// obu_type=3 with empty payload]`. Used to exercise the OBU walker.
    fn av1_synth_temporal_unit(seq_header_payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        // OBU_TEMPORAL_DELIMITER (type 2), has_size_field=1, payload empty.
        out.push(av1_obu_header(2, false, true));
        out.extend_from_slice(&av1_uleb128(0));
        // OBU_SEQUENCE_HEADER (type 1), has_size_field=1, payload as given.
        out.push(av1_obu_header(1, false, true));
        out.extend_from_slice(&av1_uleb128(seq_header_payload.len() as u32));
        out.extend_from_slice(seq_header_payload);
        // OBU_FRAME_HEADER (type 3), has_size_field=1, payload empty.
        out.push(av1_obu_header(3, false, true));
        out.extend_from_slice(&av1_uleb128(0));
        out
    }

    /// `extract_av1_sequence_header_obu` returns the full Sequence Header
    /// OBU (header byte + uleb128 size + payload) — bytes exactly as they
    /// appear in the input temporal unit.
    #[test]
    fn av1_extract_sequence_header_obu_returns_full_obu_bytes() {
        let sh_payload = [0xAA, 0xBB, 0xCC];
        let tu = av1_synth_temporal_unit(&sh_payload);
        let sh = extract_av1_sequence_header_obu(&tu).expect("seq header present");
        // OBU header byte = type=1, has_size=1 → 0x0A.
        assert_eq!(sh[0], 0x0A);
        // Then uleb128(3) = single byte 0x03.
        assert_eq!(sh[1], 0x03);
        // Then payload verbatim.
        assert_eq!(&sh[2..], &sh_payload);
        // Total = 1 (header) + 1 (uleb) + 3 (payload) = 5 bytes.
        assert_eq!(sh.len(), 5);
    }

    /// When the temporal unit carries no Sequence Header OBU,
    /// `extract_av1_sequence_header_obu` returns `None` and the decoder
    /// falls back to the round-8 (codec_type, width, height) path.
    #[test]
    fn av1_extract_sequence_header_obu_none_when_absent() {
        let mut tu = Vec::new();
        // Only a Temporal Delimiter and a Frame Header — no Sequence Header.
        tu.push(av1_obu_header(2, false, true));
        tu.extend_from_slice(&av1_uleb128(0));
        tu.push(av1_obu_header(3, false, true));
        tu.extend_from_slice(&av1_uleb128(0));
        assert!(extract_av1_sequence_header_obu(&tu).is_none());
    }

    /// An obu_forbidden_bit set to 1 means the buffer is not a valid OBU
    /// stream — the extractor must refuse rather than mis-walk it.
    #[test]
    fn av1_extract_sequence_header_obu_rejects_forbidden_bit() {
        // Header with obu_forbidden_bit (MSB) set.
        let buf = [0x80u8, 0x00];
        assert!(extract_av1_sequence_header_obu(&buf).is_none());
    }

    /// `obu_has_size_field = 0` means the low-overhead bitstream format
    /// isn't in use; the walker can't safely advance and returns `None`.
    #[test]
    fn av1_extract_sequence_header_obu_requires_size_field() {
        // OBU_SEQUENCE_HEADER (type 1) with has_size_field=0.
        let buf = [av1_obu_header(1, false, false), 0xAA];
        assert!(extract_av1_sequence_header_obu(&buf).is_none());
    }

    /// A Sequence Header OBU whose payload's uleb128 size points past the
    /// buffer end must be rejected (no out-of-bounds read).
    #[test]
    fn av1_extract_sequence_header_obu_rejects_truncated_payload() {
        let mut buf = Vec::new();
        buf.push(av1_obu_header(1, false, true));
        buf.extend_from_slice(&av1_uleb128(99)); // claims 99 bytes...
        buf.extend_from_slice(&[0xAA, 0xBB]); // ...but only 2 follow.
        assert!(extract_av1_sequence_header_obu(&buf).is_none());
    }

    /// `build_av1c_config_record` lays out the av1C body per AV1 ISOBMFF
    /// binding spec §2.3.3: first byte `marker=1, version=1` (= `0x81`),
    /// then 3 bytes of packed bit-fields, then `configOBUs` containing
    /// the Sequence Header OBU verbatim.
    #[test]
    fn av1c_marker_and_version_byte_is_0x81() {
        let sh = [0x0A, 0x01, 0x00]; // minimal SH OBU (header + uleb128(1) + 1 byte payload).
        let av1c = build_av1c_config_record(&sh);
        assert_eq!(
            av1c[0], 0x81,
            "marker=1 in bit 7, version=1 in bits 6..0 = 0x81"
        );
    }

    /// av1C header byte 3 must be `0` — reserved(3) + iPDP(1)=0 +
    /// reserved(4). We never signal initial_presentation_delay here.
    #[test]
    fn av1c_byte_3_is_zero_reserved() {
        let sh = [0x0A, 0x01, 0x00];
        let av1c = build_av1c_config_record(&sh);
        assert!(av1c.len() >= 4);
        assert_eq!(av1c[3], 0);
    }

    /// av1C `configOBUs` field (bytes 4..) must equal the Sequence Header
    /// OBU bytes passed in, verbatim. The binding spec §2.3.4 mandates the
    /// Sequence Header be the first OBU in `configOBUs` when present.
    #[test]
    fn av1c_configobus_includes_sequence_header_obu_verbatim() {
        let sh: Vec<u8> = (0..7).collect();
        let av1c = build_av1c_config_record(&sh);
        assert_eq!(&av1c[4..], &sh);
    }

    /// Header bit-fields for `seq_profile = 1, seq_level_idx = 5` (a
    /// reduced-still-picture-header SH) must land in the right bit
    /// positions of byte 1. byte1 = (1<<5) | 5 = 0x25.
    #[test]
    fn av1c_byte_1_packs_seq_profile_and_level() {
        // Build a synthetic SH payload: seq_profile=1 (3 bits), still=0,
        // reduced_still=1, seq_level_idx[0]=5 (5 bits). MSB-packed:
        // 001 0 1 00101 = 00101 00101 padded with zeros.
        // Bits: 0_0_1 (profile=1) | 0 (still) | 1 (reduced) | 00101 (level)
        // = 0010_1001_01 + padding → 0x29, 0x40.
        let sh_payload = [0b0010_1001u8, 0b0100_0000u8];
        let mut tu = Vec::new();
        tu.push(av1_obu_header(1, false, true));
        tu.extend_from_slice(&av1_uleb128(sh_payload.len() as u32));
        tu.extend_from_slice(&sh_payload);

        let sh_obu = extract_av1_sequence_header_obu(&tu).expect("seq header present");
        let av1c = build_av1c_config_record(sh_obu);
        // byte 1: profile in bits 7..5, level in bits 4..0.
        assert_eq!(av1c[1] >> 5, 1, "seq_profile = 1");
        assert_eq!(av1c[1] & 0x1F, 5, "seq_level_idx_0 = 5");
    }

    /// `parse_av1_seq_header_fields` on a reduced-still-picture-header
    /// payload must recover `seq_profile` and `seq_level_idx_0`, with
    /// `seq_tier_0 = 0` per the spec.
    #[test]
    fn av1_seq_header_fields_reduced_still_picture_header() {
        // profile=2, still=1, reduced=1, level=12.
        // 010 1 1 01100 = 0101_1011_00 + padding → 0x5B, 0x00.
        let payload = [0b0101_1011u8, 0b0000_0000u8];
        let f = parse_av1_seq_header_fields(&payload);
        assert_eq!(f.seq_profile, 2);
        assert_eq!(f.seq_level_idx_0, 12);
        assert_eq!(f.seq_tier_0, 0);
    }

    /// Defaults must be 8-bit 4:2:0 main-profile when the payload is empty
    /// or unparseable — `configOBUs` then carries the SH OBU verbatim.
    #[test]
    fn av1_seq_header_fields_defaults_on_empty() {
        let f = parse_av1_seq_header_fields(&[]);
        let d = Av1SeqHeaderFields::defaults();
        assert_eq!(f, d);
        assert_eq!(d.high_bitdepth, 0);
        assert_eq!(d.chroma_subsampling_x, 1);
        assert_eq!(d.chroma_subsampling_y, 1);
    }

    // ── VVC (H.266) helpers ──────────────────────────────────────────────────
    //
    // Build a 2-byte VVC NAL unit header with the given nal_unit_type. The
    // top two bits (forbidden_zero_bit + nuh_reserved_zero_bit) are 0;
    // nuh_layer_id is 0; nuh_temporal_id_plus1 is 1. This is the minimum
    // valid header shape per H.266 §7.3.1.2 / §7.4.2.2.
    fn vvc_hdr(nut: u8) -> [u8; 2] {
        [0x00, ((nut & 0x1F) << 3) | 0x01]
    }

    // Build a single Annex-B byte-stream NAL unit: 3-byte start code prefix
    // + 2-byte header + an opaque RBSP byte. The 4-byte form is exercised
    // in a dedicated test.
    fn vvc_annex_b_nal(nut: u8, rbsp_byte: u8) -> Vec<u8> {
        let hdr = vvc_hdr(nut);
        vec![0x00, 0x00, 0x01, hdr[0], hdr[1], rbsp_byte]
    }

    #[test]
    fn vvc_nal_unit_type_extracts_5_bit_field() {
        for &nut in &[
            VVC_NUT_TRAIL,
            VVC_NUT_IDR_W_RADL,
            VVC_NUT_VPS,
            VVC_NUT_SPS,
            VVC_NUT_PPS,
            VVC_NUT_PH,
            VVC_NUT_AUD,
        ] {
            let h = vvc_hdr(nut);
            assert_eq!(vvc_nal_unit_type(&h), Some(nut), "round-trip nut {nut}");
        }
    }

    #[test]
    fn vvc_nal_unit_type_rejects_nonzero_top_bits() {
        // forbidden_zero_bit set.
        assert_eq!(vvc_nal_unit_type(&[0x80, 0x08]), None);
        // nuh_reserved_zero_bit set.
        assert_eq!(vvc_nal_unit_type(&[0x40, 0x08]), None);
    }

    #[test]
    fn vvc_nal_unit_type_short_input_is_none() {
        assert_eq!(vvc_nal_unit_type(&[]), None);
        assert_eq!(vvc_nal_unit_type(&[0x00]), None);
    }

    #[test]
    fn vvc_vcl_classification_matches_table_5() {
        // VCL types: 0..=11 per H.266 Table 5.
        for nut in 0..=11u8 {
            assert!(vvc_is_vcl_nut(nut), "{nut} should be VCL");
        }
        // Non-VCL: parameter sets, PH, AUD, SEI, etc.
        for &nut in &[
            VVC_NUT_OPI,
            VVC_NUT_DCI,
            VVC_NUT_VPS,
            VVC_NUT_SPS,
            VVC_NUT_PPS,
            VVC_NUT_PREFIX_APS,
            VVC_NUT_PH,
            VVC_NUT_AUD,
        ] {
            assert!(!vvc_is_vcl_nut(nut), "{nut} should not be VCL");
        }
    }

    #[test]
    fn vvc_split_nal_units_three_byte_start_codes() {
        // SPS + PPS + IDR — three NALs with 3-byte start codes back-to-back.
        let mut buf = Vec::new();
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0xAA));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_PPS, 0xBB));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_IDR_W_RADL, 0xCC));

        let nals = split_vvc_nal_units(&buf);
        assert_eq!(nals.len(), 3);
        // Each NAL is exactly the 2-byte header + 1 RBSP byte = 3 bytes.
        for (_, len) in &nals {
            assert_eq!(*len, 3);
        }
        // First NAL is SPS.
        let (off0, len0) = nals[0];
        assert_eq!(
            vvc_nal_unit_type(&buf[off0..off0 + len0]),
            Some(VVC_NUT_SPS)
        );
    }

    #[test]
    fn vvc_split_nal_units_four_byte_start_codes() {
        // VPS + SPS with 4-byte start codes (00 00 00 01 … 00 00 00 01 …).
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        buf.extend_from_slice(&vvc_hdr(VVC_NUT_VPS));
        buf.push(0x11);
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        buf.extend_from_slice(&vvc_hdr(VVC_NUT_SPS));
        buf.push(0x22);
        let nals = split_vvc_nal_units(&buf);
        assert_eq!(nals.len(), 2, "two NALs expected: {nals:?}");
        let (off0, len0) = nals[0];
        let (off1, len1) = nals[1];
        assert_eq!(
            vvc_nal_unit_type(&buf[off0..off0 + len0]),
            Some(VVC_NUT_VPS)
        );
        assert_eq!(
            vvc_nal_unit_type(&buf[off1..off1 + len1]),
            Some(VVC_NUT_SPS)
        );
    }

    #[test]
    fn vvc_extract_nals_of_type_returns_only_matching() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_VPS, 0xA0));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0xA1));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0xA2));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_PPS, 0xA3));
        let sps = extract_vvc_nals_of_type(&buf, VVC_NUT_SPS);
        assert_eq!(sps.len(), 2);
        assert_eq!(sps[0][2], 0xA1);
        assert_eq!(sps[1][2], 0xA2);
        let pps = extract_vvc_nals_of_type(&buf, VVC_NUT_PPS);
        assert_eq!(pps.len(), 1);
        let none = extract_vvc_nals_of_type(&buf, VVC_NUT_AUD);
        assert!(none.is_empty());
    }

    #[test]
    fn vvc_extract_config_prefix_stops_at_first_vcl() {
        // VPS + SPS + PPS + IDR → prefix is VPS+SPS+PPS bytes.
        let mut buf = Vec::new();
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_VPS, 0x11));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0x22));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_PPS, 0x33));
        let vcl_off = buf.len();
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_IDR_W_RADL, 0x44));
        let prefix = extract_vvc_config_prefix(&buf).expect("prefix present");
        assert_eq!(prefix.len(), vcl_off);
        // Sanity: it round-trips through the NAL walker as 3 NALs.
        assert_eq!(split_vvc_nal_units(prefix).len(), 3);
    }

    #[test]
    fn vvc_extract_config_prefix_none_when_starts_with_vcl() {
        let buf = vvc_annex_b_nal(VVC_NUT_IDR_W_RADL, 0x77);
        assert!(extract_vvc_config_prefix(&buf).is_none());
    }

    #[test]
    fn vvc_extract_config_prefix_none_on_empty_or_no_start_codes() {
        assert!(extract_vvc_config_prefix(&[]).is_none());
        assert!(extract_vvc_config_prefix(&[1, 2, 3, 4]).is_none());
    }

    #[test]
    fn vvc_decoder_config_record_byte_0_is_0xfe() {
        // Reserved(5)=0b11111 | LengthSizeMinusOne(2)=3 | ptl_present(1)=0
        // → 0b11111110 = 0xFE.
        let mut prefix = Vec::new();
        prefix.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0xAA));
        let r = build_vvc_decoder_config_record(&prefix);
        assert_eq!(r[0], 0xFE);
    }

    #[test]
    fn vvc_decoder_config_record_lists_only_present_arrays() {
        // VPS + SPS + PPS prefix → num_of_arrays = 3.
        let mut prefix = Vec::new();
        prefix.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_VPS, 0x10));
        prefix.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0x20));
        prefix.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_PPS, 0x30));
        let r = build_vvc_decoder_config_record(&prefix);
        assert_eq!(r[0], 0xFE);
        assert_eq!(r[1], 3, "expected num_of_arrays=3 for VPS+SPS+PPS");
    }

    #[test]
    fn vvc_decoder_config_record_array_layout() {
        // VPS prefix only — verify the first array header byte equals
        // NAL_unit_type (array_completeness=0, reserved=0).
        let mut prefix = Vec::new();
        prefix.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_VPS, 0xCC));
        let r = build_vvc_decoder_config_record(&prefix);
        // Header byte 0 = 0xFE, byte 1 = num_of_arrays = 1.
        assert_eq!(r[0], 0xFE);
        assert_eq!(r[1], 1);
        // Array entry: byte 2 = NAL_unit_type (with top 3 bits zero).
        assert_eq!(r[2] & 0x1F, VVC_NUT_VPS);
        // Bytes 3..5 = num_nalus = 1 (BE u16).
        assert_eq!(&r[3..5], &[0x00, 0x01]);
        // Bytes 5..7 = nal_unit_length = 3 (header + 1 RBSP byte).
        assert_eq!(&r[5..7], &[0x00, 0x03]);
        // Bytes 7..10 = NAL bytes (header + 0xCC).
        let hdr = vvc_hdr(VVC_NUT_VPS);
        assert_eq!(r[7], hdr[0]);
        assert_eq!(r[8], hdr[1]);
        assert_eq!(r[9], 0xCC);
    }

    #[test]
    fn vvc_decoder_config_record_dci_array_omits_num_nalus() {
        // Per §11.2.4.2.2: DCI_NUT and OPI_NUT arrays omit the `num_nalus`
        // u16 field. Verify the DCI array body is shorter than a VPS one.
        let mut prefix_dci = Vec::new();
        prefix_dci.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_DCI, 0xDD));
        let r_dci = build_vvc_decoder_config_record(&prefix_dci);

        let mut prefix_vps = Vec::new();
        prefix_vps.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_VPS, 0xEE));
        let r_vps = build_vvc_decoder_config_record(&prefix_vps);

        // VPS adds 2 bytes (num_nalus u16) over DCI.
        assert_eq!(r_vps.len(), r_dci.len() + 2);
    }

    #[test]
    fn vvc_split_access_units_single_picture_with_param_sets() {
        // Single AU: VPS + SPS + PPS + IDR slice. The parameter sets ride
        // along with the first (and only) access unit.
        let mut buf = Vec::new();
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_VPS, 0x01));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0x02));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_PPS, 0x03));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_IDR_W_RADL, 0x04));
        let units = split_vvc_access_units(&buf);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0], &buf[..]);
    }

    #[test]
    fn vvc_split_access_units_two_pictures_first_keeps_param_sets() {
        // AU 0: VPS + SPS + PPS + IDR. AU 1: TRAIL slice (next picture).
        // Without AUD/PH the splitter must open AU 1 at the next VCL when
        // a VCL has already been emitted in the current pending unit.
        let mut au0 = Vec::new();
        au0.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_VPS, 0x01));
        au0.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0x02));
        au0.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_PPS, 0x03));
        au0.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_IDR_W_RADL, 0x04));
        let mut au1 = Vec::new();
        au1.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_TRAIL, 0x05));

        let mut buf = Vec::new();
        buf.extend_from_slice(&au0);
        buf.extend_from_slice(&au1);
        let units = split_vvc_access_units(&buf);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0], &au0[..]);
        assert_eq!(units[1], &au1[..]);
    }

    #[test]
    fn vvc_split_access_units_aud_starts_new_unit() {
        // AU 0: SPS + IDR. AU 1: AUD + TRAIL — the AUD opens AU 1 even
        // though the existing splitter logic would also fire on the VCL
        // transition. Either rule fires at the AUD boundary.
        let mut buf = Vec::new();
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0x01));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_IDR_W_RADL, 0x02));
        let au1_start = buf.len();
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_AUD, 0x03));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_TRAIL, 0x04));
        let units = split_vvc_access_units(&buf);
        assert_eq!(units.len(), 2);
        assert_eq!(units[1].as_ptr(), buf[au1_start..].as_ptr());
    }

    #[test]
    fn vvc_split_access_units_ph_starts_new_unit() {
        // Per H.266 §7.4.2.4, the picture header (PH_NUT) begins a new
        // picture; the splitter opens AU 1 at PH even without an AUD.
        let mut buf = Vec::new();
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_SPS, 0x01));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_PH, 0x02));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_IDR_W_RADL, 0x03));
        let au1_start = buf.len();
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_PH, 0x04));
        buf.extend_from_slice(&vvc_annex_b_nal(VVC_NUT_TRAIL, 0x05));
        let units = split_vvc_access_units(&buf);
        assert_eq!(units.len(), 2);
        assert_eq!(units[1].as_ptr(), buf[au1_start..].as_ptr());
    }

    #[test]
    fn vvc_split_access_units_empty_buffer_yields_nothing() {
        assert!(split_vvc_access_units(&[]).is_empty());
    }

    #[test]
    fn vvc_codec_type_is_vvc1_fourcc() {
        let expected = u32::from_be_bytes(*b"vvc1");
        assert_eq!(super::K_CM_VIDEO_CODEC_TYPE_VVC, expected);
        assert_eq!(super::K_CM_VIDEO_CODEC_TYPE_VVC, 0x7676_6331);
    }
}
