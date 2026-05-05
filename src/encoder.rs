//! VTCompressionSession-backed H.264 and HEVC encoders.
//!
//! # Design
//!
//! Each encoder holds a `VTCompressionSessionRef` built once at
//! construction time. `send_frame` wraps the I420 `VideoFrame` into a
//! biplanar NV12 `CVPixelBuffer` (VideoToolbox's preferred format) and
//! submits it. The compression callback is called synchronously (we call
//! `VTCompressionSessionCompleteFrames` after each encode) and stores the
//! resulting Annex-B packet in a queue that `receive_packet` drains.
//!
//! Output packets contain SPS/PPS (keyframes) or just slice data
//! exactly as VT produces them, which is already Annex-B compatible
//! once we re-prefix start codes.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use oxideav_core::{CodecId, CodecParameters, Error, Frame, Packet, PixelFormat, Result, TimeBase};

use crate::sys::{self, CMTime, K_OS_STATUS_NO_ERROR};

// kCMVideoCodecType_H264 = 'avc1' = 0x61766331
const K_CM_VIDEO_CODEC_TYPE_H264: u32 = 0x61766331;
// kCMVideoCodecType_HEVC = 'hvc1' = 0x68766331
const K_CM_VIDEO_CODEC_TYPE_HEVC: u32 = 0x68766331;

// kVTCompressionPropertyKey_RealTime
const K_VT_REAL_TIME: &str = "RealTime";
// kVTCompressionPropertyKey_AverageBitRate (reserved for future use)
#[allow(dead_code)]
const K_VT_AVERAGE_BIT_RATE: &str = "AverageBitRate";
// kVTCompressionPropertyKey_AllowFrameReordering
const K_VT_ALLOW_FRAME_REORDER: &str = "AllowFrameReordering";
// kVTCompressionPropertyKey_ProfileLevel (H.264)
const K_VT_PROFILE_LEVEL: &str = "ProfileLevel";
// kVTProfileLevel_H264_Baseline_AutoLevel
const K_VT_H264_BASELINE: &str = "H264_Baseline_AutoLevel";
// kVTProfileLevel_HEVC_Main_AutoLevel
const K_VT_HEVC_MAIN: &str = "HEVC_Main_AutoLevel";

// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange = '420v'
const K_CV_PIXEL_FORMAT_NV12: u32 = 0x34323076;

// ─────────────────────────── Callback state ───────────────────────────────────

struct EncCallbackState {
    packets: VecDeque<Vec<u8>>, // Annex-B encoded packets
    error: Option<String>,
    is_hevc: bool,
}

impl EncCallbackState {
    fn new(is_hevc: bool) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            packets: VecDeque::new(),
            error: None,
            is_hevc,
        }))
    }
}

/// Extract H.264 parameter sets (SPS + PPS) from a CMVideoFormatDescription
/// and return them as Annex-B bytes (start code + raw NAL).
unsafe fn extract_h264_param_sets(
    vt: &sys::Vtable,
    fmt_desc: sys::CMVideoFormatDescriptionRef,
) -> Vec<u8> {
    let mut out = Vec::new();
    // Query count first.
    let mut count: usize = 0;
    let mut _ptr: *const u8 = std::ptr::null();
    let mut _size: usize = 0;
    let mut _nal_len: i32 = 0;
    unsafe {
        (vt.cm_fmt_h264_param_at_idx)(
            fmt_desc,
            0,
            &mut _ptr,
            &mut _size,
            &mut count,
            &mut _nal_len,
        );
    }
    for i in 0..count {
        let mut ptr: *const u8 = std::ptr::null();
        let mut size: usize = 0;
        let st = unsafe {
            (vt.cm_fmt_h264_param_at_idx)(
                fmt_desc,
                i,
                &mut ptr,
                &mut size,
                &mut std::mem::zeroed(),
                &mut std::mem::zeroed(),
            )
        };
        if st == 0 && !ptr.is_null() && size > 0 {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
        }
    }
    out
}

/// Extract HEVC parameter sets (VPS + SPS + PPS) from a CMVideoFormatDescription.
unsafe fn extract_hevc_param_sets(
    vt: &sys::Vtable,
    fmt_desc: sys::CMVideoFormatDescriptionRef,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut count: usize = 0;
    let mut _ptr: *const u8 = std::ptr::null();
    let mut _size: usize = 0;
    let mut _nal_len: i32 = 0;
    unsafe {
        (vt.cm_fmt_hevc_param_at_idx)(
            fmt_desc,
            0,
            &mut _ptr,
            &mut _size,
            &mut count,
            &mut _nal_len,
        );
    }
    for i in 0..count {
        let mut ptr: *const u8 = std::ptr::null();
        let mut size: usize = 0;
        let st = unsafe {
            (vt.cm_fmt_hevc_param_at_idx)(
                fmt_desc,
                i,
                &mut ptr,
                &mut size,
                &mut std::mem::zeroed(),
                &mut std::mem::zeroed(),
            )
        };
        if st == 0 && !ptr.is_null() && size > 0 {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
        }
    }
    out
}

/// Extract Annex-B data from a CMSampleBuffer produced by VT.
///
/// VT encodes output as AVCC (length-prefixed NAL units). We convert
/// to Annex-B (start-code prefixed) for portability.
/// If `is_hevc` and the format description contains parameter sets, they are
/// prepended on every keyframe (IDR / CRA) packet.
unsafe fn extract_annex_b(
    vt: &sys::Vtable,
    sample_buffer: sys::CMSampleBufferRef,
    is_hevc: bool,
) -> Result<Vec<u8>> {
    let block_buf = unsafe { (vt.cm_sample_get_data_buffer)(sample_buffer) };
    if block_buf.is_null() {
        return Err(Error::other("CMSampleBufferGetDataBuffer returned null"));
    }

    let total_len = unsafe { (vt.cm_block_get_data_length)(block_buf) };
    let mut avcc_data = vec![0u8; total_len];

    let status = unsafe {
        (vt.cm_block_copy_data)(
            block_buf,
            0,
            total_len,
            avcc_data.as_mut_ptr() as *mut c_void,
        )
    };
    if status != K_OS_STATUS_NO_ERROR {
        return Err(Error::other(format!(
            "CMBlockBufferCopyDataBytes: {status}"
        )));
    }

    // Convert AVCC → Annex-B: replace each 4-byte big-endian length
    // with a 4-byte start code (00 00 00 01).
    let mut out = Vec::with_capacity(total_len + 256);
    let mut pos = 0usize;
    let mut is_keyframe = false;

    while pos + 4 <= avcc_data.len() {
        let nal_len = u32::from_be_bytes([
            avcc_data[pos],
            avcc_data[pos + 1],
            avcc_data[pos + 2],
            avcc_data[pos + 3],
        ]) as usize;
        pos += 4;
        if pos + nal_len > avcc_data.len() {
            break;
        }
        let nal = &avcc_data[pos..pos + nal_len];
        // Detect keyframe NAL types.
        if !nal.is_empty() {
            if is_hevc {
                let nal_type = (nal[0] >> 1) & 0x3F;
                // IDR_W_RADL=19, IDR_N_LP=20, CRA=21
                if nal_type == 19 || nal_type == 20 || nal_type == 21 {
                    is_keyframe = true;
                }
            } else {
                let nal_type = nal[0] & 0x1F;
                // IDR=5
                if nal_type == 5 {
                    is_keyframe = true;
                }
            }
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(nal);
        pos += nal_len;
    }

    // Prepend parameter sets on keyframes (VT doesn't include them in the
    // AVCC data payload — they live in the CMVideoFormatDescription).
    if is_keyframe {
        let fmt_desc = unsafe { (vt.cm_sample_get_format_desc)(sample_buffer) };
        if !fmt_desc.is_null() {
            let params = if is_hevc {
                unsafe { extract_hevc_param_sets(vt, fmt_desc) }
            } else {
                unsafe { extract_h264_param_sets(vt, fmt_desc) }
            };
            if !params.is_empty() {
                let mut combined = params;
                combined.extend_from_slice(&out);
                return Ok(combined);
            }
        }
    }

    Ok(out)
}

/// Extern-C callback — called by VT with each encoded sample buffer.
unsafe extern "C" fn comp_callback(
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
        guard.error = Some(format!("VT encode callback error: OSStatus {status}"));
        return;
    }

    if sample_buffer.is_null() {
        return;
    }

    let vt = match sys::vtable() {
        Ok(v) => v,
        Err(e) => {
            guard.error = Some(format!("vtable unavailable in encode callback: {e}"));
            return;
        }
    };

    let is_hevc = guard.is_hevc;
    match unsafe { extract_annex_b(vt, sample_buffer, is_hevc) } {
        Ok(data) => guard.packets.push_back(data),
        Err(e) => guard.error = Some(e.to_string()),
    }
}

// ─────────────────────────── VtEncoder ────────────────────────────────────────

pub struct VtEncoder {
    codec_id: CodecId,
    session: sys::VTCompressionSessionRef,
    state: Arc<Mutex<EncCallbackState>>,
    output_queue: VecDeque<Packet>,
    output_params: CodecParameters,
    pts_counter: i64,
    width: usize,
    height: usize,
}

// SAFETY: VTCompressionSessionRef is thread-safe per Apple docs.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new_h264(params: &CodecParameters) -> Result<Box<dyn oxideav_core::Encoder>> {
        Self::create(
            "h264",
            K_CM_VIDEO_CODEC_TYPE_H264,
            K_VT_H264_BASELINE,
            false,
            params,
        )
    }

    pub fn new_hevc(params: &CodecParameters) -> Result<Box<dyn oxideav_core::Encoder>> {
        Self::create(
            "hevc",
            K_CM_VIDEO_CODEC_TYPE_HEVC,
            K_VT_HEVC_MAIN,
            true,
            params,
        )
    }

    fn create(
        codec_id_str: &str,
        codec_type: u32,
        profile_level: &str,
        is_hevc: bool,
        params: &CodecParameters,
    ) -> Result<Box<dyn oxideav_core::Encoder>> {
        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;

        let width = params.width.unwrap_or(320) as usize;
        let height = params.height.unwrap_or(240) as usize;

        let state = EncCallbackState::new(is_hevc);
        let state_raw = Arc::into_raw(Arc::clone(&state)) as *mut c_void;

        let mut session: sys::VTCompressionSessionRef = std::ptr::null_mut();
        let status = unsafe {
            (vt.vt_comp_create)(
                std::ptr::null_mut(),
                width as i32,
                height as i32,
                codec_type,
                std::ptr::null_mut(), // encoder specification
                std::ptr::null_mut(), // source image buffer attributes
                std::ptr::null_mut(), // compressed data allocator
                comp_callback,
                state_raw,
                &mut session,
            )
        };

        if status != K_OS_STATUS_NO_ERROR {
            // Reclaim the leaked Arc.
            let _ = unsafe { Arc::from_raw(state_raw as *const Mutex<EncCallbackState>) };
            return Err(Error::other(format!(
                "VTCompressionSessionCreate: OSStatus {status} (codec_type 0x{codec_type:08x})"
            )));
        }

        // Configure properties.
        // Allow frame reordering = false (reduces latency, simpler test).
        let bool_false = unsafe { sys::cf_number_i32(vt, 0) };
        let reorder_key = unsafe { sys::cf_string(vt, K_VT_ALLOW_FRAME_REORDER) };
        unsafe {
            (vt.vt_session_set_property)(session, reorder_key, bool_false);
            (vt.cf_release)(reorder_key);
            (vt.cf_release)(bool_false);
        }

        // Profile level.
        let profile_cf = unsafe { sys::cf_string(vt, profile_level) };
        let profile_key = unsafe { sys::cf_string(vt, K_VT_PROFILE_LEVEL) };
        unsafe {
            (vt.vt_session_set_property)(session, profile_key, profile_cf);
            (vt.cf_release)(profile_key);
            (vt.cf_release)(profile_cf);
        }

        // Real-time = true (kCFBooleanTrue = a special CF singleton, but VT
        // also accepts a CFNumber 1).
        let bool_true = unsafe { sys::cf_number_i32(vt, 1) };
        let rt_key = unsafe { sys::cf_string(vt, K_VT_REAL_TIME) };
        unsafe {
            (vt.vt_session_set_property)(session, rt_key, bool_true);
            (vt.cf_release)(rt_key);
            (vt.cf_release)(bool_true);
        }

        // Prepare.
        let prep_status = unsafe { (vt.vt_comp_prepare)(session) };
        if prep_status != K_OS_STATUS_NO_ERROR {
            // Non-fatal on older macOS; ignore.
        }

        let mut output_params = CodecParameters::video(CodecId::new(codec_id_str));
        output_params.width = Some(width as u32);
        output_params.height = Some(height as u32);
        output_params.pixel_format = Some(PixelFormat::Yuv420P);
        output_params.frame_rate = params.frame_rate;
        output_params.bit_rate = params.bit_rate;

        Ok(Box::new(VtEncoder {
            codec_id: CodecId::new(codec_id_str),
            session,
            state,
            output_queue: VecDeque::new(),
            output_params,
            pts_counter: 0,
            width,
            height,
        }))
    }

    /// Convert an I420 `VideoFrame` → biplanar NV12 `CVPixelBuffer`.
    ///
    /// VT strongly prefers NV12 on Apple Silicon. We convert on-the-fly.
    fn frame_to_pixel_buffer(
        &self,
        vt: &sys::Vtable,
        frame: &oxideav_core::VideoFrame,
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

        // Build NV12: Y plane + interleaved UV.
        // Keep the data alive in a Box until the pixel buffer is released.
        let y_len = y_plane.stride * height;
        let uv_len = chroma_w * 2 * chroma_h;

        let mut y_data: Vec<u8> = vec![0u8; y_len];
        let mut uv_data: Vec<u8> = vec![0u8; uv_len];

        // Copy Y (possibly re-stride).
        for row in 0..height.min(y_plane.data.len() / y_plane.stride.max(1)) {
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

        // Wrap in a Box so they stay alive.
        let mut y_boxed = y_data.into_boxed_slice();
        let mut uv_boxed = uv_data.into_boxed_slice();

        let mut plane_ptrs: [*mut c_void; 2] = [
            y_boxed.as_mut_ptr() as *mut c_void,
            uv_boxed.as_mut_ptr() as *mut c_void,
        ];
        let plane_widths: [usize; 2] = [width, chroma_w];
        let plane_heights: [usize; 2] = [height, chroma_h];
        let plane_bpr: [usize; 2] = [width, chroma_w * 2];

        // We pass a release callback to free our heap allocations.
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
            // data_ptr is the opaque context we passed; we passed boxes_raw as release_ref_con.
            let _ = data_ptr;
        }

        let mut pixel_buf: sys::CVPixelBufferRef = std::ptr::null_mut();
        let ret = unsafe {
            (vt.cv_pb_create_planar)(
                std::ptr::null_mut(),
                width,
                height,
                K_CV_PIXEL_FORMAT_NV12,
                std::ptr::null_mut(), // dataPtr (base of all planes combined, can be NULL)
                0,                    // dataSize
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
            // Free the boxes ourselves since the callback won't be called.
            let _ = unsafe { Box::from_raw(boxes_raw as *mut PlaneBoxes) };
            return Err(Error::other(format!(
                "CVPixelBufferCreateWithPlanarBytes: CVReturn {ret}"
            )));
        }

        Ok(pixel_buf)
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        if self.session.is_null() {
            return;
        }
        if let Ok(vt) = sys::vtable() {
            unsafe { (vt.vt_comp_invalidate)(self.session) };
        }
    }
}

impl oxideav_core::Encoder for VtEncoder {
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
        let dur_time = CMTime::make(1, 30); // 30 fps default

        let status = unsafe {
            (vt.vt_comp_encode)(
                self.session,
                pixel_buf,
                pts_time,
                dur_time,
                std::ptr::null_mut(), // frame properties
                std::ptr::null_mut(), // source frame ref con
                std::ptr::null_mut(), // info flags out
            )
        };

        // Release our reference — VT retains it internally.
        unsafe { (vt.cf_release)(pixel_buf) };

        if status != K_OS_STATUS_NO_ERROR {
            return Err(Error::other(format!(
                "VTCompressionSessionEncodeFrame: {status}"
            )));
        }

        // Force synchronous completion.
        let complete_status =
            unsafe { (vt.vt_comp_complete)(self.session, CMTime::make(i64::MAX, 1)) };
        if complete_status != K_OS_STATUS_NO_ERROR {
            return Err(Error::other(format!(
                "VTCompressionSessionCompleteFrames: {complete_status}"
            )));
        }

        // Drain newly produced packets.
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

// ─────────────────────────── Public factory functions ─────────────────────────

pub fn make_h264_encoder(params: &CodecParameters) -> Result<Box<dyn oxideav_core::Encoder>> {
    VtEncoder::new_h264(params)
}

pub fn make_hevc_encoder(params: &CodecParameters) -> Result<Box<dyn oxideav_core::Encoder>> {
    VtEncoder::new_hevc(params)
}
