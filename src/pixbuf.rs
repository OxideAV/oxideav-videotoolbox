//! `CVPixelBuffer` ⇄ [`VideoFrame`] bridge shared by every decoder.
//!
//! # Truthful labels
//!
//! A decoder's output frames are labelled with the layout it **actually
//! produces**. The decoders therefore negotiate one *target* label up
//! front (the container's declared [`PixelFormat`] when VideoToolbox can
//! emit it, otherwise the codec's native layout), ask VideoToolbox for the
//! matching `kCVPixelFormatType_*` through
//! `destinationImageBufferAttributes`, and convert whatever comes back
//! into planar frames of exactly that label. The two layouts VideoToolbox
//! has no pixel format for (4:1:1 and 4:4:0) are derived from a 4:4:4
//! request by a chroma re-sampling in software so the frame geometry
//! still matches the label.
//!
//! Formats VideoToolbox cannot be asked for at all (12-bit planar, CMYK,
//! RGB, alpha planes, …) make [`request_for_label`] return `None`; the
//! decoder factories turn that into a typed `Unsupported` error so the
//! registry falls back to the pure-Rust implementation instead of
//! emitting mislabelled frames.
//!
//! Numeric `kCVPixelFormatType_*` values are the four-character codes
//! declared in the platform SDK header `CoreVideo/CVPixelBuffer.h`.

use oxideav_core::{PixelFormat, VideoFrame, VideoPlane};

use crate::sys;

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange` (`'420v'`).
pub const FOURCC_420V: u32 = 0x3432_3076;
/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange` (`'420f'`).
pub const FOURCC_420F: u32 = 0x3432_3066;
/// `kCVPixelFormatType_422YpCbCr8BiPlanarVideoRange` (`'422v'`).
pub const FOURCC_422V: u32 = 0x3432_3276;
/// `kCVPixelFormatType_422YpCbCr8BiPlanarFullRange` (`'422f'`).
pub const FOURCC_422F: u32 = 0x3432_3266;
/// `kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange` (`'444v'`).
pub const FOURCC_444V: u32 = 0x3434_3476;
/// `kCVPixelFormatType_444YpCbCr8BiPlanarFullRange` (`'444f'`).
pub const FOURCC_444F: u32 = 0x3434_3466;
/// `kCVPixelFormatType_420YpCbCr8Planar` (`'y420'`).
pub const FOURCC_Y420: u32 = 0x7934_3230;
/// `kCVPixelFormatType_420YpCbCr8PlanarFullRange` (`'f420'`).
pub const FOURCC_F420: u32 = 0x6634_3230;
/// `kCVPixelFormatType_422YpCbCr8` (`'2vuy'`, packed Cb Y0 Cr Y1).
pub const FOURCC_2VUY: u32 = 0x3276_7579;
/// `kCVPixelFormatType_422YpCbCr8_yuvs` (`'yuvs'`, packed Y0 Cb Y1 Cr).
pub const FOURCC_YUVS: u32 = 0x7975_7673;
/// `kCVPixelFormatType_OneComponent8` (`'L008'`).
pub const FOURCC_L008: u32 = 0x4c30_3038;
/// `kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange` (`'x420'`).
pub const FOURCC_X420: u32 = 0x7834_3230;
/// `kCVPixelFormatType_420YpCbCr10BiPlanarFullRange` (`'xf20'`).
pub const FOURCC_XF20: u32 = 0x7866_3230;
/// `kCVPixelFormatType_422YpCbCr10BiPlanarVideoRange` (`'x422'`).
pub const FOURCC_X422: u32 = 0x7834_3232;
/// `kCVPixelFormatType_422YpCbCr10BiPlanarFullRange` (`'xf22'`).
pub const FOURCC_XF22: u32 = 0x7866_3232;
/// `kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange` (`'x444'`).
pub const FOURCC_X444: u32 = 0x7834_3434;
/// `kCVPixelFormatType_444YpCbCr10BiPlanarFullRange` (`'xf44'`).
pub const FOURCC_XF44: u32 = 0x7866_3434;
/// `kCVPixelFormatType_422YpCbCr16BiPlanarVideoRange` (`'sv22'`).
pub const FOURCC_SV22: u32 = 0x7376_3232;
/// `kCVPixelFormatType_444YpCbCr16BiPlanarVideoRange` (`'sv44'`).
pub const FOURCC_SV44: u32 = 0x7376_3434;
/// `kCVPixelFormatType_422YpCbCr16` (`'v216'`, packed Cb Y0 Cr Y1, 16-bit).
pub const FOURCC_V216: u32 = 0x7632_3136;
/// `kCVPixelFormatType_32BGRA` (`'BGRA'`).
pub const FOURCC_BGRA: u32 = 0x4247_5241;

/// Four printable characters of a `kCVPixelFormatType_*` code.
pub fn fourcc_str(fourcc: u32) -> String {
    fourcc
        .to_be_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

/// The pixel format to request from VideoToolbox so that the frames can
/// be delivered as `label`, and the label VideoToolbox's buffer natively
/// maps to (equal to `label` except for the two software-derived
/// layouts). `None` when VideoToolbox has no pixel format that can carry
/// `label` without lying about it.
///
/// `full_range` picks the full-range flavour of the 8-bit bi-planar
/// families for the range-agnostic labels (`Yuv4xxP`); the `YuvJ4xxP`
/// labels always map to the full-range flavour, and the deeper labels
/// have no full-range twin in the core enum so their range is left to
/// the stream's colour signal.
pub fn request_for_label(label: PixelFormat, full_range: bool) -> Option<(u32, PixelFormat)> {
    use PixelFormat as P;
    Some(match label {
        P::Yuv420P if full_range => (FOURCC_420F, P::Yuv420P),
        P::Yuv420P => (FOURCC_420V, P::Yuv420P),
        P::YuvJ420P => (FOURCC_420F, P::YuvJ420P),
        P::Yuv422P if full_range => (FOURCC_422F, P::Yuv422P),
        P::Yuv422P => (FOURCC_422V, P::Yuv422P),
        P::YuvJ422P => (FOURCC_422F, P::YuvJ422P),
        P::Yuv444P if full_range => (FOURCC_444F, P::Yuv444P),
        P::Yuv444P => (FOURCC_444V, P::Yuv444P),
        P::YuvJ444P => (FOURCC_444F, P::YuvJ444P),
        // No 4:1:1 / 4:4:0 pixel format exists: decode at 4:4:4 (the
        // lossless superset) and re-sample chroma in software.
        P::Yuv411P if full_range => (FOURCC_444F, P::Yuv444P),
        P::Yuv411P => (FOURCC_444V, P::Yuv444P),
        P::Yuv440P if full_range => (FOURCC_444F, P::Yuv444P),
        P::Yuv440P => (FOURCC_444V, P::Yuv444P),
        P::Gray8 => (FOURCC_L008, P::Gray8),
        P::Yuv420P10Le if full_range => (FOURCC_XF20, P::Yuv420P10Le),
        P::Yuv420P10Le => (FOURCC_X420, P::Yuv420P10Le),
        P::Yuv422P10Le if full_range => (FOURCC_XF22, P::Yuv422P10Le),
        P::Yuv422P10Le => (FOURCC_X422, P::Yuv422P10Le),
        P::Yuv444P10Le if full_range => (FOURCC_XF44, P::Yuv444P10Le),
        P::Yuv444P10Le => (FOURCC_X444, P::Yuv444P10Le),
        P::Yuv422P16Le => (FOURCC_SV22, P::Yuv422P16Le),
        P::Yuv444P16Le => (FOURCC_SV44, P::Yuv444P16Le),
        P::Bgra => (FOURCC_BGRA, P::Bgra),
        _ => return None,
    })
}

/// The label a `CVPixelBuffer` of pixel format `fourcc` converts to
/// (planar, see [`pixel_buffer_to_frame`]). `None` for formats this
/// module does not read.
pub fn label_for_fourcc(fourcc: u32) -> Option<PixelFormat> {
    use PixelFormat as P;
    Some(match fourcc {
        FOURCC_420V | FOURCC_Y420 => P::Yuv420P,
        FOURCC_420F | FOURCC_F420 => P::YuvJ420P,
        FOURCC_422V | FOURCC_2VUY | FOURCC_YUVS => P::Yuv422P,
        FOURCC_422F => P::YuvJ422P,
        FOURCC_444V => P::Yuv444P,
        FOURCC_444F => P::YuvJ444P,
        FOURCC_L008 => P::Gray8,
        FOURCC_X420 | FOURCC_XF20 => P::Yuv420P10Le,
        FOURCC_X422 | FOURCC_XF22 => P::Yuv422P10Le,
        FOURCC_X444 | FOURCC_XF44 => P::Yuv444P10Le,
        FOURCC_SV22 | FOURCC_V216 => P::Yuv422P16Le,
        FOURCC_SV44 => P::Yuv444P16Le,
        FOURCC_BGRA => P::Bgra,
        _ => return None,
    })
}

/// `true` when two labels describe the same sample layout and differ
/// only in the range the name implies (`Yuv420P` vs `YuvJ420P`).
pub fn same_layout(a: PixelFormat, b: PixelFormat) -> bool {
    use PixelFormat as P;
    a == b
        || matches!(
            (a, b),
            (P::Yuv420P, P::YuvJ420P)
                | (P::YuvJ420P, P::Yuv420P)
                | (P::Yuv422P, P::YuvJ422P)
                | (P::YuvJ422P, P::Yuv422P)
                | (P::Yuv444P, P::YuvJ444P)
                | (P::YuvJ444P, P::Yuv444P)
        )
}

/// Copy `rows` rows of `row_bytes` bytes out of a plane with stride
/// `bpr` into a tightly-packed vector (rows past the buffer are zero —
/// VideoToolbox buffers always cover the picture, this is defensive).
///
/// # Safety
/// `base` must point to at least `bpr * rows` readable bytes (or be
/// null, in which case the plane comes back zeroed).
unsafe fn copy_rows(base: *const u8, bpr: usize, rows: usize, row_bytes: usize) -> Vec<u8> {
    let mut out = vec![0u8; row_bytes * rows];
    if base.is_null() {
        return out;
    }
    let take = row_bytes.min(bpr);
    for r in 0..rows {
        let src = unsafe { std::slice::from_raw_parts(base.add(r * bpr), take) };
        out[r * row_bytes..r * row_bytes + take].copy_from_slice(src);
    }
    out
}

/// Split an interleaved `CbCr` plane (`bytes_per_sample` 1 or 2, little
/// endian when 2) into two planar chroma planes of `cw × ch` samples,
/// shifting each sample right by `shift` bits (10-bit-in-MSBs formats).
///
/// # Safety
/// `base` must point to at least `bpr * ch` readable bytes (or be null).
unsafe fn split_chroma(
    base: *const u8,
    bpr: usize,
    cw: usize,
    ch: usize,
    bytes_per_sample: usize,
    shift: u32,
) -> (Vec<u8>, Vec<u8>) {
    let row_bytes = cw * bytes_per_sample;
    let mut u = vec![0u8; row_bytes * ch];
    let mut v = vec![0u8; row_bytes * ch];
    if base.is_null() {
        return (u, v);
    }
    let pairs = (bpr / (2 * bytes_per_sample)).min(cw);
    for r in 0..ch {
        let row =
            unsafe { std::slice::from_raw_parts(base.add(r * bpr), pairs * 2 * bytes_per_sample) };
        let (ur, vr) = (
            &mut u[r * row_bytes..(r + 1) * row_bytes],
            &mut v[r * row_bytes..(r + 1) * row_bytes],
        );
        if bytes_per_sample == 1 {
            for c in 0..pairs {
                ur[c] = row[2 * c];
                vr[c] = row[2 * c + 1];
            }
        } else {
            for c in 0..pairs {
                let cb = u16::from_le_bytes([row[4 * c], row[4 * c + 1]]) >> shift;
                let cr = u16::from_le_bytes([row[4 * c + 2], row[4 * c + 3]]) >> shift;
                ur[2 * c..2 * c + 2].copy_from_slice(&cb.to_le_bytes());
                vr[2 * c..2 * c + 2].copy_from_slice(&cr.to_le_bytes());
            }
        }
    }
    (u, v)
}

/// Shift every little-endian 16-bit word of `plane` right by `shift`.
fn shift_words(plane: &mut [u8], shift: u32) {
    if shift == 0 {
        return;
    }
    for w in plane.as_chunks_mut::<2>().0 {
        let v = u16::from_le_bytes([w[0], w[1]]) >> shift;
        w.copy_from_slice(&v.to_le_bytes());
    }
}

fn plane(stride: usize, data: Vec<u8>) -> VideoPlane {
    VideoPlane { stride, data }
}

/// Convert a locked `CVPixelBuffer` into a planar [`VideoFrame`] and the
/// label that frame carries. Every `kCVPixelFormatType_*` named in
/// [`label_for_fourcc`] is handled; anything else is an error naming
/// the code so the caller can report it.
///
/// Bi-planar formats come back as three planes (Y, Cb, Cr); packed
/// 4:2:2 is de-interleaved to planar 4:2:2 (no vertical averaging —
/// the frame keeps every chroma row the decoder produced); 10-bit
/// MSB-aligned formats are shifted down to the low 10 bits of each
/// 16-bit word (`*P10Le`); 16-bit formats are copied verbatim
/// (`*P16Le`); `'L008'` is one grey plane; `'BGRA'` one packed plane.
///
/// # Safety
/// `image_buffer` must be a valid `CVPixelBuffer` whose base address is
/// locked for the duration of the call.
pub unsafe fn pixel_buffer_to_frame(
    vt: &sys::Vtable,
    image_buffer: sys::CVImageBufferRef,
) -> Result<(VideoFrame, PixelFormat), String> {
    let width = unsafe { (vt.cv_pb_get_width)(image_buffer) };
    let height = unsafe { (vt.cv_pb_get_height)(image_buffer) };
    let fourcc = unsafe { (vt.cv_pb_get_pixel_format)(image_buffer) };
    let label = label_for_fourcc(fourcc).ok_or_else(|| {
        format!(
            "unsupported CVPixelBuffer format '{}' (0x{fourcc:08x}, {width}x{height})",
            fourcc_str(fourcc)
        )
    })?;
    if width == 0 || height == 0 {
        return Err(format!(
            "CVPixelBuffer with empty geometry {width}x{height}"
        ));
    }

    let plane_ptr =
        |i: usize| unsafe { (vt.cv_pb_get_base_of_plane)(image_buffer, i) } as *const u8;
    let plane_bpr = |i: usize| unsafe { (vt.cv_pb_get_bpr_of_plane)(image_buffer, i) };

    let planes = match fourcc {
        // 8-bit bi-planar: Y, then interleaved CbCr at the chroma grid.
        FOURCC_420V | FOURCC_420F | FOURCC_422V | FOURCC_422F | FOURCC_444V | FOURCC_444F => {
            let (cw, ch) = chroma_dims(label, width, height);
            let y = unsafe { copy_rows(plane_ptr(0), plane_bpr(0), height, width) };
            let (u, v) = unsafe { split_chroma(plane_ptr(1), plane_bpr(1), cw, ch, 1, 0) };
            vec![plane(width, y), plane(cw, u), plane(cw, v)]
        }
        // 8-bit planar 4:2:0: three planes already.
        FOURCC_Y420 | FOURCC_F420 => {
            let (cw, ch) = chroma_dims(label, width, height);
            let y = unsafe { copy_rows(plane_ptr(0), plane_bpr(0), height, width) };
            let u = unsafe { copy_rows(plane_ptr(1), plane_bpr(1), ch, cw) };
            let v = unsafe { copy_rows(plane_ptr(2), plane_bpr(2), ch, cw) };
            vec![plane(width, y), plane(cw, u), plane(cw, v)]
        }
        // Packed 8-bit 4:2:2 → planar 4:2:2.
        FOURCC_2VUY | FOURCC_YUVS => {
            let base = unsafe { (vt.cv_pb_get_base)(image_buffer) } as *const u8;
            let bpr = unsafe { (vt.cv_pb_get_bpr)(image_buffer) };
            let cw = width.div_ceil(2);
            let mut y = vec![0u8; width * height];
            let mut u = vec![0u8; cw * height];
            let mut v = vec![0u8; cw * height];
            if !base.is_null() {
                let (yo, co) = if fourcc == FOURCC_2VUY {
                    (1, 0)
                } else {
                    (0, 1)
                };
                for r in 0..height {
                    let row = unsafe { std::slice::from_raw_parts(base.add(r * bpr), bpr) };
                    for c in 0..cw {
                        let b = 4 * c;
                        if b + 4 > bpr {
                            break;
                        }
                        y[r * width + 2 * c] = row[b + yo];
                        if 2 * c + 1 < width {
                            y[r * width + 2 * c + 1] = row[b + 2 + yo];
                        }
                        u[r * cw + c] = row[b + co];
                        v[r * cw + c] = row[b + 2 + co];
                    }
                }
            }
            vec![plane(width, y), plane(cw, u), plane(cw, v)]
        }
        // One 8-bit component.
        FOURCC_L008 => {
            let base = unsafe { (vt.cv_pb_get_base)(image_buffer) } as *const u8;
            let bpr = unsafe { (vt.cv_pb_get_bpr)(image_buffer) };
            vec![plane(width, unsafe { copy_rows(base, bpr, height, width) })]
        }
        // 10-bit-in-MSBs bi-planar → low-10-bit words.
        FOURCC_X420 | FOURCC_XF20 | FOURCC_X422 | FOURCC_XF22 | FOURCC_X444 | FOURCC_XF44 => {
            let (cw, ch) = chroma_dims(label, width, height);
            let mut y = unsafe { copy_rows(plane_ptr(0), plane_bpr(0), height, width * 2) };
            shift_words(&mut y, 6);
            let (u, v) = unsafe { split_chroma(plane_ptr(1), plane_bpr(1), cw, ch, 2, 6) };
            vec![plane(width * 2, y), plane(cw * 2, u), plane(cw * 2, v)]
        }
        // 16-bit bi-planar, copied verbatim.
        FOURCC_SV22 | FOURCC_SV44 => {
            let (cw, ch) = chroma_dims(label, width, height);
            let y = unsafe { copy_rows(plane_ptr(0), plane_bpr(0), height, width * 2) };
            let (u, v) = unsafe { split_chroma(plane_ptr(1), plane_bpr(1), cw, ch, 2, 0) };
            vec![plane(width * 2, y), plane(cw * 2, u), plane(cw * 2, v)]
        }
        // Packed 16-bit 4:2:2 (Cb0 Y0 Cr0 Y1 per 2 pixels) → planar.
        FOURCC_V216 => {
            let base = unsafe { (vt.cv_pb_get_base)(image_buffer) } as *const u8;
            let bpr = unsafe { (vt.cv_pb_get_bpr)(image_buffer) };
            let cw = width.div_ceil(2);
            let mut y = vec![0u8; width * height * 2];
            let mut u = vec![0u8; cw * height * 2];
            let mut v = vec![0u8; cw * height * 2];
            if !base.is_null() {
                for r in 0..height {
                    let row = unsafe { std::slice::from_raw_parts(base.add(r * bpr), bpr) };
                    for c in 0..cw {
                        let b = 8 * c;
                        if b + 8 > bpr {
                            break;
                        }
                        u[(r * cw + c) * 2..(r * cw + c) * 2 + 2].copy_from_slice(&row[b..b + 2]);
                        y[(r * width + 2 * c) * 2..(r * width + 2 * c) * 2 + 2]
                            .copy_from_slice(&row[b + 2..b + 4]);
                        v[(r * cw + c) * 2..(r * cw + c) * 2 + 2]
                            .copy_from_slice(&row[b + 4..b + 6]);
                        if 2 * c + 1 < width {
                            y[(r * width + 2 * c + 1) * 2..(r * width + 2 * c + 1) * 2 + 2]
                                .copy_from_slice(&row[b + 6..b + 8]);
                        }
                    }
                }
            }
            vec![plane(width * 2, y), plane(cw * 2, u), plane(cw * 2, v)]
        }
        // Packed BGRA.
        FOURCC_BGRA => {
            let base = unsafe { (vt.cv_pb_get_base)(image_buffer) } as *const u8;
            let bpr = unsafe { (vt.cv_pb_get_bpr)(image_buffer) };
            vec![plane(width * 4, unsafe {
                copy_rows(base, bpr, height, width * 4)
            })]
        }
        _ => unreachable!("label_for_fourcc admitted an unhandled code"),
    };

    Ok((VideoFrame { pts: None, planes }, label))
}

/// Chroma plane dimensions of `label` for a `width × height` picture.
fn chroma_dims(label: PixelFormat, width: usize, height: usize) -> (usize, usize) {
    match label.chroma_subsampling() {
        Some((sx, sy)) => (width.div_ceil(1 << sx), height.div_ceil(1 << sy)),
        None => (width, height),
    }
}

/// Re-sample a planar 4:4:4 frame (`from` ∈ {`Yuv444P`, `YuvJ444P`}) into
/// the 4:1:1 or 4:4:0 layout `to` asks for, averaging the chroma samples
/// each target position covers (edge positions average the samples that
/// exist). `None` when the pair is not one of those conversions.
pub fn resample_from_444(
    frame: &VideoFrame,
    from: PixelFormat,
    to: PixelFormat,
) -> Option<VideoFrame> {
    use PixelFormat as P;
    if !matches!(from, P::Yuv444P | P::YuvJ444P) || frame.planes.len() < 3 {
        return None;
    }
    let (fx, fy) = match to {
        P::Yuv411P => (4usize, 1usize),
        P::Yuv440P => (1, 2),
        _ => return None,
    };
    let width = frame.planes[0].stride;
    if width == 0 {
        return None;
    }
    let height = frame.planes[0].data.len() / width;
    let (cw, ch) = (width.div_ceil(fx), height.div_ceil(fy));
    let shrink = |src: &VideoPlane| -> VideoPlane {
        let mut out = vec![0u8; cw * ch];
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut sum, mut n) = (0u32, 0u32);
                for dy in 0..fy {
                    let y = cy * fy + dy;
                    if y >= height {
                        break;
                    }
                    for dx in 0..fx {
                        let x = cx * fx + dx;
                        if x >= width {
                            break;
                        }
                        sum += src.data[y * src.stride + x] as u32;
                        n += 1;
                    }
                }
                out[cy * cw + cx] = ((sum + n / 2) / n.max(1)) as u8;
            }
        }
        VideoPlane {
            stride: cw,
            data: out,
        }
    };
    Some(VideoFrame {
        pts: frame.pts,
        planes: vec![
            frame.planes[0].clone(),
            shrink(&frame.planes[1]),
            shrink(&frame.planes[2]),
        ],
    })
}

/// Bring a frame delivered as `native` to the negotiated `target` label:
/// identity when the layouts agree (a range-only rename is free), the
/// 4:4:4 → 4:1:1 / 4:4:0 re-sampling otherwise. `None` when no
/// conversion exists — the caller reports a mismatch.
pub fn deliver_as(
    frame: VideoFrame,
    native: PixelFormat,
    target: PixelFormat,
) -> Option<VideoFrame> {
    if same_layout(native, target) {
        return Some(frame);
    }
    resample_from_444(&frame, native, target)
}

/// `true` when `frame`'s image planes have exactly the tight geometry
/// `label` prescribes for a `width × height` picture.
pub fn geometry_matches(frame: &VideoFrame, label: PixelFormat, width: u32, height: u32) -> bool {
    let n = label.plane_count();
    let planes: &[VideoPlane] = frame.image_planes();
    if planes.len() != n {
        return false;
    }
    planes.iter().enumerate().all(|(i, p)| {
        let Some(row) = label.plane_row_bytes(i, width) else {
            return false;
        };
        let Some((_, rows)) = label.plane_dimensions(i, width, height) else {
            return false;
        };
        p.stride == row && p.data.len() == row * rows as usize
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_map_is_truthful() {
        assert_eq!(
            request_for_label(PixelFormat::YuvJ444P, false),
            Some((FOURCC_444F, PixelFormat::YuvJ444P))
        );
        assert_eq!(
            request_for_label(PixelFormat::Yuv420P, false),
            Some((FOURCC_420V, PixelFormat::Yuv420P))
        );
        assert_eq!(
            request_for_label(PixelFormat::Yuv420P, true),
            Some((FOURCC_420F, PixelFormat::Yuv420P))
        );
        assert_eq!(
            request_for_label(PixelFormat::Yuv411P, true),
            Some((FOURCC_444F, PixelFormat::Yuv444P))
        );
        assert_eq!(request_for_label(PixelFormat::Cmyk, true), None);
        assert_eq!(request_for_label(PixelFormat::Yuv444P12Le, false), None);
        for (fourcc, label) in [
            (FOURCC_420V, PixelFormat::Yuv420P),
            (FOURCC_444F, PixelFormat::YuvJ444P),
            (FOURCC_L008, PixelFormat::Gray8),
            (FOURCC_X422, PixelFormat::Yuv422P10Le),
            (FOURCC_SV44, PixelFormat::Yuv444P16Le),
        ] {
            assert_eq!(
                label_for_fourcc(fourcc),
                Some(label),
                "{}",
                fourcc_str(fourcc)
            );
        }
        assert_eq!(fourcc_str(FOURCC_420V), "420v");
    }

    #[test]
    fn resample_444_to_411_and_440_keeps_geometry() {
        let (w, h) = (9usize, 5usize);
        let y: Vec<u8> = (0..w * h).map(|i| i as u8).collect();
        let c: Vec<u8> = (0..w * h).map(|i| (i % 7) as u8 * 30).collect();
        let f = VideoFrame {
            pts: Some(3),
            planes: vec![
                VideoPlane { stride: w, data: y },
                VideoPlane {
                    stride: w,
                    data: c.clone(),
                },
                VideoPlane { stride: w, data: c },
            ],
        };
        let r = resample_from_444(&f, PixelFormat::YuvJ444P, PixelFormat::Yuv411P).unwrap();
        assert!(geometry_matches(
            &r,
            PixelFormat::Yuv411P,
            w as u32,
            h as u32
        ));
        assert_eq!(r.pts, Some(3));
        // First 4:1:1 chroma sample = mean of the first four 4:4:4 samples.
        assert_eq!(r.planes[1].data[0], ((30 + 60 + 90 + 2) / 4) as u8);
        let r = resample_from_444(&f, PixelFormat::Yuv444P, PixelFormat::Yuv440P).unwrap();
        assert!(geometry_matches(
            &r,
            PixelFormat::Yuv440P,
            w as u32,
            h as u32
        ));
        assert!(resample_from_444(&f, PixelFormat::Yuv420P, PixelFormat::Yuv411P).is_none());
        assert!(deliver_as(f.clone(), PixelFormat::Yuv444P, PixelFormat::YuvJ444P).is_some());
        assert!(deliver_as(f, PixelFormat::Yuv444P, PixelFormat::Yuv420P).is_none());
    }

    #[test]
    fn shift_words_moves_msb_aligned_samples_down() {
        let mut p = 0x8000u16.to_le_bytes().to_vec();
        shift_words(&mut p, 6);
        assert_eq!(u16::from_le_bytes([p[0], p[1]]), 0x200);
    }
}
