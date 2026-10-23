//! VTDecompressionSession-backed H.264 and HEVC decoders.
//!
//! # Design
//!
//! Both decoders share one NAL-stream core ([`NalVtDecoder`]):
//!
//! * **Parameter sets** come from the container's configuration record
//!   in `CodecParameters::extradata` (`avcC` per ISO/IEC 14496-15 §5.3.3,
//!   `hvcC` per §8.3.3, or Annex-B-framed parameter sets) *and* from the
//!   stream itself. The `CMVideoFormatDescription` is built from them
//!   with `CMVideoFormatDescriptionCreateFromH264ParameterSets` /
//!   `…HEVCParameterSets`, and the session is re-created when the
//!   active parameter sets change mid-stream.
//! * **Framing** is detected per packet: a packet that starts with an
//!   Annex-B start code is split on start codes (and, when it carries
//!   several coded pictures, into access units on the §7.4.1.2.3 /
//!   §7.4.2.4.4 boundaries); any other packet is length-prefixed with the
//!   configuration record's `lengthSizeMinusOne + 1` (4 when no record
//!   was seen). Each access unit is re-framed with 4-byte lengths and
//!   handed to VideoToolbox as one `CMSampleBuffer`.
//! * **Output order.** VideoToolbox returns pictures in decode order
//!   (measured: `kVTDecodeFrame_EnableTemporalProcessing` defers the
//!   callbacks but never re-orders them). Each access unit is submitted
//!   with a sequence stamp, its picture order count is derived from the
//!   slice header ([`crate::nalparse`]), and decoded pictures wait in a
//!   buffer sorted by order count until the stream's reorder depth is
//!   exceeded, the container's decode timeline (`dts`) proves they are
//!   due, a new coded video sequence starts, or `flush` — the
//!   `VTDecompressionSessionWaitForAsynchronousFrames` barrier — drains
//!   everything.
//! * **Labels.** The output label is negotiated once (see
//!   [`crate::pixbuf`]): the container's declared pixel format when
//!   VideoToolbox can emit it, otherwise the layout the SPS describes
//!   (chroma format × bit depth). VideoToolbox is asked for exactly that
//!   layout and the frames are converted into it, so
//!   [`Decoder::output_pixel_format`] and the planes always agree.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use oxideav_core::{
    CodecId, CodecParameters, Decoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame,
};

use crate::encoder::{HardwareMode, build_hw_spec_dict, parse_hardware_mode, vt_error};
use crate::nalparse::{
    H264PocState, H264Pps, H264Sps, HevcPocState, HevcPps, HevcSps, h264_slice_info,
    hevc_slice_info,
};
use crate::pixbuf;
use crate::sys::{
    self, CMSampleTimingInfo, CMTime, K_CV_PIXEL_BUFFER_LOCK_FLAGS_READ_ONLY, K_OS_STATUS_NO_ERROR,
};

// ─────────────────────────── NAL unit framing ─────────────────────────────────

/// Split an Annex B byte stream (ITU-T H.264 / H.265 Annex B) into its
/// NAL units (start codes stripped, trailing zero bytes trimmed).
pub fn annex_b_nals(buf: &[u8]) -> Vec<&[u8]> {
    let mut result = Vec::new();
    let mut pos = 0usize;
    let len = buf.len();

    while pos < len {
        // Find start code.
        let mut sc_len = 0usize;
        while pos + 3 <= len {
            if buf[pos] == 0 && buf[pos + 1] == 0 {
                if pos + 4 <= len && buf[pos + 2] == 0 && buf[pos + 3] == 1 {
                    sc_len = 4;
                    break;
                } else if buf[pos + 2] == 1 {
                    sc_len = 3;
                    break;
                }
            }
            pos += 1;
        }

        if sc_len == 0 {
            break;
        }
        pos += sc_len;
        let nal_start = pos;

        // Find end (next start code or end of buf).
        let mut nal_end = len;
        while pos + 3 <= len {
            let three = buf[pos] == 0 && buf[pos + 1] == 0 && buf[pos + 2] == 1;
            let four = buf[pos] == 0
                && buf[pos + 1] == 0
                && buf[pos + 2] == 0
                && pos + 4 <= len
                && buf[pos + 3] == 1;
            if three || four {
                nal_end = pos;
                // Strip trailing zeros.
                while nal_end > nal_start && buf[nal_end - 1] == 0 {
                    nal_end -= 1;
                }
                break;
            }
            pos += 1;
        }

        if nal_end > nal_start {
            result.push(&buf[nal_start..nal_end]);
        }
        // Don't advance pos past nal_end — the outer loop will rescan.
        pos = nal_end;
    }

    result
}

/// `true` when `buf` starts with an Annex B start code (3- or 4-byte).
pub fn starts_with_start_code(buf: &[u8]) -> bool {
    buf.starts_with(&[0, 0, 1]) || buf.starts_with(&[0, 0, 0, 1])
}

/// Split a length-prefixed sample (ISO/IEC 14496-15 §5.3.4.2 / §8.3.2,
/// `length_size` ∈ {1, 2, 4} bytes, big-endian) into its NAL units. A
/// length that overruns the buffer ends the walk (the units before it
/// are still returned).
pub fn length_prefixed_nals(buf: &[u8], length_size: usize) -> Vec<&[u8]> {
    length_prefixed_walk(buf, length_size).0
}

/// The NAL units of a length-prefixed walk and whether the walk consumed
/// `buf` exactly (every length in bounds, no trailing bytes).
fn length_prefixed_walk(buf: &[u8], length_size: usize) -> (Vec<&[u8]>, bool) {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + length_size <= buf.len() {
        let mut len = 0usize;
        for &b in &buf[pos..pos + length_size] {
            len = (len << 8) | b as usize;
        }
        pos += length_size;
        if len == 0 || pos + len > buf.len() {
            return (out, false);
        }
        out.push(&buf[pos..pos + len]);
        pos += len;
    }
    let exact = pos == buf.len() && !out.is_empty();
    (out, exact)
}

/// NAL units of one packet in whichever framing it uses. A length prefix
/// can look like a start code (`00 00 01 xx` is a 4-byte length of
/// 256–511 bytes), so when a configuration record declared the sample
/// length size the length-prefixed reading wins whenever it consumes
/// the packet exactly; without a record a start code means Annex B and
/// anything else is read with 4-byte lengths.
pub fn packet_nals(buf: &[u8], length_size: Option<usize>) -> Vec<&[u8]> {
    match length_size {
        Some(ls) => {
            let (nals, exact) = length_prefixed_walk(buf, ls);
            if exact { nals } else { annex_b_nals(buf) }
        }
        None if starts_with_start_code(buf) => annex_b_nals(buf),
        None => length_prefixed_nals(buf, 4),
    }
}

/// Which of the two NAL-stream codecs a [`NalVtDecoder`] drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalCodec {
    H264,
    Hevc,
}

impl NalCodec {
    fn id(self) -> &'static str {
        match self {
            NalCodec::H264 => "h264",
            NalCodec::Hevc => "hevc",
        }
    }

    /// NAL unit type of `nal` (`None` for a header too short to carry one).
    pub fn nal_type(self, nal: &[u8]) -> Option<u8> {
        match self {
            NalCodec::H264 => nal.first().map(|b| b & 0x1F),
            NalCodec::Hevc => (nal.len() >= 2).then(|| (nal[0] >> 1) & 0x3F),
        }
    }

    /// `true` for a VCL (coded slice) NAL unit type.
    pub fn is_vcl(self, nal_type: u8) -> bool {
        match self {
            NalCodec::H264 => (1..=5).contains(&nal_type),
            NalCodec::Hevc => nal_type <= 31,
        }
    }

    /// Parameter-set slot of a NAL unit type: 0 = VPS, 1 = SPS, 2 = PPS.
    fn param_set_slot(self, nal_type: u8) -> Option<usize> {
        match (self, nal_type) {
            (NalCodec::H264, 7) => Some(1),
            (NalCodec::H264, 8) => Some(2),
            (NalCodec::Hevc, 32) => Some(0),
            (NalCodec::Hevc, 33) => Some(1),
            (NalCodec::Hevc, 34) => Some(2),
            _ => None,
        }
    }

    /// `true` when a VCL NAL unit starts a new coded picture: H.264
    /// §7.4.1.2.4 (`first_mb_in_slice == 0`, the first `ue(v)` of the
    /// slice header, i.e. the first bit after the NAL header is 1);
    /// H.265 §7.4.2.4.4 (`first_slice_segment_in_pic_flag`, the first
    /// bit after the two-byte NAL header).
    pub fn starts_picture(self, nal: &[u8]) -> bool {
        match self {
            NalCodec::H264 => nal.len() >= 2 && nal[1] & 0x80 != 0,
            NalCodec::Hevc => nal.len() >= 3 && nal[2] & 0x80 != 0,
        }
    }

    /// `true` for the non-VCL NAL unit types that may only precede the
    /// first VCL NAL unit of an access unit (H.264 §7.4.1.2.3: AUD, SPS,
    /// PPS, SEI, prefix / subset-SPS / DPS types 14–18; H.265 §7.4.2.4.4:
    /// VPS, SPS, PPS, AUD, prefix SEI, reserved 41–44).
    pub fn starts_access_unit(self, nal_type: u8) -> bool {
        match self {
            NalCodec::H264 => matches!(nal_type, 6..=9 | 14..=18),
            NalCodec::Hevc => matches!(nal_type, 32..=35 | 39 | 41..=44),
        }
    }
}

/// Group the NAL units of one packet into access units (one coded
/// picture each, with its leading non-VCL units). Parameter sets are
/// *not* removed — the caller files them and strips them.
pub fn split_access_units<'a>(codec: NalCodec, nals: &[&'a [u8]]) -> Vec<Vec<&'a [u8]>> {
    let mut units: Vec<Vec<&'a [u8]>> = Vec::new();
    let mut current: Vec<&'a [u8]> = Vec::new();
    let mut seen_vcl = false;
    for &nal in nals {
        let Some(t) = codec.nal_type(nal) else {
            continue;
        };
        let boundary = seen_vcl
            && (codec.starts_access_unit(t) || (codec.is_vcl(t) && codec.starts_picture(nal)));
        if boundary {
            units.push(std::mem::take(&mut current));
            seen_vcl = false;
        }
        if codec.is_vcl(t) {
            seen_vcl = true;
        }
        current.push(nal);
    }
    if !current.is_empty() {
        units.push(current);
    }
    units
}

// ─────────────────────────── Configuration records ───────────────────────────

/// Parameter sets and sample NAL length size read from a codec
/// configuration record (`avcC` / `hvcC`) or from Annex-B extradata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParamSets {
    /// `[VPS, SPS, PPS]` NAL units (VPS stays empty for H.264).
    pub sets: [Vec<Vec<u8>>; 3],
    /// `lengthSizeMinusOne + 1` of the record, when one was parsed.
    pub length_size: Option<usize>,
}

impl ParamSets {
    fn is_complete(&self) -> bool {
        !self.sets[1].is_empty() && !self.sets[2].is_empty()
    }

    /// File a parameter-set NAL unit. Returns `true` when the active
    /// set changed (a new NAL replaces every earlier one in its slot).
    fn file(&mut self, slot: usize, nal: &[u8]) -> bool {
        if self.sets[slot].len() == 1 && self.sets[slot][0] == nal {
            return false;
        }
        self.sets[slot] = vec![nal.to_vec()];
        true
    }

    /// Parse `extradata`: an `avcC` record (ISO/IEC 14496-15 §5.3.3.1.2),
    /// an `hvcC` record (§8.3.3.1.2), or Annex-B-framed parameter sets.
    /// Returns `None` when nothing usable was found.
    pub fn from_extradata(codec: NalCodec, extradata: &[u8]) -> Option<Self> {
        if extradata.is_empty() {
            return None;
        }
        if starts_with_start_code(extradata) {
            let mut ps = ParamSets::default();
            for nal in annex_b_nals(extradata) {
                if let Some(slot) = codec.nal_type(nal).and_then(|t| codec.param_set_slot(t)) {
                    ps.sets[slot].push(nal.to_vec());
                }
            }
            return ps.is_complete().then_some(ps);
        }
        match codec {
            NalCodec::H264 => parse_avcc(extradata),
            NalCodec::Hevc => parse_hvcc(extradata),
        }
    }
}

fn read_u16(buf: &[u8], pos: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]) as usize)
}

/// `AVCDecoderConfigurationRecord`: configurationVersion(1) = 1,
/// profile / compatibility / level (3), `0b111111` + lengthSizeMinusOne
/// (1), `0b111` + numOfSequenceParameterSets (1), then each SPS as
/// (u16 length, bytes), numOfPictureParameterSets (1), each PPS likewise.
fn parse_avcc(buf: &[u8]) -> Option<ParamSets> {
    if buf.len() < 7 || buf[0] != 1 {
        return None;
    }
    let mut ps = ParamSets {
        length_size: Some((buf[4] & 0x03) as usize + 1),
        ..Default::default()
    };
    let mut pos = 6;
    let num_sps = (buf[5] & 0x1F) as usize;
    for _ in 0..num_sps {
        let len = read_u16(buf, pos)?;
        pos += 2;
        ps.sets[1].push(buf.get(pos..pos + len)?.to_vec());
        pos += len;
    }
    let num_pps = *buf.get(pos)? as usize;
    pos += 1;
    for _ in 0..num_pps {
        let len = read_u16(buf, pos)?;
        pos += 2;
        ps.sets[2].push(buf.get(pos..pos + len)?.to_vec());
        pos += len;
    }
    ps.is_complete().then_some(ps)
}

/// `HEVCDecoderConfigurationRecord`: a 22-byte fixed header
/// (configurationVersion = 1 … `lengthSizeMinusOne` in the low two bits
/// of byte 21), numOfArrays (1), then per array: array_completeness /
/// NAL_unit_type (1), numNalus (2), each NAL as (u16 length, bytes).
fn parse_hvcc(buf: &[u8]) -> Option<ParamSets> {
    if buf.len() < 23 || buf[0] != 1 {
        return None;
    }
    let mut ps = ParamSets {
        length_size: Some((buf[21] & 0x03) as usize + 1),
        ..Default::default()
    };
    let num_arrays = buf[22] as usize;
    let mut pos = 23;
    for _ in 0..num_arrays {
        let nal_type = *buf.get(pos)? & 0x3F;
        let count = read_u16(buf, pos + 1)?;
        pos += 3;
        for _ in 0..count {
            let len = read_u16(buf, pos)?;
            pos += 2;
            let nal = buf.get(pos..pos + len)?;
            pos += len;
            if let Some(slot) = NalCodec::Hevc.param_set_slot(nal_type) {
                ps.sets[slot].push(nal.to_vec());
            }
        }
    }
    ps.is_complete().then_some(ps)
}

// ─────────────────────────── Callback state ───────────────────────────────────

struct CallbackState {
    /// Decoded pictures with the label VideoToolbox delivered them in and
    /// the sequence stamp they were submitted with.
    frames: VecDeque<(VideoFrame, PixelFormat, Option<i64>)>,
    error: Option<String>,
}

impl CallbackState {
    fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            frames: VecDeque::new(),
            error: None,
        }))
    }
}

/// Timescale of the sequence stamp each access unit is submitted with
/// (the value is the submission index; the picture comes back carrying
/// it, which is how the decoded picture is matched to its metadata).
const SEQ_TIMESCALE: i32 = 1;

unsafe extern "C" fn decomp_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: u32,
    image_buffer: sys::CVImageBufferRef,
    presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    let state_ptr = output_callback_ref_con as *const Mutex<CallbackState>;
    let state = unsafe { &*state_ptr };
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    if status != K_OS_STATUS_NO_ERROR {
        guard.error = Some(format!(
            "VT decode callback: OSStatus {}",
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
            guard.error = Some(format!("vtable in callback: {e}"));
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
    let frame = unsafe { pixbuf::pixel_buffer_to_frame(vt, image_buffer) };
    unsafe { (vt.cv_pb_unlock)(image_buffer, 0) };

    match frame {
        Ok((f, label)) => {
            // The CMTime handed back is the sequence stamp the sample was
            // submitted with.
            let seq = (presentation_time_stamp.is_valid()
                && presentation_time_stamp.timescale == SEQ_TIMESCALE)
                .then_some(presentation_time_stamp.value);
            guard.frames.push_back((f, label, seq));
        }
        Err(e) => guard.error = Some(e),
    }
}

// ─────────────────────────── Session creation ─────────────────────────────────

/// Create the decompression session for `fmt_desc`, asking for the
/// negotiated output pixel format (`request`).
///
/// `hw_mode` (from `options["hardware"]`) selects the optional
/// video-decoder-specification dictionary
/// (`kVTVideoDecoderSpecification_{Require,Enable}HardwareAcceleratedVideoDecoder`
/// per `VTDecompressionProperties.h`); `None` keeps VT's default policy.
fn create_vt_session(
    vt: &sys::Vtable,
    fmt_desc: sys::CMVideoFormatDescriptionRef,
    request: u32,
    state: &Arc<Mutex<CallbackState>>,
    hw_mode: Option<HardwareMode>,
) -> Result<sys::VTDecompressionSessionRef> {
    let pixel_fmt_num = unsafe { sys::cf_number_i32(vt, request as i32) };
    // Keyed by the framework's own `kCVPixelBufferPixelFormatTypeKey`
    // constant (a CFString built from the symbol's *name* never matches
    // and the request is silently ignored).
    let dest_attrs = unsafe {
        sys::cf_dict1(
            vt,
            vt.cv_pixel_format_type_key,
            pixel_fmt_num as sys::CFTypeRef,
        )
    };

    let state_raw = Arc::as_ptr(state) as *mut c_void;
    let record = sys::VTDecompressionOutputCallbackRecord {
        decomp_output_callback: decomp_callback,
        decomp_output_ref_con: state_raw,
    };

    let hw_spec = match hw_mode {
        Some(mode) => unsafe { build_hw_spec_dict(vt, mode, false) },
        None => std::ptr::null_mut(),
    };

    let mut session = std::ptr::null_mut();
    let status = unsafe {
        (vt.vt_decomp_create)(
            std::ptr::null_mut(),
            fmt_desc,
            hw_spec,
            dest_attrs,
            &record,
            &mut session,
        )
    };

    if !hw_spec.is_null() {
        unsafe { (vt.cf_release)(hw_spec) };
    }
    unsafe { (vt.cf_release)(dest_attrs) };
    unsafe { (vt.cf_release)(pixel_fmt_num) };

    if status != K_OS_STATUS_NO_ERROR {
        Err(vt_error("VTDecompressionSessionCreate", status))
    } else {
        Ok(session)
    }
}

/// Submit one access unit (NAL units re-framed with 4-byte lengths) to a
/// VT session. `fmt_desc` must be the format description the session was
/// created with. `stamp` is the CMTime the frame comes back with.
fn submit_access_unit(
    vt: &sys::Vtable,
    session: sys::VTDecompressionSessionRef,
    fmt_desc: sys::CMVideoFormatDescriptionRef,
    nal_units: &[&[u8]],
    stamp: CMTime,
) -> Result<()> {
    if nal_units.is_empty() {
        return Ok(());
    }

    // Build the length-prefixed payload (4-byte big-endian length per
    // NAL, matching the `NALUnitHeaderLength` of 4 the format
    // description was created with).
    let mut avcc: Vec<u8> = Vec::new();
    for nal in nal_units {
        let len = nal.len() as u32;
        avcc.extend_from_slice(&len.to_be_bytes());
        avcc.extend_from_slice(nal);
    }
    let avcc_len = avcc.len();

    // Hand CoreMedia a malloc'd copy with the default block allocator so
    // the block buffer owns (and frees) the bytes.
    let data_copy = unsafe {
        let ptr = libc_malloc(avcc_len);
        if ptr.is_null() {
            return Err(Error::other("malloc for CMBlockBuffer data failed"));
        }
        std::ptr::copy_nonoverlapping(avcc.as_ptr(), ptr as *mut u8, avcc_len);
        ptr
    };

    let mut block_buf: sys::CMBlockBufferRef = std::ptr::null_mut();
    let status = unsafe {
        (vt.cm_block_create_with_mem)(
            std::ptr::null_mut(), // structure allocator = kCFAllocatorDefault
            data_copy,
            avcc_len,
            std::ptr::null_mut(), // block allocator = kCFAllocatorDefault = will free data_copy
            std::ptr::null(),     // custom block source
            0,                    // offset
            avcc_len,
            0, // flags
            &mut block_buf,
        )
    };

    if status != K_OS_STATUS_NO_ERROR {
        // Free our copy since CF won't.
        unsafe { libc_free(data_copy) };
        return Err(vt_error("CMBlockBufferCreateWithMemoryBlock", status));
    }

    let timing = CMSampleTimingInfo {
        duration: CMTime::make(1, 30),
        presentation_time_stamp: stamp,
        decode_time_stamp: CMTime::invalid(),
    };

    let mut sample_buf: sys::CMSampleBufferRef = std::ptr::null_mut();
    let status = unsafe {
        (vt.cm_sample_create_ready)(
            std::ptr::null_mut(),
            block_buf,
            fmt_desc, // must match the session's format description
            1,        // num samples
            1,        // num timing entries
            &timing,
            1,
            &avcc_len,
            &mut sample_buf,
        )
    };

    unsafe { (vt.cf_release)(block_buf) };

    if status != K_OS_STATUS_NO_ERROR {
        return Err(vt_error("CMSampleBufferCreateReady", status));
    }

    // No decode flags: the picture is delivered (in decode order) before
    // this call returns; display ordering is the decoder's own job.
    let dec_status = unsafe {
        (vt.vt_decomp_decode)(
            session,
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

    Ok(())
}

// Minimal libc shim — just malloc/free.
unsafe fn libc_malloc(size: usize) -> *mut c_void {
    unsafe extern "C" {
        fn malloc(size: usize) -> *mut c_void;
    }
    unsafe { malloc(size) }
}

unsafe fn libc_free(ptr: *mut c_void) {
    unsafe extern "C" {
        fn free(ptr: *mut c_void);
    }
    unsafe { free(ptr) }
}

// ─────────────────────────── Output re-ordering ──────────────────────────────

/// What the decoder knows about a submitted access unit while VideoToolbox
/// holds it.
#[derive(Clone, Copy, Debug)]
struct Submitted {
    /// Coded-video-sequence number (bumped at every IDR / IRAP that
    /// starts a sequence) — pictures of an earlier sequence always come
    /// out first.
    period: u64,
    /// Picture order count within the sequence (`None`: the slice header
    /// could not be read — the picture is emitted in decode order).
    poc: Option<i64>,
    /// The packet's presentation timestamp, handed back on the frame.
    pts: Option<i64>,
}

/// A decoded picture waiting for its turn in display order.
struct Held {
    period: u64,
    poc: i64,
    seq: i64,
    frame: VideoFrame,
}

/// Parsed parameter sets and order-count history of one codec.
enum OrderState {
    H264 {
        sps: HashMap<u32, H264Sps>,
        pps: HashMap<u32, H264Pps>,
        poc: H264PocState,
    },
    Hevc {
        sps: HashMap<u32, HevcSps>,
        pps: HashMap<u32, HevcPps>,
        poc: HevcPocState,
    },
}

impl OrderState {
    fn new(codec: NalCodec) -> Self {
        match codec {
            NalCodec::H264 => OrderState::H264 {
                sps: HashMap::new(),
                pps: HashMap::new(),
                poc: H264PocState::default(),
            },
            NalCodec::Hevc => OrderState::Hevc {
                sps: HashMap::new(),
                pps: HashMap::new(),
                poc: HevcPocState::new(),
            },
        }
    }

    /// File a parameter-set NAL unit (`slot` 1 = SPS, 2 = PPS).
    fn file(&mut self, slot: usize, nal: &[u8]) {
        match self {
            OrderState::H264 { sps, pps, .. } => match slot {
                1 => {
                    if let Some(s) = H264Sps::parse(nal) {
                        sps.insert(s.id, s);
                    }
                }
                2 => {
                    if let Some(p) = H264Pps::parse(nal) {
                        pps.insert(p.id, p);
                    }
                }
                _ => {}
            },
            OrderState::Hevc { sps, pps, .. } => match slot {
                1 => {
                    if let Some(s) = HevcSps::parse(nal) {
                        sps.insert(s.id, s);
                    }
                }
                2 => {
                    if let Some(p) = HevcPps::parse(nal) {
                        pps.insert(p.id, p);
                    }
                }
                _ => {}
            },
        }
    }

    /// Order count of the picture whose first VCL NAL unit is `vcl`,
    /// whether it starts a new coded video sequence, and the reorder
    /// depth of its SPS. `None` when the slice header cannot be read.
    fn picture(&mut self, vcl: &[u8]) -> Option<(i64, bool, usize)> {
        match self {
            OrderState::H264 { sps, pps, poc } => {
                let info = h264_slice_info(vcl, sps, pps)?;
                let s = sps.get(&info.sps_id)?;
                let count = poc.next(s, &info);
                Some((count, info.idr, s.reorder_depth()))
            }
            OrderState::Hevc { sps, pps, poc } => {
                let info = hevc_slice_info(vcl, sps, pps)?;
                let s = sps.get(&info.sps_id)?;
                let (count, starts) = poc.next(s, &info);
                Some((count, starts, s.reorder_depth()))
            }
        }
    }

    /// An end-of-sequence / end-of-bitstream NAL unit: the next IRAP
    /// starts a new sequence (H.265 `NoRaslOutputFlag`).
    fn end_of_sequence(&mut self) {
        if let OrderState::Hevc { poc, .. } = self {
            poc.sequence_start_pending = true;
        }
    }

    /// The layout the most recently filed SPS describes.
    fn sps_label(&self, nal: &[u8]) -> Option<PixelFormat> {
        match self {
            OrderState::H264 { .. } => H264Sps::parse(nal)?.pixel_format(),
            OrderState::Hevc { .. } => HevcSps::parse(nal)?.pixel_format(),
        }
    }
}

// ─────────────────────────── Shared NAL-stream decoder ───────────────────────

/// The H.264 / HEVC decoder core (see the module docs).
pub struct NalVtDecoder {
    codec: NalCodec,
    codec_id: CodecId,
    session: sys::VTDecompressionSessionRef,
    /// Retained format description — used to create CMSampleBuffers.
    fmt_desc: sys::CMVideoFormatDescriptionRef,
    state: Arc<Mutex<CallbackState>>,
    /// Active parameter sets (from extradata, then the stream).
    params: ParamSets,
    /// Parameter sets the live session was created from.
    session_params: ParamSets,
    /// Label of the delivered frames, once negotiated.
    target: Option<PixelFormat>,
    /// Container-declared label (takes precedence over the SPS layout).
    declared: Option<PixelFormat>,
    /// `kCVPixelFormatType_*` requested for `target`.
    request: Option<u32>,
    /// Order-count bookkeeping.
    order: OrderState,
    /// Submitted access units VideoToolbox has not returned yet, by stamp.
    pending: HashMap<i64, Submitted>,
    /// Decoded pictures waiting for display order, sorted by
    /// `(period, poc, seq)`.
    held: Vec<Held>,
    /// Current coded-video-sequence number.
    period: u64,
    /// Reorder depth of the active SPS.
    depth: usize,
    /// Latest packet `dts`, once the container has shown a real decode
    /// timeline (`dts < pts` on some packet) — a held picture whose
    /// `pts` is at or before it is due.
    latest_dts: Option<i64>,
    dts_timeline: bool,
    next_seq: i64,
    output_queue: VecDeque<VideoFrame>,
    flushed: bool,
    /// Hardware-acceleration policy from `options["hardware"]`.
    hw_mode: Option<HardwareMode>,
}

unsafe impl Send for NalVtDecoder {}

impl NalVtDecoder {
    /// Construct for `codec`. Fails with `Unsupported` when the framework
    /// cannot be loaded or the container declares a pixel format
    /// VideoToolbox cannot deliver (the registry then falls back to the
    /// pure-Rust decoder).
    pub fn make(codec: NalCodec, params: &CodecParameters) -> Result<Box<dyn Decoder>> {
        sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;
        let declared = params.pixel_format;
        if let Some(pf) = declared
            && pixbuf::request_for_label(pf, false).is_none()
        {
            return Err(Error::unsupported(format!(
                "videotoolbox: cannot deliver {} frames as {pf:?}",
                codec.id()
            )));
        }
        let mut dec = NalVtDecoder {
            codec,
            codec_id: CodecId::new(codec.id()),
            session: std::ptr::null_mut(),
            fmt_desc: std::ptr::null_mut(),
            state: CallbackState::new(),
            params: ParamSets::default(),
            session_params: ParamSets::default(),
            target: None,
            declared,
            request: None,
            order: OrderState::new(codec),
            pending: HashMap::new(),
            held: Vec::new(),
            period: 0,
            depth: 16,
            latest_dts: None,
            dts_timeline: false,
            next_seq: 0,
            output_queue: VecDeque::new(),
            flushed: false,
            hw_mode: params.options.get("hardware").and_then(parse_hardware_mode),
        };
        if let Some(ps) = ParamSets::from_extradata(codec, &params.extradata) {
            for (slot, set) in ps.sets.iter().enumerate() {
                for nal in set {
                    dec.order.file(slot, nal);
                }
            }
            dec.params = ps;
            // With a configuration record the session can be built now,
            // so `output_pixel_format` is known before the first packet.
            dec.ensure_session()?;
        }
        Ok(Box::new(dec))
    }

    /// Negotiate the output label: the container's declaration, else the
    /// layout the active SPS describes, else 8-bit 4:2:0.
    fn negotiate_target(&mut self) -> Result<()> {
        if self.target.is_some() {
            return Ok(());
        }
        let sps_label = self.params.sets[1]
            .first()
            .and_then(|sps| self.order.sps_label(sps));
        let label = self.declared.or(sps_label).unwrap_or(PixelFormat::Yuv420P);
        let full_range = label.implied_color_range() == Some(oxideav_core::ColorRange::Full);
        let (request, _) = pixbuf::request_for_label(label, full_range).ok_or_else(|| {
            Error::unsupported(format!(
                "videotoolbox: cannot deliver {} frames as {label:?} (stream layout)",
                self.codec.id()
            ))
        })?;
        self.target = Some(label);
        self.request = Some(request);
        Ok(())
    }

    fn ensure_session(&mut self) -> Result<()> {
        if !self.params.is_complete() {
            return Ok(());
        }
        if !self.session.is_null() {
            if self.session_params.sets == self.params.sets {
                return Ok(());
            }
            // Parameter sets changed: drain the old session, then rebuild.
            self.teardown_session(true);
        }
        self.negotiate_target()?;

        let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;

        let all: Vec<&Vec<u8>> = self.params.sets.iter().flatten().collect();
        let param_ptrs: Vec<*const u8> = all.iter().map(|v| v.as_ptr()).collect();
        let param_sizes: Vec<usize> = all.iter().map(|v| v.len()).collect();

        let mut fmt_desc: sys::CMVideoFormatDescriptionRef = std::ptr::null_mut();
        let st = match self.codec {
            NalCodec::H264 => unsafe {
                (vt.cm_fmt_from_h264_params)(
                    std::ptr::null_mut(),
                    param_ptrs.len(),
                    param_ptrs.as_ptr(),
                    param_sizes.as_ptr(),
                    4,
                    &mut fmt_desc,
                )
            },
            NalCodec::Hevc => unsafe {
                (vt.cm_fmt_from_hevc_params)(
                    std::ptr::null_mut(),
                    param_ptrs.len(),
                    param_ptrs.as_ptr(),
                    param_sizes.as_ptr(),
                    4,
                    std::ptr::null_mut(),
                    &mut fmt_desc,
                )
            },
        };
        if st != K_OS_STATUS_NO_ERROR {
            return Err(vt_error(
                match self.codec {
                    NalCodec::H264 => "CMVideoFormatDescriptionCreateFromH264ParameterSets",
                    NalCodec::Hevc => "CMVideoFormatDescriptionCreateFromHEVCParameterSets",
                },
                st,
            ));
        }

        let request = self.request.unwrap_or(pixbuf::FOURCC_420V);
        let session = match create_vt_session(vt, fmt_desc, request, &self.state, self.hw_mode) {
            Ok(s) => s,
            Err(e) => {
                unsafe { (vt.cf_release)(fmt_desc) };
                return Err(e);
            }
        };

        // The session retains fmt_desc; keep the creation reference as
        // our own (released in `teardown_session` / `Drop`).
        self.session = session;
        self.fmt_desc = fmt_desc;
        self.session_params = self.params.clone();

        Ok(())
    }

    /// Invalidate and release the live session (after draining its
    /// outstanding pictures when `drain`).
    fn teardown_session(&mut self, drain: bool) {
        if self.session.is_null() {
            return;
        }
        if let Ok(vt) = sys::vtable() {
            unsafe {
                if drain {
                    (vt.vt_decomp_wait)(self.session);
                }
                // Per VTDecompressionSession.h: invalidate to tear it
                // down, then CFRelease the object reference (a session
                // is a CF object; invalidating alone leaks it).
                (vt.vt_decomp_invalidate)(self.session);
                (vt.cf_release)(self.session);
                if !self.fmt_desc.is_null() {
                    (vt.cf_release)(self.fmt_desc);
                }
            }
        }
        self.session = std::ptr::null_mut();
        self.fmt_desc = std::ptr::null_mut();
        if drain {
            self.pull_frames();
        }
    }

    /// Move the callback's pictures into the hold buffer (converted into
    /// the negotiated target label, matched to their submission
    /// metadata), then release whatever is due in display order.
    fn pull_frames(&mut self) {
        let mut arrived: Vec<(VideoFrame, PixelFormat, Option<i64>)> = Vec::new();
        if let Ok(mut g) = self.state.lock() {
            arrived.extend(g.frames.drain(..));
        }
        for (f, native, seq) in arrived {
            let target = self.target.unwrap_or(native);
            let Some(mut frame) = pixbuf::deliver_as(f, native, target) else {
                if let Ok(mut g) = self.state.lock() {
                    g.error = Some(format!(
                        "VideoToolbox delivered {native:?} frames but the stream is labelled {target:?}"
                    ));
                }
                continue;
            };
            let seq = seq.unwrap_or(-1);
            let meta = self.pending.remove(&seq).unwrap_or(Submitted {
                period: self.period,
                poc: None,
                pts: None,
            });
            frame.pts = meta.pts;
            match meta.poc {
                Some(poc) => {
                    let at = self
                        .held
                        .partition_point(|h| (h.period, h.poc, h.seq) <= (meta.period, poc, seq));
                    self.held.insert(
                        at,
                        Held {
                            period: meta.period,
                            poc,
                            seq,
                            frame,
                        },
                    );
                }
                // Unreadable slice header: decode order, ahead of nothing.
                None => {
                    self.release(usize::MAX);
                    self.output_queue.push_back(frame);
                }
            }
        }
        self.release_due();
    }

    /// Release held pictures that are due: those of an earlier coded
    /// video sequence, those beyond the reorder depth, and (when the
    /// container has a decode timeline) those presented at or before the
    /// latest decode time.
    fn release_due(&mut self) {
        while self.held.first().is_some_and(|h| h.period < self.period) {
            self.release(1);
        }
        if self.held.len() > self.depth {
            let n = self.held.len() - self.depth;
            self.release(n);
        }
        if let (true, Some(dts)) = (self.dts_timeline, self.latest_dts) {
            while self
                .held
                .first()
                .and_then(|h| h.frame.pts)
                .is_some_and(|pts| pts <= dts)
            {
                self.release(1);
            }
        }
    }

    /// Release up to `n` held pictures (the lowest order counts first).
    fn release(&mut self, n: usize) {
        let n = n.min(self.held.len());
        for h in self.held.drain(..n) {
            self.output_queue.push_back(h.frame);
        }
    }

    /// Error recorded by the output callback since the last check
    /// (taken, so one bad access unit does not latch the session).
    fn take_error(&self) -> Option<Error> {
        self.state
            .lock()
            .ok()
            .and_then(|mut g| g.error.take())
            .map(|e| {
                if e.contains("kVTVideoDecoderUnsupportedDataFormatErr")
                    || e.contains("kVTPixelTransferNotSupportedErr")
                {
                    Error::unsupported(e)
                } else {
                    Error::other(e)
                }
            })
    }
}

impl Drop for NalVtDecoder {
    fn drop(&mut self) {
        self.teardown_session(false);
    }
}

impl Decoder for NalVtDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.flushed = false;

        if let Some(e) = self.take_error() {
            return Err(e);
        }

        if let (Some(pts), Some(dts)) = (packet.pts, packet.dts) {
            if dts < pts {
                self.dts_timeline = true;
            }
            self.latest_dts = Some(dts);
        }

        let nals = packet_nals(&packet.data, self.params.length_size);
        let units = split_access_units(self.codec, &nals);

        for (idx, unit) in units.iter().enumerate() {
            let mut vcl: Vec<&[u8]> = Vec::with_capacity(unit.len());
            let mut first_vcl: Option<&[u8]> = None;
            for &nal in unit {
                let Some(t) = self.codec.nal_type(nal) else {
                    continue;
                };
                match self.codec.param_set_slot(t) {
                    Some(slot) => {
                        self.params.file(slot, nal);
                        self.order.file(slot, nal);
                    }
                    None => {
                        if self.codec == NalCodec::Hevc && matches!(t, 36 | 37) {
                            self.order.end_of_sequence();
                        }
                        if self.codec.is_vcl(t) && first_vcl.is_none() {
                            first_vcl = Some(nal);
                        }
                        vcl.push(nal);
                    }
                }
            }
            let Some(first_vcl) = first_vcl else {
                // No coded picture here (parameter sets / SEI / AUD only).
                continue;
            };
            // (Re)build the session on first use and on parameter-set change.
            self.ensure_session()?;
            if self.session.is_null() {
                // VCL data before any parameter set: nothing to decode with.
                continue;
            }
            let vt = sys::vtable().map_err(|e| Error::unsupported(format!("videotoolbox: {e}")))?;

            // Order count and sequence bookkeeping before submission.
            let poc = match self.order.picture(first_vcl) {
                Some((count, starts_sequence, depth)) => {
                    if starts_sequence {
                        self.period += 1;
                    }
                    self.depth = depth;
                    Some(count)
                }
                None => None,
            };
            // Only the first access unit of a packet carries the packet's
            // PTS; further pictures in the same packet (Annex-B streams
            // handed over in bulk) have none.
            let pts = if idx == 0 { packet.pts } else { None };
            let seq = self.next_seq;
            self.next_seq += 1;
            self.pending.insert(
                seq,
                Submitted {
                    period: self.period,
                    poc,
                    pts,
                },
            );
            // Forget stamps VideoToolbox never answered (dropped pictures).
            if self.pending.len() > 64 {
                let floor = seq - 64;
                self.pending.retain(|&k, _| k > floor);
            }
            submit_access_unit(
                vt,
                self.session,
                self.fmt_desc,
                &vcl,
                CMTime::make(seq, SEQ_TIMESCALE),
            )?;
        }

        self.pull_frames();
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(f) = self.output_queue.pop_front() {
            return Ok(Frame::Video(f));
        }
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        Err(if self.flushed {
            Error::Eof
        } else {
            Error::NeedMore
        })
    }

    fn flush(&mut self) -> Result<()> {
        if !self.session.is_null()
            && let Ok(vt) = sys::vtable()
        {
            // Barrier: waits for every outstanding callback.
            unsafe { (vt.vt_decomp_wait)(self.session) };
        }
        self.pull_frames();
        self.release(usize::MAX);
        self.flushed = true;
        if let Some(e) = self.take_error() {
            return Err(e);
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // Drop in-flight and queued output; keep the parameter sets and
        // the session (the next packet decodes from a fresh random-access
        // point).
        if !self.session.is_null()
            && let Ok(vt) = sys::vtable()
        {
            unsafe { (vt.vt_decomp_wait)(self.session) };
        }
        if let Ok(mut g) = self.state.lock() {
            g.frames.clear();
            g.error = None;
        }
        self.pending.clear();
        self.held.clear();
        self.output_queue.clear();
        self.period += 1;
        self.latest_dts = None;
        self.order = OrderState::new(self.codec);
        for (slot, set) in self.params.sets.clone().iter().enumerate() {
            for nal in set {
                self.order.file(slot, nal);
            }
        }
        self.flushed = false;
        Ok(())
    }

    fn output_pixel_format(&self) -> Option<PixelFormat> {
        self.target
    }
}

// ─────────────────────────── Historical factories ────────────────────────────

/// H.264 decoder factory (kept as a type for the historical API; the
/// implementation is [`NalVtDecoder`]).
pub struct H264VtDecoder;

impl H264VtDecoder {
    pub fn make(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
        NalVtDecoder::make(NalCodec::H264, params)
    }
}

/// HEVC decoder factory (kept as a type for the historical API; the
/// implementation is [`NalVtDecoder`]).
pub struct HevcVtDecoder;

impl HevcVtDecoder {
    pub fn make(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
        NalVtDecoder::make(NalCodec::Hevc, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS_H264: &[u8] = &[0x67, 0x64, 0x00, 0x0A, 0xAC, 0xD9, 0x47];
    const PPS_H264: &[u8] = &[0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];

    #[test]
    fn annex_b_and_length_prefixed_framing() {
        let mut annexb = vec![0, 0, 0, 1];
        annexb.extend_from_slice(SPS_H264);
        annexb.extend_from_slice(&[0, 0, 1]);
        annexb.extend_from_slice(PPS_H264);
        annexb.extend_from_slice(&[0x00]); // trailing_zero_8bits before a start code
        annexb.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x80, 0x00]);
        let nals = annex_b_nals(&annexb);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], SPS_H264);
        assert_eq!(
            nals[1], PPS_H264,
            "trailing zero before a start code trimmed"
        );
        assert_eq!(
            nals[2],
            &[0x65, 0x88, 0x80, 0x00][..],
            "final NAL kept verbatim"
        );
        assert!(starts_with_start_code(&annexb));

        let mut lp = Vec::new();
        for n in [SPS_H264, PPS_H264] {
            lp.extend_from_slice(&(n.len() as u32).to_be_bytes());
            lp.extend_from_slice(n);
        }
        lp.extend_from_slice(&[0, 0, 0, 9, 1, 2]); // overrun: dropped
        let nals = length_prefixed_nals(&lp, 4);
        assert_eq!(nals, vec![SPS_H264, PPS_H264]);
        assert!(!starts_with_start_code(&lp));
        assert_eq!(
            length_prefixed_nals(&[0, 2, 0xAA, 0xBB, 0, 1, 0xCC], 2).len(),
            2
        );

        // A 290-byte sample: its length prefix 00 00 01 22 looks like a
        // start code, yet with a known length size it is read as one NAL.
        let mut sample = vec![0, 0, 1, 0x22];
        sample.extend(std::iter::repeat_n(0x41u8, 0x122));
        assert_eq!(packet_nals(&sample, Some(4)).len(), 1);
        assert_eq!(packet_nals(&sample, Some(4))[0].len(), 0x122);
        // Annex B data under a known length size (in-band parameter sets
        // in a stream that mixes framings) still reads as Annex B.
        assert_eq!(packet_nals(&annexb, Some(4)).len(), 3);
        assert_eq!(packet_nals(&annexb, None).len(), 3);
        assert_eq!(packet_nals(&lp, None).len(), 2);
    }

    #[test]
    fn access_unit_split_on_first_mb_and_leading_non_vcl() {
        let idr0: &[u8] = &[0x65, 0x88, 0x84]; // first_mb_in_slice = 0
        let idr_slice2: &[u8] = &[0x65, 0x40, 0x00]; // first_mb_in_slice != 0
        let sei: &[u8] = &[0x06, 0x05, 0x01];
        let p0: &[u8] = &[0x41, 0x9A, 0x00];
        let units = split_access_units(
            NalCodec::H264,
            &[SPS_H264, PPS_H264, idr0, idr_slice2, sei, p0, p0],
        );
        assert_eq!(units.len(), 3);
        assert_eq!(units[0], vec![SPS_H264, PPS_H264, idr0, idr_slice2]);
        assert_eq!(units[1], vec![sei, p0]);
        assert_eq!(units[2], vec![p0]);

        // HEVC: first_slice_segment_in_pic_flag is bit 0 of byte 2.
        let vps: &[u8] = &[0x40, 0x01, 0x0C];
        let idr: &[u8] = &[0x26, 0x01, 0xAF];
        let trail_first: &[u8] = &[0x02, 0x01, 0xD0];
        let trail_more: &[u8] = &[0x02, 0x01, 0x50];
        let units = split_access_units(
            NalCodec::Hevc,
            &[vps, idr, trail_first, trail_more, trail_first],
        );
        assert_eq!(units.len(), 3);
        assert_eq!(units[1], vec![trail_first, trail_more]);
    }

    #[test]
    fn avcc_and_hvcc_records_parse() {
        let mut avcc = vec![1, 0x64, 0x00, 0x0A, 0xFF, 0xE1];
        avcc.extend_from_slice(&(SPS_H264.len() as u16).to_be_bytes());
        avcc.extend_from_slice(SPS_H264);
        avcc.push(1);
        avcc.extend_from_slice(&(PPS_H264.len() as u16).to_be_bytes());
        avcc.extend_from_slice(PPS_H264);
        let ps = ParamSets::from_extradata(NalCodec::H264, &avcc).expect("avcC");
        assert_eq!(ps.length_size, Some(4));
        assert_eq!(ps.sets[1], vec![SPS_H264.to_vec()]);
        assert_eq!(ps.sets[2], vec![PPS_H264.to_vec()]);
        assert!(ParamSets::from_extradata(NalCodec::H264, &avcc[..10]).is_none());

        let vps: &[u8] = &[0x40, 0x01, 0x0C, 0x01];
        let sps: &[u8] = &[0x42, 0x01, 0x01, 0x01];
        let pps: &[u8] = &[0x44, 0x01, 0xC0];
        let mut hvcc = vec![1u8; 22];
        hvcc[21] = 0x0F; // lengthSizeMinusOne = 3
        hvcc.push(3);
        for (t, n) in [(32u8, vps), (33, sps), (34, pps)] {
            hvcc.push(0x80 | t);
            hvcc.extend_from_slice(&1u16.to_be_bytes());
            hvcc.extend_from_slice(&(n.len() as u16).to_be_bytes());
            hvcc.extend_from_slice(n);
        }
        let ps = ParamSets::from_extradata(NalCodec::Hevc, &hvcc).expect("hvcC");
        assert_eq!(ps.length_size, Some(4));
        assert_eq!(
            ps.sets,
            [vec![vps.to_vec()], vec![sps.to_vec()], vec![pps.to_vec()]]
        );

        // Annex-B extradata is accepted too; empty is None.
        let mut annexb = vec![0, 0, 0, 1];
        annexb.extend_from_slice(SPS_H264);
        annexb.extend_from_slice(&[0, 0, 0, 1]);
        annexb.extend_from_slice(PPS_H264);
        let ps = ParamSets::from_extradata(NalCodec::H264, &annexb).expect("annex b");
        assert_eq!(ps.length_size, None);
        assert!(ps.is_complete());
        assert!(ParamSets::from_extradata(NalCodec::H264, &[]).is_none());
    }
}
