//! Just enough H.264 / H.265 header parsing to put VideoToolbox's output
//! back into display order.
//!
//! VideoToolbox hands decoded pictures back in *decode* order (measured
//! on the M-series media engine: `kVTDecodeFrame_EnableTemporalProcessing`
//! only defers the callbacks, it never re-orders them, whatever the
//! presentation stamps say). The pure-Rust decoders emit display order,
//! so the bridge derives each picture's order count from its slice
//! header — ITU-T H.264 §8.2.1 (all three `pic_order_cnt_type`s) and
//! ITU-T H.265 §8.3.1 — and holds pictures in a bounded buffer sized by
//! the stream's own reorder depth (H.264 VUI `max_num_reorder_frames`,
//! else the level's `MaxDpbFrames` per Table A-1; H.265
//! `sps_max_num_reorder_pics`).
//!
//! Only the syntax elements on the way to those values are read: SPS
//! §7.3.2.1.1 (with the scaling lists and VUI walked over), PPS §7.3.2.2
//! up to `bottom_field_pic_order_in_frame_present_flag`, slice header
//! §7.3.3 up to the order-count fields, and the H.265 twins §7.3.2.2.1 /
//! §7.3.2.3.1 / §7.3.6.1.
//!
//! Known limit: a `memory_management_control_operation` equal to 5 sits
//! past the reference-list syntax and is not detected; the order-count
//! history is reset at IDR pictures only.

use std::collections::HashMap;

use oxideav_core::PixelFormat;

/// Exp-Golomb bit reader over an RBSP (emulation-prevention bytes removed).
pub struct BitReader {
    data: Vec<u8>,
    pos: usize,
}

impl BitReader {
    /// Reader over the payload that follows a NAL unit header.
    pub fn new(nal_payload: &[u8]) -> Self {
        // Remove emulation_prevention_three_byte (§7.4.1 / H.265 §7.4.2).
        let mut data = Vec::with_capacity(nal_payload.len());
        let mut zeros = 0usize;
        for &b in nal_payload {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            data.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        Self { data, pos: 0 }
    }

    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(bit as u32)
    }

    pub fn flag(&mut self) -> Option<bool> {
        self.bit().map(|b| b == 1)
    }

    pub fn bits(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.pos + n > self.data.len() * 8 {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// `ue(v)` (§9.1).
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0usize;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let suffix = self.bits(zeros)?;
        Some((1u32 << zeros) - 1 + suffix)
    }

    /// `se(v)` (§9.1.1, Table 9-3): codeNum k ↦ (−1)^(k+1) ⌈k/2⌉.
    pub fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        let v = (k + 1) / 2;
        Some(if k % 2 == 1 { v } else { -v } as i32)
    }
}

// ─────────────────────────── H.264 ────────────────────────────────────────────

/// The H.264 sequence parameter set fields this crate uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct H264Sps {
    pub id: u32,
    pub profile_idc: u32,
    pub level_idc: u32,
    pub chroma_format_idc: u32,
    pub bit_depth: u32,
    pub separate_colour_plane: bool,
    pub log2_max_frame_num: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub pic_width_in_mbs: u32,
    pub frame_height_in_mbs: u32,
    pub frame_mbs_only: bool,
    /// VUI `max_num_reorder_frames` when `bitstream_restriction_flag`.
    pub max_num_reorder_frames: Option<u32>,
}

impl H264Sps {
    /// Parse an SPS NAL unit (header byte included), §7.3.2.1.1.
    pub fn parse(nal: &[u8]) -> Option<Self> {
        let mut r = BitReader::new(nal.get(1..)?);
        let profile_idc = r.bits(8)?;
        r.skip(8)?; // constraint_set0..5 + reserved_zero_2bits
        let level_idc = r.bits(8)?;
        let id = r.ue()?;
        let (mut chroma_format_idc, mut bit_depth, mut separate_colour_plane) = (1, 8, false);
        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            chroma_format_idc = r.ue()?;
            if chroma_format_idc == 3 {
                separate_colour_plane = r.flag()?;
            }
            bit_depth = r.ue()? + 8;
            r.ue()?; // bit_depth_chroma_minus8
            r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
            if r.flag()? {
                // seq_scaling_matrix_present_flag
                let lists = if chroma_format_idc != 3 { 8 } else { 12 };
                for i in 0..lists {
                    if r.flag()? {
                        skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }
        let log2_max_frame_num = r.ue()? + 4;
        let pic_order_cnt_type = r.ue()?;
        let mut log2_max_pic_order_cnt_lsb = 0;
        let mut delta_pic_order_always_zero = false;
        let mut offset_for_non_ref_pic = 0;
        let mut offset_for_top_to_bottom_field = 0;
        let mut offset_for_ref_frame = Vec::new();
        if pic_order_cnt_type == 0 {
            log2_max_pic_order_cnt_lsb = r.ue()? + 4;
        } else if pic_order_cnt_type == 1 {
            delta_pic_order_always_zero = r.flag()?;
            offset_for_non_ref_pic = r.se()?;
            offset_for_top_to_bottom_field = r.se()?;
            let n = r.ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                offset_for_ref_frame.push(r.se()?);
            }
        }
        let max_num_ref_frames = r.ue()?;
        r.skip(1)?; // gaps_in_frame_num_value_allowed_flag
        let pic_width_in_mbs = r.ue()? + 1;
        let pic_height_in_map_units = r.ue()? + 1;
        let frame_mbs_only = r.flag()?;
        if !frame_mbs_only {
            r.skip(1)?; // mb_adaptive_frame_field_flag
        }
        r.skip(1)?; // direct_8x8_inference_flag
        if r.flag()? {
            // frame_cropping_flag
            for _ in 0..4 {
                r.ue()?;
            }
        }
        let mut max_num_reorder_frames = None;
        if r.flag()? {
            // vui_parameters_present_flag — walked only as far as needed;
            // a truncated VUI just leaves the depth unknown.
            max_num_reorder_frames = parse_vui_reorder(&mut r);
        }
        Some(H264Sps {
            id,
            profile_idc,
            level_idc,
            chroma_format_idc,
            bit_depth,
            separate_colour_plane,
            log2_max_frame_num,
            pic_order_cnt_type,
            log2_max_pic_order_cnt_lsb,
            delta_pic_order_always_zero,
            offset_for_non_ref_pic,
            offset_for_top_to_bottom_field,
            offset_for_ref_frame,
            max_num_ref_frames,
            pic_width_in_mbs,
            frame_height_in_mbs: pic_height_in_map_units * if frame_mbs_only { 1 } else { 2 },
            frame_mbs_only,
            max_num_reorder_frames,
        })
    }

    /// Planar label of the coded layout (`None` for depths the core enum
    /// does not name).
    pub fn pixel_format(&self) -> Option<PixelFormat> {
        layout_label(self.chroma_format_idc, self.bit_depth)
    }

    /// Pictures that may precede a picture in decode order yet follow it
    /// in output order: VUI `max_num_reorder_frames` when signalled,
    /// otherwise 0 for `pic_order_cnt_type == 2` (output order is decode
    /// order, §8.2.1.3) and the level's `MaxDpbFrames` (Table A-1
    /// `MaxDpbMbs` over the frame size, at most 16) for the rest.
    pub fn reorder_depth(&self) -> usize {
        if let Some(n) = self.max_num_reorder_frames {
            return n.min(16) as usize;
        }
        if self.pic_order_cnt_type == 2 {
            return 0;
        }
        let frame_mbs = (self.pic_width_in_mbs * self.frame_height_in_mbs).max(1);
        (max_dpb_mbs(self.level_idc) / frame_mbs).clamp(1, 16) as usize
    }
}

/// §7.3.2.1.1.1 `scaling_list( )`: walk the `delta_scale` entries.
fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let mut last = 8i32;
    let mut next = 8i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            next = (last + delta + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Some(())
}

/// Annex E `vui_parameters( )` as far as `max_num_reorder_frames`.
fn parse_vui_reorder(r: &mut BitReader) -> Option<u32> {
    if r.flag()? {
        // aspect_ratio_info_present_flag
        if r.bits(8)? == 255 {
            r.skip(32)?; // sar_width + sar_height
        }
    }
    if r.flag()? {
        r.skip(1)?; // overscan_appropriate_flag
    }
    if r.flag()? {
        // video_signal_type_present_flag
        r.skip(4)?; // video_format + video_full_range_flag
        if r.flag()? {
            r.skip(24)?; // colour description
        }
    }
    if r.flag()? {
        // chroma_loc_info_present_flag
        r.ue()?;
        r.ue()?;
    }
    if r.flag()? {
        // timing_info_present_flag
        r.skip(65)?;
    }
    let nal_hrd = r.flag()?;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.flag()?;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.skip(1)?; // low_delay_hrd_flag
    }
    r.skip(1)?; // pic_struct_present_flag
    if !r.flag()? {
        // bitstream_restriction_flag
        return None;
    }
    r.skip(1)?; // motion_vectors_over_pic_boundaries_flag
    for _ in 0..4 {
        r.ue()?;
    }
    r.ue() // max_num_reorder_frames
}

/// Annex E.1.2 `hrd_parameters( )`.
fn skip_hrd(r: &mut BitReader) -> Option<()> {
    let cpb_cnt = r.ue()? + 1;
    if cpb_cnt > 32 {
        return None;
    }
    r.skip(8)?; // bit_rate_scale + cpb_size_scale
    for _ in 0..cpb_cnt {
        r.ue()?;
        r.ue()?;
        r.skip(1)?;
    }
    r.skip(20)
}

/// Table A-1 `MaxDpbMbs` by `level_idc` (level 1b shares `level_idc`
/// 11 with constraint_set3 or uses 9; both map to 396 here).
fn max_dpb_mbs(level_idc: u32) -> u32 {
    match level_idc {
        9 | 10 => 396,
        11 => 900,
        12 | 13 | 20 => 2376,
        21 => 4752,
        22 | 30 => 8100,
        31 => 18000,
        32 => 20480,
        40 | 41 => 32768,
        42 => 34816,
        50 => 110_400,
        51 | 52 => 184_320,
        _ => 696_320, // 6, 6.1, 6.2 and anything newer
    }
}

/// The H.264 picture parameter set fields this crate uses (§7.3.2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct H264Pps {
    pub id: u32,
    pub sps_id: u32,
    pub bottom_field_pic_order_in_frame_present: bool,
}

impl H264Pps {
    pub fn parse(nal: &[u8]) -> Option<Self> {
        let mut r = BitReader::new(nal.get(1..)?);
        let id = r.ue()?;
        let sps_id = r.ue()?;
        r.skip(1)?; // entropy_coding_mode_flag
        let bottom_field_pic_order_in_frame_present = r.flag()?;
        Some(H264Pps {
            id,
            sps_id,
            bottom_field_pic_order_in_frame_present,
        })
    }
}

/// Order-count-relevant fields of one H.264 coded picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct H264PictureInfo {
    pub idr: bool,
    pub reference: bool,
    pub frame_num: u32,
    pub field_pic: bool,
    pub bottom_field: bool,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub sps_id: u32,
}

/// Parse the slice header of `nal` (a VCL NAL unit, header byte included)
/// up to the order-count fields (§7.3.3).
pub fn h264_slice_info(
    nal: &[u8],
    sps_by_id: &HashMap<u32, H264Sps>,
    pps_by_id: &HashMap<u32, H264Pps>,
) -> Option<H264PictureInfo> {
    let nal_ref_idc = (nal.first()? >> 5) & 3;
    let nal_unit_type = nal[0] & 0x1F;
    let idr = nal_unit_type == 5;
    let mut r = BitReader::new(&nal[1..]);
    r.ue()?; // first_mb_in_slice
    r.ue()?; // slice_type
    let pps = pps_by_id.get(&r.ue()?)?;
    let sps = sps_by_id.get(&pps.sps_id)?;
    if sps.separate_colour_plane {
        r.skip(2)?; // colour_plane_id
    }
    let frame_num = r.bits(sps.log2_max_frame_num as usize)?;
    let (mut field_pic, mut bottom_field) = (false, false);
    if !sps.frame_mbs_only {
        field_pic = r.flag()?;
        if field_pic {
            bottom_field = r.flag()?;
        }
    }
    if idr {
        r.ue()?; // idr_pic_id
    }
    let mut pic_order_cnt_lsb = 0;
    let mut delta_pic_order_cnt_bottom = 0;
    let mut delta_pic_order_cnt = [0i32; 2];
    if sps.pic_order_cnt_type == 0 {
        pic_order_cnt_lsb = r.bits(sps.log2_max_pic_order_cnt_lsb as usize)?;
        if pps.bottom_field_pic_order_in_frame_present && !field_pic {
            delta_pic_order_cnt_bottom = r.se()?;
        }
    } else if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero {
        delta_pic_order_cnt[0] = r.se()?;
        if pps.bottom_field_pic_order_in_frame_present && !field_pic {
            delta_pic_order_cnt[1] = r.se()?;
        }
    }
    Some(H264PictureInfo {
        idr,
        reference: nal_ref_idc != 0,
        frame_num,
        field_pic,
        bottom_field,
        pic_order_cnt_lsb,
        delta_pic_order_cnt_bottom,
        delta_pic_order_cnt,
        sps_id: sps.id,
    })
}

/// §8.2.1 picture-order-count state across a coded video sequence.
#[derive(Clone, Debug, Default)]
pub struct H264PocState {
    prev_pic_order_cnt_msb: i64,
    prev_pic_order_cnt_lsb: i64,
    prev_frame_num_offset: i64,
    prev_frame_num: i64,
}

impl H264PocState {
    /// `PicOrderCnt( )` of the picture (`Min( TopFieldOrderCnt,
    /// BottomFieldOrderCnt )` for a frame, the field's own count for a
    /// field), updating the state for the next picture.
    pub fn next(&mut self, sps: &H264Sps, p: &H264PictureInfo) -> i64 {
        let (top, bottom) = match sps.pic_order_cnt_type {
            0 => {
                let max_lsb = 1i64 << sps.log2_max_pic_order_cnt_lsb;
                let (prev_msb, prev_lsb) = if p.idr {
                    (0, 0)
                } else {
                    (self.prev_pic_order_cnt_msb, self.prev_pic_order_cnt_lsb)
                };
                let lsb = p.pic_order_cnt_lsb as i64;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                let bottom = if !p.field_pic {
                    top + p.delta_pic_order_cnt_bottom as i64
                } else {
                    msb + lsb
                };
                if p.reference {
                    self.prev_pic_order_cnt_msb = msb;
                    self.prev_pic_order_cnt_lsb = lsb;
                }
                (top, bottom)
            }
            1 => {
                let frame_num_offset = self.frame_num_offset(sps, p);
                let cycle = sps.offset_for_ref_frame.len() as i64;
                let mut abs_frame_num = if cycle != 0 {
                    frame_num_offset + p.frame_num as i64
                } else {
                    0
                };
                if !p.reference && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let mut expected = 0i64;
                if abs_frame_num > 0 {
                    let cycle_cnt = (abs_frame_num - 1) / cycle;
                    let in_cycle = ((abs_frame_num - 1) % cycle) as usize;
                    let expected_delta: i64 =
                        sps.offset_for_ref_frame.iter().map(|&o| o as i64).sum();
                    expected = cycle_cnt * expected_delta;
                    for &o in &sps.offset_for_ref_frame[..=in_cycle] {
                        expected += o as i64;
                    }
                }
                if !p.reference {
                    expected += sps.offset_for_non_ref_pic as i64;
                }
                let d0 = p.delta_pic_order_cnt[0] as i64;
                let (top, bottom) = if !p.field_pic {
                    let top = expected + d0;
                    (
                        top,
                        top + sps.offset_for_top_to_bottom_field as i64
                            + p.delta_pic_order_cnt[1] as i64,
                    )
                } else if !p.bottom_field {
                    (expected + d0, expected + d0)
                } else {
                    let b = expected + sps.offset_for_top_to_bottom_field as i64 + d0;
                    (b, b)
                };
                self.prev_frame_num_offset = frame_num_offset;
                self.prev_frame_num = p.frame_num as i64;
                (top, bottom)
            }
            _ => {
                let frame_num_offset = self.frame_num_offset(sps, p);
                let temp = if p.idr {
                    0
                } else if !p.reference {
                    2 * (frame_num_offset + p.frame_num as i64) - 1
                } else {
                    2 * (frame_num_offset + p.frame_num as i64)
                };
                self.prev_frame_num_offset = frame_num_offset;
                self.prev_frame_num = p.frame_num as i64;
                (temp, temp)
            }
        };
        if !p.field_pic {
            top.min(bottom)
        } else if p.bottom_field {
            bottom
        } else {
            top
        }
    }

    /// §8.2.1.2 / §8.2.1.3 `FrameNumOffset`.
    fn frame_num_offset(&self, sps: &H264Sps, p: &H264PictureInfo) -> i64 {
        if p.idr {
            0
        } else if self.prev_frame_num > p.frame_num as i64 {
            self.prev_frame_num_offset + (1i64 << sps.log2_max_frame_num)
        } else {
            self.prev_frame_num_offset
        }
    }
}

// ─────────────────────────── H.265 ────────────────────────────────────────────

/// The H.265 sequence parameter set fields this crate uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HevcSps {
    pub id: u32,
    pub chroma_format_idc: u32,
    pub bit_depth: u32,
    pub separate_colour_plane: bool,
    pub log2_max_pic_order_cnt_lsb: u32,
    /// `sps_max_num_reorder_pics[ sps_max_sub_layers_minus1 ]`.
    pub max_num_reorder_pics: u32,
}

impl HevcSps {
    /// Parse an SPS NAL unit (two header bytes included), §7.3.2.2.1,
    /// walking the §7.3.3 `profile_tier_level( 1, sps_max_sub_layers_minus1 )`
    /// structure first (96 general bits, two flag bits per sub-layer
    /// padded to 16 bits, then 88 / 8 bits per sub-layer with a present
    /// profile / level).
    pub fn parse(nal: &[u8]) -> Option<Self> {
        let mut r = BitReader::new(nal.get(2..)?);
        r.skip(4)?; // sps_video_parameter_set_id
        let max_sub_layers_minus1 = r.bits(3)? as usize;
        r.skip(1)?; // sps_temporal_id_nesting_flag
        r.skip(96)?; // general profile / tier / level
        let mut profile_present = [false; 8];
        let mut level_present = [false; 8];
        for i in 0..max_sub_layers_minus1 {
            profile_present[i] = r.flag()?;
            level_present[i] = r.flag()?;
        }
        if max_sub_layers_minus1 > 0 {
            r.skip(2 * (8 - max_sub_layers_minus1))?;
        }
        for i in 0..max_sub_layers_minus1 {
            if profile_present[i] {
                r.skip(88)?;
            }
            if level_present[i] {
                r.skip(8)?;
            }
        }
        let id = r.ue()?;
        let chroma_format_idc = r.ue()?;
        let mut separate_colour_plane = false;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.flag()?;
        }
        r.ue()?; // pic_width_in_luma_samples
        r.ue()?; // pic_height_in_luma_samples
        if r.flag()? {
            for _ in 0..4 {
                r.ue()?; // conf_win_*_offset
            }
        }
        let bit_depth = r.ue()? + 8;
        r.ue()?; // bit_depth_chroma_minus8
        let log2_max_pic_order_cnt_lsb = r.ue()? + 4;
        let ordering_info_present = r.flag()?;
        let first = if ordering_info_present {
            0
        } else {
            max_sub_layers_minus1
        };
        let mut max_num_reorder_pics = 0;
        for _ in first..=max_sub_layers_minus1 {
            r.ue()?; // sps_max_dec_pic_buffering_minus1
            max_num_reorder_pics = r.ue()?;
            r.ue()?; // sps_max_latency_increase_plus1
        }
        Some(HevcSps {
            id,
            chroma_format_idc,
            bit_depth,
            separate_colour_plane,
            log2_max_pic_order_cnt_lsb,
            max_num_reorder_pics,
        })
    }

    pub fn pixel_format(&self) -> Option<PixelFormat> {
        layout_label(self.chroma_format_idc, self.bit_depth)
    }

    pub fn reorder_depth(&self) -> usize {
        self.max_num_reorder_pics.min(16) as usize
    }
}

/// The H.265 picture parameter set fields this crate uses (§7.3.2.3.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HevcPps {
    pub id: u32,
    pub sps_id: u32,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u32,
}

impl HevcPps {
    pub fn parse(nal: &[u8]) -> Option<Self> {
        let mut r = BitReader::new(nal.get(2..)?);
        Some(HevcPps {
            id: r.ue()?,
            sps_id: r.ue()?,
            dependent_slice_segments_enabled: r.flag()?,
            output_flag_present: r.flag()?,
            num_extra_slice_header_bits: r.bits(3)?,
        })
    }
}

/// Order-count-relevant fields of one H.265 coded picture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HevcPictureInfo {
    pub nal_unit_type: u8,
    pub temporal_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub sps_id: u32,
}

impl HevcPictureInfo {
    /// IRAP picture (§3: BLA / IDR / CRA / reserved 22–23).
    pub fn is_irap(&self) -> bool {
        (16..=23).contains(&self.nal_unit_type)
    }

    /// `true` for the sub-layer non-reference, RADL and RASL pictures
    /// that never become `prevTid0Pic` (§8.3.1).
    fn excluded_from_prev_tid0(&self) -> bool {
        let t = self.nal_unit_type;
        (t <= 14 && t.is_multiple_of(2)) || (6..=9).contains(&t)
    }
}

/// Parse the first slice segment header of `nal` (a VCL NAL unit, two
/// header bytes included) up to `slice_pic_order_cnt_lsb` (§7.3.6.1).
/// `None` for a dependent / non-first slice segment or on any parse
/// failure.
pub fn hevc_slice_info(
    nal: &[u8],
    sps_by_id: &HashMap<u32, HevcSps>,
    pps_by_id: &HashMap<u32, HevcPps>,
) -> Option<HevcPictureInfo> {
    let nal_unit_type = (nal.first()? >> 1) & 0x3F;
    let temporal_id = (nal.get(1)? & 0x07).checked_sub(1)? as u32;
    let mut r = BitReader::new(&nal[2..]);
    if !r.flag()? {
        return None; // not the first slice segment of the picture
    }
    if (16..=23).contains(&nal_unit_type) {
        r.skip(1)?; // no_output_of_prior_pics_flag
    }
    let pps = pps_by_id.get(&r.ue()?)?;
    let sps = sps_by_id.get(&pps.sps_id)?;
    r.skip(pps.num_extra_slice_header_bits as usize)?;
    r.ue()?; // slice_type
    if pps.output_flag_present {
        r.skip(1)?; // pic_output_flag
    }
    if sps.separate_colour_plane {
        r.skip(2)?; // colour_plane_id
    }
    let pic_order_cnt_lsb = if matches!(nal_unit_type, 19 | 20) {
        0
    } else {
        r.bits(sps.log2_max_pic_order_cnt_lsb as usize)?
    };
    Some(HevcPictureInfo {
        nal_unit_type,
        temporal_id,
        pic_order_cnt_lsb,
        sps_id: sps.id,
    })
}

/// §8.3.1 picture-order-count state across a coded video sequence.
#[derive(Clone, Debug, Default)]
pub struct HevcPocState {
    prev_tid0_lsb: i64,
    prev_tid0_msb: i64,
    /// The next IRAP picture starts a new sequence (`NoRaslOutputFlag`
    /// = 1): first picture, after an end-of-sequence NAL unit, or any
    /// IDR / BLA.
    pub sequence_start_pending: bool,
}

impl HevcPocState {
    pub fn new() -> Self {
        Self {
            sequence_start_pending: true,
            ..Default::default()
        }
    }

    /// `PicOrderCntVal` of the picture, updating the state. Returns the
    /// count and whether the picture started a new coded video sequence.
    pub fn next(&mut self, sps: &HevcSps, p: &HevcPictureInfo) -> (i64, bool) {
        let max_lsb = 1i64 << sps.log2_max_pic_order_cnt_lsb;
        let lsb = p.pic_order_cnt_lsb as i64;
        let starts_sequence =
            p.is_irap() && (self.sequence_start_pending || matches!(p.nal_unit_type, 16..=20));
        let msb = if starts_sequence {
            0
        } else {
            let (prev_lsb, prev_msb) = (self.prev_tid0_lsb, self.prev_tid0_msb);
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            }
        };
        if p.temporal_id == 0 && !p.excluded_from_prev_tid0() {
            self.prev_tid0_lsb = lsb;
            self.prev_tid0_msb = msb;
        }
        self.sequence_start_pending = false;
        (msb + lsb, starts_sequence)
    }
}

/// Planar label of a (chroma_format_idc, bit depth) pair.
pub fn layout_label(chroma_format_idc: u32, bit_depth: u32) -> Option<PixelFormat> {
    use PixelFormat as P;
    Some(match (chroma_format_idc, bit_depth) {
        (0, 8) => P::Gray8,
        (0, 10) => P::Gray10Le,
        (0, 12) => P::Gray12Le,
        (1, 8) => P::Yuv420P,
        (1, 10) => P::Yuv420P10Le,
        (1, 12) => P::Yuv420P12Le,
        (2, 8) => P::Yuv422P,
        (2, 10) => P::Yuv422P10Le,
        (2, 12) => P::Yuv422P12Le,
        (3, 8) => P::Yuv444P,
        (3, 10) => P::Yuv444P10Le,
        (3, 12) => P::Yuv444P12Le,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a NAL payload from a bit string ("0"/"1" characters).
    fn bits(s: &str) -> Vec<u8> {
        let mut s = s.replace(' ', "");
        while !s.len().is_multiple_of(8) {
            s.push('0');
        }
        s.as_bytes()
            .chunks(8)
            .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap())
            .collect()
    }

    fn ue(v: u32) -> String {
        let code = v + 1;
        let len = 32 - code.leading_zeros();
        format!(
            "{}{:0width$b}",
            "0".repeat(len as usize - 1),
            code,
            width = len as usize
        )
    }

    fn se(v: i32) -> String {
        ue(if v > 0 {
            2 * v as u32 - 1
        } else {
            (-2 * v) as u32
        })
    }

    #[test]
    fn exp_golomb() {
        let mut r = BitReader::new(&bits(&format!("{}{}{}{}", ue(0), ue(5), se(-3), se(4))));
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.ue(), Some(5));
        assert_eq!(r.se(), Some(-3));
        assert_eq!(r.se(), Some(4));
        // Emulation prevention bytes vanish.
        let r = BitReader::new(&[0, 0, 3, 1]);
        assert_eq!(r.data, vec![0, 0, 1]);
    }

    /// The High-profile SPS of the 100x60 reproduction fixture (black-box encoder).
    const X264_SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x0A, 0xAC, 0xD9, 0x47, 0x27, 0x9E, 0xF0, 0x11, 0x00, 0x00, 0x03, 0x00,
        0x01, 0x00, 0x00, 0x03, 0x00, 0x32, 0x0F, 0x12, 0x25, 0x96,
    ];
    const X264_PPS: &[u8] = &[0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];

    #[test]
    fn fixture_sps_pps_parse() {
        let sps = H264Sps::parse(X264_SPS).expect("sps");
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.level_idc, 10);
        assert_eq!(sps.pixel_format(), Some(PixelFormat::Yuv420P));
        assert_eq!(sps.pic_order_cnt_type, 0);
        assert_eq!((sps.pic_width_in_mbs, sps.frame_height_in_mbs), (7, 4));
        assert!(sps.frame_mbs_only);
        // The fixture's encoder writes bitstream_restriction with max_num_reorder_frames = 2
        // for its default B-frame pyramid.
        assert_eq!(sps.max_num_reorder_frames, Some(2));
        assert_eq!(sps.reorder_depth(), 2);
        let pps = H264Pps::parse(X264_PPS).expect("pps");
        assert_eq!((pps.id, pps.sps_id), (0, 0));
    }

    #[test]
    fn level_fallback_depth_and_poc_type_2() {
        let mut sps = H264Sps::parse(X264_SPS).unwrap();
        sps.max_num_reorder_frames = None;
        // Level 1.0: 396 MBs / 28 MBs per frame = 14.
        assert_eq!(sps.reorder_depth(), 14);
        sps.level_idc = 51;
        assert_eq!(sps.reorder_depth(), 16);
        sps.pic_order_cnt_type = 2;
        assert_eq!(sps.reorder_depth(), 0);
    }

    fn sps_map(sps: H264Sps) -> HashMap<u32, H264Sps> {
        HashMap::from([(sps.id, sps)])
    }

    #[test]
    fn h264_poc_type_0_orders_a_b_pyramid() {
        let sps = H264Sps::parse(X264_SPS).unwrap();
        let pps = H264Pps::parse(X264_PPS).unwrap();
        let (sm, pm) = (sps_map(sps.clone()), HashMap::from([(0, pps)]));
        // Slice header prefix: first_mb ue(0), slice_type ue, pps_id ue(0),
        // frame_num u(log2_max_frame_num), [idr_pic_id], poc_lsb.
        let slice = |nal_type: u8, ref_idc: u8, slice_type: u32, frame_num: u32, lsb: u32| {
            let mut s = format!("{}{}{}", ue(0), ue(slice_type), ue(0));
            s += &format!("{:0w$b}", frame_num, w = sps.log2_max_frame_num as usize);
            if nal_type == 5 {
                s += &ue(0);
            }
            s += &format!("{:0w$b}", lsb, w = sps.log2_max_pic_order_cnt_lsb as usize);
            let mut nal = vec![(ref_idc << 5) | nal_type];
            nal.extend(bits(&s));
            nal
        };
        let mut st = H264PocState::default();
        // Decode order I0 P4 B2 b1 b3 with lsb = 2 × display index.
        let seq = [
            (5u8, 3u8, 7u32, 0u32, 0u32),
            (1, 2, 5, 1, 8),
            (1, 2, 6, 2, 4),
            (1, 0, 6, 3, 2),
            (1, 0, 6, 3, 6),
        ];
        let pocs: Vec<i64> = seq
            .iter()
            .map(|&(t, r, s, f, l)| {
                let info = h264_slice_info(&slice(t, r, s, f, l), &sm, &pm).expect("slice");
                st.next(&sps, &info)
            })
            .collect();
        assert_eq!(pocs, vec![0, 8, 4, 2, 6]);
        // §8.2.1.1 wrap-around (MaxPicOrderCntLsb = 64 here). References
        // at lsb 20 / 40 / 60 stay within half the range of each other;
        // lsb 4 after 60 is a forward wrap (MSB + 64 → 68); lsb 62 after 4
        // is a jump of more than half the range, read as a step back
        // (MSB − 64 → 62).
        let max = 1u32 << sps.log2_max_pic_order_cnt_lsb;
        assert_eq!(max, 64);
        for (frame_num, lsb, want) in [
            (4, 20, 20),
            (5, 40, 40),
            (6, 60, 60),
            (7, 4, 68),
            (8, 62, 62),
        ] {
            let info = h264_slice_info(&slice(1, 2, 5, frame_num, lsb), &sm, &pm).unwrap();
            assert_eq!(st.next(&sps, &info), want, "lsb {lsb}");
        }
        // A new IDR resets the history.
        let info = h264_slice_info(&slice(5, 3, 7, 0, 0), &sm, &pm).unwrap();
        assert!(info.idr);
        assert_eq!(st.next(&sps, &info), 0);
    }

    #[test]
    fn h264_poc_type_2_is_decode_order() {
        let mut sps = H264Sps::parse(X264_SPS).unwrap();
        sps.pic_order_cnt_type = 2;
        let mut st = H264PocState::default();
        let pic = |idr: bool, reference: bool, frame_num: u32| H264PictureInfo {
            idr,
            reference,
            frame_num,
            field_pic: false,
            bottom_field: false,
            pic_order_cnt_lsb: 0,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0, 0],
            sps_id: 0,
        };
        assert_eq!(st.next(&sps, &pic(true, true, 0)), 0);
        assert_eq!(st.next(&sps, &pic(false, true, 1)), 2);
        assert_eq!(st.next(&sps, &pic(false, false, 2)), 3);
        assert_eq!(st.next(&sps, &pic(false, true, 2)), 4);
        // frame_num wrap adds MaxFrameNum to the offset.
        let max = 1u32 << sps.log2_max_frame_num;
        assert_eq!(
            st.next(&sps, &pic(false, true, max - 1)),
            2 * (max as i64 - 1)
        );
        assert_eq!(st.next(&sps, &pic(false, true, 0)), 2 * max as i64);
    }

    #[test]
    fn h264_poc_type_1_cycle() {
        let mut sps = H264Sps::parse(X264_SPS).unwrap();
        sps.pic_order_cnt_type = 1;
        sps.offset_for_ref_frame = vec![2];
        sps.offset_for_non_ref_pic = -1;
        let mut st = H264PocState::default();
        let pic = |idr: bool, reference: bool, frame_num: u32| H264PictureInfo {
            idr,
            reference,
            frame_num,
            field_pic: false,
            bottom_field: false,
            pic_order_cnt_lsb: 0,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0, 0],
            sps_id: 0,
        };
        assert_eq!(st.next(&sps, &pic(true, true, 0)), 0);
        assert_eq!(st.next(&sps, &pic(false, true, 1)), 2);
        assert_eq!(st.next(&sps, &pic(false, true, 2)), 4);
        // Non-reference picture between frames 2 and 3 (same frame_num 3): 6 - 1.
        assert_eq!(st.next(&sps, &pic(false, false, 3)), 3);
    }

    #[test]
    fn hevc_sps_pps_slice_and_poc() {
        // Synthetic Main-profile SPS (see the syntax walk in `HevcSps::parse`).
        let mut s = String::new();
        s += "0000"; // vps id
        s += "000"; // max_sub_layers_minus1
        s += "1"; // temporal id nesting
        s += &"0".repeat(96); // profile_tier_level(1, 0)
        s += &ue(0); // sps_id
        s += &ue(1); // chroma_format_idc
        s += &ue(64); // width
        s += &ue(64); // height
        s += "0"; // conformance_window_flag
        s += &ue(2); // bit_depth_luma_minus8
        s += &ue(2); // bit_depth_chroma_minus8
        s += &ue(4); // log2_max_pic_order_cnt_lsb_minus4 → 8 bits
        s += "1"; // sps_sub_layer_ordering_info_present_flag
        s += &ue(4); // max_dec_pic_buffering_minus1
        s += &ue(3); // max_num_reorder_pics
        s += &ue(0); // max_latency_increase_plus1
        let mut nal = vec![0x42, 0x01];
        nal.extend(bits(&s));
        let sps = HevcSps::parse(&nal).expect("sps");
        assert_eq!(sps.pixel_format(), Some(PixelFormat::Yuv420P10Le));
        assert_eq!(sps.log2_max_pic_order_cnt_lsb, 8);
        assert_eq!(sps.reorder_depth(), 3);

        let mut pps = vec![0x44, 0x01];
        pps.extend(bits(&format!("{}{}0 0 000", ue(0), ue(0))));
        let pps = HevcPps::parse(&pps).expect("pps");
        assert_eq!(pps.num_extra_slice_header_bits, 0);
        let (sm, pm) = (HashMap::from([(0, sps.clone())]), HashMap::from([(0, pps)]));

        let slice = |nal_type: u8, lsb: u32| {
            let mut s = String::from("1"); // first_slice_segment_in_pic_flag
            if (16..=23).contains(&nal_type) {
                s += "0"; // no_output_of_prior_pics_flag
            }
            s += &ue(0); // pps id
            s += &ue(1); // slice_type
            if !matches!(nal_type, 19 | 20) {
                s += &format!("{lsb:08b}");
            }
            let mut nal = vec![nal_type << 1, 1]; // layer 0, temporal id 0
            nal.extend(bits(&s));
            nal
        };
        let mut st = HevcPocState::new();
        let idr = hevc_slice_info(&slice(19, 0), &sm, &pm).unwrap();
        assert_eq!(st.next(&sps, &idr), (0, true));
        let trail = hevc_slice_info(&slice(1, 4), &sm, &pm).unwrap();
        assert_eq!(st.next(&sps, &trail), (4, false));
        let b = hevc_slice_info(&slice(0, 2), &sm, &pm).unwrap(); // TRAIL_N
        assert_eq!(st.next(&sps, &b), (2, false));
        // Wrap: lsb 250 after prev 4 → MSB − 256.
        let early = hevc_slice_info(&slice(1, 250), &sm, &pm).unwrap();
        assert_eq!(st.next(&sps, &early), (-6, false));
        // A CRA mid-stream keeps the history; one after a sequence start resets.
        let cra = hevc_slice_info(&slice(21, 8), &sm, &pm).unwrap();
        assert_eq!(st.next(&sps, &cra), (8, false));
        st.sequence_start_pending = true;
        assert_eq!(st.next(&sps, &cra), (8, true));
        // A non-first slice segment is skipped.
        let mut second = slice(1, 4);
        second[2] &= 0x7F;
        assert!(hevc_slice_info(&second, &sm, &pm).is_none());
    }
}
