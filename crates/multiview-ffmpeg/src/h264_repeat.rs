//! H.264 **repeat access unit** synthesis (output-routing §12.5 "Cadence drift").
//!
//! When a passthrough source runs slow, the passthrough clock emits, on the
//! tick before a strict IDR, one synthesised picture that repeats the previous
//! reference picture: a **non-reference P picture whose every macroblock is
//! `P_Skip`** with a zero motion vector. This module builds that access unit
//! from the stream's active SPS/PPS and the slice header of the previously
//! emitted access unit, so its `frame_num` / POC are valid where it is placed.
//!
//! Pure Rust, always compiled, no libav. Every function returns `None` on
//! anything unsupported or malformed (the caller then bridges instead); hostile
//! input never panics (no indexing, every read bounds-checked, every size
//! capped).
//!
//! ## What is supported (anything else returns `None`)
//!
//! * Profiles Baseline (66), Main (77), High (100), High 10 (110), High 4:2:2
//!   (122) and High 4:4:4 Predictive (244) without the intra-only constraint
//!   (`constraint_set3_flag` on 110/122/244). Extended, CAVLC 4:4:4 Intra and
//!   the SVC/MVC profiles are refused.
//! * One slice group; no `separate_colour_plane_flag`; no MBAFF. Field coding
//!   (PAFF) is accepted only when the previous picture is a frame.
//! * `pic_order_cnt_type` 0 and 2. Type 1 is refused (its expected-POC state
//!   is not reconstructible from one slice header). Type 2 also refuses a
//!   repeat after a non-reference picture (two consecutive non-reference
//!   pictures are not allowed with POC type 2, §8.2.1.3).
//! * The previous picture must be what the default `RefPicList0[0]` holds: a
//!   short-term reference marked by the sliding window. A non-reference
//!   previous picture, adaptive reference marking (any MMCO, including 3, 5
//!   and 6) or an IDR with `long_term_reference_flag` is refused.
//! * Bounded work: SPS / PPS over 4 KiB are refused before unescaping; only
//!   the first 4 KiB of the previous picture's first VCL NAL is unescaped
//!   and parsed; at most 1024 NALs are examined and a zero-length NAL in
//!   length-prefixed framing is malformed.
//! * Scaling matrices in the SPS are parsed (§7.3.2.1.1.1), never guessed.
//!
//! ## The repeat picture
//!
//! `AUD (primary_pic_type 1)` then one slice NAL (`nal_unit_type 1`,
//! `nal_ref_idc 0`): `first_mb_in_slice 0`, `slice_type 5` (P, all slices of
//! the picture), the PPS id, `frame_num = (prev_frame_num + (prev is reference
//! ? 1 : 0)) mod MaxFrameNum`, `field_pic_flag 0` when the SPS allows fields,
//! POC type 0 `pic_order_cnt_lsb = (prev_lsb + 2) mod MaxPicOrderCntLsb` (and
//! `delta_pic_order_cnt_bottom 0` where present), `redundant_pic_cnt 0` where
//! present, `num_ref_idx_active_override_flag 1` with one active reference
//! (so the reference list is exactly the most recent reference picture,
//! whatever the PPS default), no list modification, a neutral
//! `pred_weight_table` when `weighted_pred_flag` is set, `cabac_init_idc 0`,
//! `slice_qp_delta 0` and `disable_deblocking_filter_idc 1` when the PPS
//! carries deblocking control. Slice data: CAVLC `mb_skip_run =
//! PicSizeInMbs`; CABAC every macroblock `mb_skip_flag = 1` (ctxIdx 11, both
//! neighbours skipped or unavailable so ctxIdxInc 0) followed by
//! `end_of_slice_flag`, arithmetic-coded by the §9.3.4 encoder below.
//!
//! With every macroblock `P_Skip` and its neighbours unavailable or zero, each
//! predicted motion vector is zero (§8.4.1.1), so the picture decodes to an
//! exact copy of reference index 0, which is the most recent reference picture.

use crate::idr::NalFraming;

/// Largest picture accepted, in macroblocks (Level 6.2 `MaxFS`, Table A-1).
const MAX_PIC_SIZE_IN_MBS: u64 = 139_264;

/// Upper bound on MMCO / list-modification loop iterations while parsing the
/// previous slice header (a real header has at most a few dozen).
const MAX_SYNTAX_LOOP: usize = 128;

/// Upper bound on the NAL units scanned in one previous access unit
/// (zero-length NALs count).
const MAX_NALS_PER_AU: usize = 1024;

/// Largest SPS / PPS NAL accepted (bytes, before unescaping).
const MAX_PARAM_SET_BYTES: usize = 4096;

/// Bytes of the previous picture's first VCL NAL that are unescaped and
/// parsed: a slice header fits well inside (the largest legal header with
/// 32 list modifications, weights and 128 MMCOs is under 2 KiB).
const MAX_SLICE_HEADER_BYTES: usize = 4096;

// ---------------------------------------------------------------------------
// Bit reading (RBSP, emulation prevention removed).
// ---------------------------------------------------------------------------

/// Remove emulation-prevention bytes (`00 00 03` -> `00 00`) from a NAL payload.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0_u32;
    for &b in nal {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros = zeros.saturating_add(1);
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// MSB-first bit reader over RBSP bytes. Every read returns `None` past the end.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let shift = 7 - (self.pos % 8);
        self.pos += 1;
        Some(u32::from((byte >> shift) & 1))
    }

    fn flag(&mut self) -> Option<bool> {
        self.bit().map(|b| b == 1)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        if n > 32 {
            return None;
        }
        let mut v = 0_u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        u32::try_from(v).ok()
    }

    /// `ue(v)`, at most 32 leading zeros (values up to 2^32 - 2).
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0_u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = u64::from(self.bits(zeros)?);
        let v = (1_u64 << zeros) - 1 + rest;
        u32::try_from(v).ok()
    }

    /// `ue(v)` checked against an inclusive maximum.
    fn ue_max(&mut self, max: u32) -> Option<u32> {
        self.ue().filter(|&v| v <= max)
    }

    /// `se(v)`.
    fn se(&mut self) -> Option<i32> {
        let k = i64::from(self.ue()?);
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        i32::try_from(v).ok()
    }
}

// ---------------------------------------------------------------------------
// Bit writing.
// ---------------------------------------------------------------------------

/// MSB-first bit writer.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    /// Bits used in the last byte (0 = byte aligned).
    used: u32,
}

impl BitWriter {
    fn bit(&mut self, b: u32) {
        if self.used == 0 {
            self.out.push(0);
        }
        if b & 1 == 1 {
            if let Some(last) = self.out.last_mut() {
                *last |= 0x80 >> self.used;
            }
        }
        self.used = (self.used + 1) % 8;
    }

    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }

    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        for _ in 1..len {
            self.bit(0);
        }
        for i in (0..len).rev() {
            self.bit(u32::from((x >> i) & 1 == 1));
        }
    }

    fn se(&mut self, v: i32) {
        let k = if v > 0 {
            u32::try_from(2 * i64::from(v) - 1).unwrap_or(u32::MAX)
        } else {
            u32::try_from(-2 * i64::from(v)).unwrap_or(u32::MAX)
        };
        self.ue(k);
    }

    fn is_aligned(&self) -> bool {
        self.used == 0
    }

    /// `rbsp_trailing_bits()`: a stop bit then zero alignment.
    fn trailing_bits(&mut self) {
        self.bit(1);
        while !self.is_aligned() {
            self.bit(0);
        }
    }

    fn bytes(&mut self, data: &[u8]) {
        if self.is_aligned() {
            self.out.extend_from_slice(data);
        } else {
            for &b in data {
                self.bits(u32::from(b), 8);
            }
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

/// Insert emulation-prevention bytes into an RBSP (§7.4.1).
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 4);
    let mut zeros = 0_u32;
    for &b in rbsp {
        if zeros >= 2 && b <= 0x03 {
            out.push(0x03);
            zeros = 0;
        }
        out.push(b);
        if b == 0 {
            zeros = zeros.saturating_add(1);
        } else {
            zeros = 0;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Parameter sets.
// ---------------------------------------------------------------------------

/// The SPS fields the repeat picture needs (§7.3.2.1.1).
#[allow(clippy::struct_excessive_bools)] // mirrors the bitstream's flags
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264Sps {
    /// `profile_idc`.
    pub profile_idc: u8,
    /// `constraint_set0..5_flag` byte.
    pub constraint_flags: u8,
    /// `level_idc`.
    pub level_idc: u8,
    /// `seq_parameter_set_id`.
    pub sps_id: u32,
    /// `chroma_format_idc` (1 when absent).
    pub chroma_format_idc: u32,
    /// `separate_colour_plane_flag`.
    pub separate_colour_plane: bool,
    /// `bit_depth_luma_minus8 + 8`.
    pub bit_depth_luma: u32,
    /// `bit_depth_chroma_minus8 + 8`.
    pub bit_depth_chroma: u32,
    /// `seq_scaling_matrix_present_flag`.
    pub scaling_matrix_present: bool,
    /// `log2_max_frame_num_minus4 + 4`.
    pub log2_max_frame_num: u32,
    /// `pic_order_cnt_type`.
    pub pic_order_cnt_type: u32,
    /// `log2_max_pic_order_cnt_lsb_minus4 + 4` (POC type 0).
    pub log2_max_poc_lsb: u32,
    /// `delta_pic_order_always_zero_flag` (POC type 1).
    pub delta_pic_order_always_zero: bool,
    /// `max_num_ref_frames`.
    pub max_num_ref_frames: u32,
    /// `pic_width_in_mbs_minus1 + 1`.
    pub pic_width_in_mbs: u32,
    /// `pic_height_in_map_units_minus1 + 1`.
    pub pic_height_in_map_units: u32,
    /// `frame_mbs_only_flag`.
    pub frame_mbs_only: bool,
    /// `mb_adaptive_frame_field_flag`.
    pub mb_adaptive_frame_field: bool,
}

impl H264Sps {
    /// `ChromaArrayType` (§7.4.2.1.1).
    #[must_use]
    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane {
            0
        } else {
            self.chroma_format_idc
        }
    }

    /// `FrameHeightInMbs` (checked: `None` on overflow).
    #[must_use]
    pub fn frame_height_in_mbs(&self) -> Option<u64> {
        let factor = if self.frame_mbs_only { 1 } else { 2 };
        u64::from(self.pic_height_in_map_units).checked_mul(factor)
    }

    /// `PicSizeInMbs` of a frame (checked: `None` on overflow).
    #[must_use]
    pub fn frame_size_in_mbs(&self) -> Option<u64> {
        u64::from(self.pic_width_in_mbs).checked_mul(self.frame_height_in_mbs()?)
    }
}

/// The PPS fields the repeat picture needs (§7.3.2.2).
#[allow(clippy::struct_excessive_bools)] // mirrors the bitstream's flags
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264Pps {
    /// `pic_parameter_set_id`.
    pub pps_id: u32,
    /// `seq_parameter_set_id`.
    pub sps_id: u32,
    /// `entropy_coding_mode_flag` (CABAC).
    pub entropy_coding_mode: bool,
    /// `bottom_field_pic_order_in_frame_present_flag`.
    pub bottom_field_pic_order_in_frame_present: bool,
    /// `num_slice_groups_minus1 + 1`.
    pub num_slice_groups: u32,
    /// `num_ref_idx_l0_default_active_minus1 + 1`.
    pub num_ref_idx_l0_default_active: u32,
    /// `num_ref_idx_l1_default_active_minus1 + 1`.
    pub num_ref_idx_l1_default_active: u32,
    /// `weighted_pred_flag`.
    pub weighted_pred: bool,
    /// `weighted_bipred_idc`.
    pub weighted_bipred_idc: u32,
    /// `26 + pic_init_qp_minus26`.
    pub pic_init_qp: i32,
    /// `deblocking_filter_control_present_flag`.
    pub deblocking_filter_control_present: bool,
    /// `constrained_intra_pred_flag`.
    pub constrained_intra_pred: bool,
    /// `redundant_pic_cnt_present_flag`.
    pub redundant_pic_cnt_present: bool,
}

/// Strip a leading Annex-B start code, if any, from a bare NAL.
fn strip_start_code(nal: &[u8]) -> &[u8] {
    if let Some(rest) = nal.strip_prefix(&[0, 0, 0, 1]) {
        rest
    } else if let Some(rest) = nal.strip_prefix(&[0, 0, 1]) {
        rest
    } else {
        nal
    }
}

/// Skip one `scaling_list()` of `size` coefficients (§7.3.2.1.1.1).
fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last = 8_i32;
    let mut next = 8_i32;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            if !(-128..=127).contains(&delta) {
                return None;
            }
            next = (last + delta + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Parse an SPS NAL (header byte included; a leading start code is tolerated).
#[must_use]
pub fn parse_sps(nal: &[u8]) -> Option<H264Sps> {
    let nal = strip_start_code(nal);
    if nal.len() > MAX_PARAM_SET_BYTES {
        return None;
    }
    let (&header, payload) = nal.split_first()?;
    if header & 0x80 != 0 || header & 0x1F != 7 {
        return None;
    }
    let rbsp = unescape(payload);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = u8::try_from(r.bits(8)?).ok()?;
    let constraint_flags = u8::try_from(r.bits(8)?).ok()?;
    let level_idc = u8::try_from(r.bits(8)?).ok()?;
    let sps_id = r.ue_max(31)?;
    let mut chroma_format_idc = 1;
    let mut separate_colour_plane = false;
    let mut bit_depth_luma = 8;
    let mut bit_depth_chroma = 8;
    let mut scaling_matrix_present = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue_max(3)?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.flag()?;
        }
        bit_depth_luma = r.ue_max(6)? + 8;
        bit_depth_chroma = r.ue_max(6)? + 8;
        let _qpprime_y_zero_transform_bypass = r.flag()?;
        scaling_matrix_present = r.flag()?;
        if scaling_matrix_present {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.flag()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let log2_max_frame_num = r.ue_max(12)? + 4;
    let pic_order_cnt_type = r.ue_max(2)?;
    let mut log2_max_poc_lsb = 0;
    let mut delta_pic_order_always_zero = false;
    match pic_order_cnt_type {
        0 => log2_max_poc_lsb = r.ue_max(12)? + 4,
        1 => {
            delta_pic_order_always_zero = r.flag()?;
            let _offset_for_non_ref_pic = r.se()?;
            let _offset_for_top_to_bottom_field = r.se()?;
            let cycle = r.ue_max(255)?;
            for _ in 0..cycle {
                let _offset_for_ref_frame = r.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue_max(16)?;
    let _gaps_in_frame_num_value_allowed = r.flag()?;
    // Each dimension alone is refused above Level 6.2 MaxFS before any product.
    let max_dim = u32::try_from(MAX_PIC_SIZE_IN_MBS).ok()?;
    let pic_width_in_mbs = r.ue_max(max_dim - 1)?.checked_add(1)?;
    let pic_height_in_map_units = r.ue_max(max_dim - 1)?.checked_add(1)?;
    let frame_mbs_only = r.flag()?;
    let mb_adaptive_frame_field = if frame_mbs_only { false } else { r.flag()? };
    // direct_8x8_inference_flag must be readable: the fields above are only
    // trusted when the SPS is not truncated right after them.
    let _direct_8x8_inference = r.flag()?;
    let sps = H264Sps {
        profile_idc,
        constraint_flags,
        level_idc,
        sps_id,
        chroma_format_idc,
        separate_colour_plane,
        bit_depth_luma,
        bit_depth_chroma,
        scaling_matrix_present,
        log2_max_frame_num,
        pic_order_cnt_type,
        log2_max_poc_lsb,
        delta_pic_order_always_zero,
        max_num_ref_frames,
        pic_width_in_mbs,
        pic_height_in_map_units,
        frame_mbs_only,
        mb_adaptive_frame_field,
    };
    if sps.frame_size_in_mbs()? > MAX_PIC_SIZE_IN_MBS {
        return None;
    }
    Some(sps)
}

/// Parse a PPS NAL (header byte included; a leading start code is tolerated).
/// Fields after `redundant_pic_cnt_present_flag` (8x8 transform, PPS scaling
/// lists, second chroma QP offset) do not affect the repeat slice and are not
/// read.
#[must_use]
pub fn parse_pps(nal: &[u8]) -> Option<H264Pps> {
    let nal = strip_start_code(nal);
    if nal.len() > MAX_PARAM_SET_BYTES {
        return None;
    }
    let (&header, payload) = nal.split_first()?;
    if header & 0x80 != 0 || header & 0x1F != 8 {
        return None;
    }
    let rbsp = unescape(payload);
    let mut r = BitReader::new(&rbsp);
    let pps_id = r.ue_max(255)?;
    let sps_id = r.ue_max(31)?;
    let entropy_coding_mode = r.flag()?;
    let bottom_field_pic_order_in_frame_present = r.flag()?;
    let num_slice_groups = r.ue_max(7)? + 1;
    if num_slice_groups != 1 {
        // Slice-group maps are FMO (Baseline only); never synthesised.
        return None;
    }
    let num_ref_idx_l0_default_active = r.ue_max(31)? + 1;
    let num_ref_idx_l1_default_active = r.ue_max(31)? + 1;
    let weighted_pred = r.flag()?;
    let weighted_bipred_idc = r.bits(2)?;
    let pic_init_qp = 26_i32.checked_add(r.se()?)?;
    let _pic_init_qs = r.se()?;
    let _chroma_qp_index_offset = r.se()?;
    let deblocking_filter_control_present = r.flag()?;
    let constrained_intra_pred = r.flag()?;
    let redundant_pic_cnt_present = r.flag()?;
    if !(-26..=51).contains(&pic_init_qp) || weighted_bipred_idc > 2 {
        return None;
    }
    Some(H264Pps {
        pps_id,
        sps_id,
        entropy_coding_mode,
        bottom_field_pic_order_in_frame_present,
        num_slice_groups,
        num_ref_idx_l0_default_active,
        num_ref_idx_l1_default_active,
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp,
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
    })
}

// ---------------------------------------------------------------------------
// The previous picture's slice header.
// ---------------------------------------------------------------------------

/// What the repeat picture needs to know about the previous picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrevSlice {
    /// `nal_ref_idc` of the previous picture.
    pub nal_ref_idc: u8,
    /// Whether it was an IDR picture.
    pub idr: bool,
    /// `frame_num`.
    pub frame_num: u32,
    /// `pic_order_cnt_lsb` (POC type 0).
    pub poc_lsb: Option<u32>,
    /// `field_pic_flag`.
    pub field_pic: bool,
    /// `pic_parameter_set_id`.
    pub pps_id: u32,
    /// Whether a `memory_management_control_operation` 5 was signalled.
    pub mmco5: bool,
    /// Whether `adaptive_ref_pic_marking_mode_flag` was set (any MMCO).
    pub adaptive_marking: bool,
    /// An IDR's `long_term_reference_flag`.
    pub long_term: bool,
}

/// Skip a `pred_weight_table()` (§7.3.3.2).
fn skip_pred_weight_table(
    r: &mut BitReader<'_>,
    chroma_array_type: u32,
    l0: u32,
    l1: Option<u32>,
) -> Option<()> {
    let _luma_log2_weight_denom = r.ue_max(7)?;
    if chroma_array_type != 0 {
        let _chroma_log2_weight_denom = r.ue_max(7)?;
    }
    for count in std::iter::once(l0).chain(l1) {
        for _ in 0..count {
            if r.flag()? {
                r.se()?;
                r.se()?;
            }
            if chroma_array_type != 0 && r.flag()? {
                for _ in 0..4 {
                    r.se()?;
                }
            }
        }
    }
    Some(())
}

/// Skip a `ref_pic_list_modification()` list (§7.3.3.1).
fn skip_ref_pic_list_modification(r: &mut BitReader<'_>) -> Option<()> {
    if !r.flag()? {
        return Some(());
    }
    for _ in 0..MAX_SYNTAX_LOOP {
        match r.ue_max(5)? {
            0..=2 | 4 | 5 => {
                r.ue()?;
            }
            _ => return Some(()),
        }
    }
    None
}

/// Parse the leading part of a slice header (§7.3.3) up to and including
/// `dec_ref_pic_marking()`.
#[allow(clippy::too_many_lines)] // one linear walk of §7.3.3
fn parse_slice_header(nal: &[u8], sps: &H264Sps, pps: &H264Pps) -> Option<PrevSlice> {
    let (&header, payload) = nal.split_first()?;
    let nal_ref_idc = (header >> 5) & 3;
    let nal_type = header & 0x1F;
    let idr = nal_type == 5;
    if header & 0x80 != 0 || !(nal_type == 1 || nal_type == 5) {
        return None;
    }
    // Only a bounded prefix is unescaped (the header is at its start).
    let rbsp = unescape(payload.get(..payload.len().min(MAX_SLICE_HEADER_BYTES))?);
    let mut r = BitReader::new(&rbsp);
    let _first_mb = r.ue()?;
    let slice_type = r.ue_max(9)? % 5;
    let pps_id = r.ue_max(255)?;
    if pps_id != pps.pps_id {
        return None;
    }
    if sps.separate_colour_plane {
        return None;
    }
    let frame_num = r.bits(sps.log2_max_frame_num)?;
    let mut field_pic = false;
    if !sps.frame_mbs_only {
        field_pic = r.flag()?;
        if field_pic {
            let _bottom_field = r.flag()?;
        }
    }
    if idr {
        let _idr_pic_id = r.ue_max(65_535)?;
    }
    let mut poc_lsb = None;
    match sps.pic_order_cnt_type {
        0 => {
            poc_lsb = Some(r.bits(sps.log2_max_poc_lsb)?);
            if pps.bottom_field_pic_order_in_frame_present && !field_pic {
                r.se()?;
            }
        }
        1 if !sps.delta_pic_order_always_zero => {
            r.se()?;
            if pps.bottom_field_pic_order_in_frame_present && !field_pic {
                r.se()?;
            }
        }
        _ => {}
    }
    if pps.redundant_pic_cnt_present {
        r.ue_max(127)?;
    }
    // slice_type % 5: 0 P, 1 B, 2 I, 3 SP, 4 SI.
    let is_b = slice_type == 1;
    let inter = matches!(slice_type, 0 | 1 | 3);
    if is_b {
        let _direct_spatial_mv_pred = r.flag()?;
    }
    let mut l0 = pps.num_ref_idx_l0_default_active;
    let mut l1 = pps.num_ref_idx_l1_default_active;
    if inter && r.flag()? {
        l0 = r.ue_max(31)? + 1;
        if is_b {
            l1 = r.ue_max(31)? + 1;
        }
    }
    if slice_type != 2 && slice_type != 4 {
        skip_ref_pic_list_modification(&mut r)?;
        if is_b {
            skip_ref_pic_list_modification(&mut r)?;
        }
    }
    if (pps.weighted_pred && matches!(slice_type, 0 | 3)) || (pps.weighted_bipred_idc == 1 && is_b)
    {
        skip_pred_weight_table(&mut r, sps.chroma_array_type(), l0, is_b.then_some(l1))?;
    }
    let mut mmco5 = false;
    let mut adaptive_marking = false;
    let mut long_term = false;
    if nal_ref_idc != 0 {
        if idr {
            let _no_output_of_prior_pics = r.flag()?;
            long_term = r.flag()?;
        } else if r.flag()? {
            adaptive_marking = true;
            let mut done = false;
            for _ in 0..MAX_SYNTAX_LOOP {
                match r.ue_max(6)? {
                    0 => {
                        done = true;
                        break;
                    }
                    1 | 2 | 4 | 6 => {
                        r.ue()?;
                    }
                    3 => {
                        r.ue()?;
                        r.ue()?;
                    }
                    _ => mmco5 = true,
                }
            }
            if !done {
                return None;
            }
        }
    }
    Some(PrevSlice {
        nal_ref_idc,
        idr,
        frame_num,
        poc_lsb,
        field_pic,
        pps_id,
        mmco5,
        adaptive_marking,
        long_term,
    })
}

/// The first VCL NAL (types 1 / 5) of an access unit, cut to at most
/// [`MAX_SLICE_HEADER_BYTES`] + 1 bytes (header byte included). Bounded: at
/// most [`MAX_NALS_PER_AU`] NALs are examined (zero-length ones count, and a
/// zero-length NAL in length-prefixed framing is malformed), and an Annex-B
/// NAL is scanned for its end only within the prefix window.
fn first_vcl_prefix(au: &[u8], framing: NalFraming) -> Option<&[u8]> {
    let is_vcl = |b: &[u8]| b.first().is_some_and(|&h| matches!(h & 0x1F, 1 | 5));
    let window = MAX_SLICE_HEADER_BYTES + 1;
    match framing {
        NalFraming::AnnexB => {
            let mut i = 0_usize;
            let mut seen = 0_usize;
            loop {
                // Next start code at or after i.
                let s = au
                    .get(i..)?
                    .windows(3)
                    .position(|w| w == [0, 0, 1])
                    .and_then(|p| i.checked_add(p)?.checked_add(3))?;
                seen += 1;
                if seen > MAX_NALS_PER_AU {
                    return None;
                }
                let limit = au.len().min(s.saturating_add(window));
                let region = au.get(s..limit)?;
                let end = region
                    .windows(3)
                    .position(|w| w == [0, 0, 1])
                    .map_or(region.len(), |p| p);
                let mut body = region.get(..end)?;
                while let Some(rest) = body.strip_suffix(&[0]) {
                    body = rest;
                }
                if is_vcl(body) {
                    return Some(body);
                }
                i = s;
            }
        }
        NalFraming::LengthPrefixed { nal_length_size } => {
            let size = usize::from(nal_length_size);
            if !(1..=4).contains(&size) {
                return None;
            }
            let mut off = 0_usize;
            for _ in 0..MAX_NALS_PER_AU {
                if off >= au.len() {
                    return None;
                }
                let len = au
                    .get(off..off.checked_add(size)?)?
                    .iter()
                    .fold(0_usize, |a, &b| (a << 8) | usize::from(b));
                if len == 0 {
                    return None;
                }
                let start = off + size;
                let end = start.checked_add(len)?;
                let body = au.get(start..end)?;
                if is_vcl(body) {
                    return body.get(..body.len().min(window));
                }
                off = end;
            }
            None
        }
        NalFraming::Obu => None,
    }
}

/// Split an access unit into NAL bodies under `framing` (bounded).
fn split_nals(au: &[u8], framing: NalFraming) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    match framing {
        NalFraming::AnnexB => {
            let mut starts = Vec::new();
            let mut i = 0_usize;
            while let Some(w) = au.get(i..i + 3) {
                if w == [0, 0, 1] {
                    starts.push(i + 3);
                    i += 3;
                    if starts.len() > MAX_NALS_PER_AU {
                        return None;
                    }
                } else {
                    i += 1;
                }
            }
            for (n, &s) in starts.iter().enumerate() {
                let e = starts.get(n + 1).map_or(au.len(), |&next| next - 3);
                let mut body = au.get(s..e)?;
                // trailing_zero_8bits / the leading zero of a 4-byte start code.
                while let Some(rest) = body.strip_suffix(&[0]) {
                    body = rest;
                }
                if !body.is_empty() {
                    out.push(body);
                }
            }
        }
        NalFraming::LengthPrefixed { nal_length_size } => {
            let size = usize::from(nal_length_size);
            if !(1..=4).contains(&size) {
                return None;
            }
            let mut off = 0_usize;
            while off < au.len() {
                let len_bytes = au.get(off..off + size)?;
                let len = len_bytes
                    .iter()
                    .fold(0_usize, |a, &b| (a << 8) | usize::from(b));
                let start = off + size;
                let end = start.checked_add(len)?;
                let body = au.get(start..end)?;
                // A zero-length NAL is malformed framing.
                if body.is_empty() {
                    return None;
                }
                out.push(body);
                if out.len() > MAX_NALS_PER_AU {
                    return None;
                }
                off = end;
            }
        }
        NalFraming::Obu => return None,
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// CABAC encoder (§9.3.4), with the tables it needs.
// ---------------------------------------------------------------------------

/// `rangeTabLPS[pStateIdx][qCodIRangeIdx]`, ITU-T H.264 Table 9-44.
const RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [95, 116, 137, 158],
    [90, 110, 130, 150],
    [85, 104, 123, 142],
    [81, 99, 117, 135],
    [77, 94, 111, 128],
    [73, 89, 105, 122],
    [69, 85, 100, 116],
    [66, 80, 95, 110],
    [62, 76, 90, 104],
    [59, 72, 86, 99],
    [56, 69, 81, 94],
    [53, 65, 77, 89],
    [51, 62, 73, 85],
    [48, 59, 69, 80],
    [46, 56, 66, 76],
    [43, 53, 63, 72],
    [41, 50, 59, 69],
    [39, 48, 56, 65],
    [37, 45, 54, 62],
    [35, 43, 51, 59],
    [33, 41, 48, 56],
    [32, 39, 46, 53],
    [30, 37, 43, 50],
    [29, 35, 41, 48],
    [27, 33, 39, 45],
    [26, 31, 37, 43],
    [24, 30, 35, 41],
    [23, 28, 33, 39],
    [22, 27, 32, 37],
    [21, 26, 30, 35],
    [20, 24, 29, 33],
    [19, 23, 27, 31],
    [18, 22, 26, 30],
    [17, 21, 25, 28],
    [16, 20, 23, 27],
    [15, 19, 22, 25],
    [14, 18, 21, 24],
    [14, 17, 20, 23],
    [13, 16, 19, 22],
    [12, 15, 18, 21],
    [12, 14, 17, 20],
    [11, 14, 16, 19],
    [11, 13, 15, 18],
    [10, 12, 15, 17],
    [10, 12, 14, 16],
    [9, 11, 13, 15],
    [9, 11, 12, 14],
    [8, 10, 12, 14],
    [8, 9, 11, 13],
    [7, 9, 11, 12],
    [7, 9, 10, 12],
    [7, 8, 10, 11],
    [6, 8, 9, 11],
    [6, 7, 9, 10],
    [6, 7, 8, 9],
    [2, 2, 2, 2],
];

/// `transIdxLPS[pStateIdx]`, ITU-T H.264 Table 9-45. (`transIdxMPS` is
/// `min(pStateIdx + 1, 62)` for `pStateIdx < 63`, and 63 stays 63.)
const TRANS_IDX_LPS: [u8; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21,
    21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34,
    34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// `(m, n)` of `mb_skip_flag` ctxIdx 11 for `cabac_init_idc` 0 (P/SP
/// slices), ITU-T H.264 Table 9-13.
const MB_SKIP_CTX11_IDC0: (i32, i32) = (23, 33);

/// One context variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CabacCtx {
    p_state_idx: u8,
    val_mps: u8,
}

impl CabacCtx {
    /// §9.3.1.1 initialisation from `(m, n)` at `SliceQPY`.
    fn init((m, n): (i32, i32), slice_qp_y: i32) -> Self {
        let qp = slice_qp_y.clamp(0, 51);
        let pre = (((m * qp) >> 4) + n).clamp(1, 126);
        if pre <= 63 {
            Self {
                p_state_idx: u8::try_from(63 - pre).unwrap_or(0),
                val_mps: 0,
            }
        } else {
            Self {
                p_state_idx: u8::try_from(pre - 64).unwrap_or(0),
                val_mps: 1,
            }
        }
    }
}

/// The arithmetic encoder of §9.3.4.
struct CabacEncoder {
    w: BitWriter,
    low: u32,
    range: u32,
    first_bit: bool,
    outstanding: u64,
}

impl CabacEncoder {
    /// `InitEncoder` (§9.3.4.1), writing into an already-aligned writer.
    fn new(w: BitWriter) -> Self {
        Self {
            w,
            low: 0,
            range: 510,
            first_bit: true,
            outstanding: 0,
        }
    }

    /// `PutBit` (Figure 9-8).
    fn put_bit(&mut self, b: u32) {
        if self.first_bit {
            self.first_bit = false;
        } else {
            self.w.bit(b);
        }
        while self.outstanding > 0 {
            self.w.bit(1 - b);
            self.outstanding -= 1;
        }
    }

    /// `RenormE` (Figure 9-7).
    fn renorm(&mut self) {
        while self.range < 256 {
            if self.low < 256 {
                self.put_bit(0);
            } else if self.low >= 512 {
                self.low -= 512;
                self.put_bit(1);
            } else {
                self.low -= 256;
                self.outstanding += 1;
            }
            self.range <<= 1;
            self.low <<= 1;
        }
    }

    /// `EncodeDecision` (Figure 9-7).
    fn decision(&mut self, ctx: &mut CabacCtx, bin: u8) {
        let q = usize::try_from((self.range >> 6) & 3).unwrap_or(0);
        let p = usize::from(ctx.p_state_idx);
        let lps = RANGE_TAB_LPS
            .get(p)
            .and_then(|row| row.get(q))
            .copied()
            .map_or(2, u32::from);
        self.range -= lps;
        if bin == ctx.val_mps {
            ctx.p_state_idx = if p < 62 {
                ctx.p_state_idx + 1
            } else {
                ctx.p_state_idx
            };
        } else {
            self.low += self.range;
            self.range = lps;
            if ctx.p_state_idx == 0 {
                ctx.val_mps = 1 - ctx.val_mps;
            }
            ctx.p_state_idx = TRANS_IDX_LPS.get(p).copied().unwrap_or(0);
        }
        self.renorm();
    }

    /// `EncodeTerminate` (Figure 9-10).
    fn terminate(&mut self, bin: bool) {
        self.range -= 2;
        if bin {
            self.low += self.range;
            self.flush();
        } else {
            self.renorm();
        }
    }

    /// `EncodeFlush` (Figure 9-11). Its last written bit is the
    /// `rbsp_stop_one_bit`.
    fn flush(&mut self) {
        self.range = 2;
        self.renorm();
        self.put_bit((self.low >> 9) & 1);
        self.w.bits(((self.low >> 7) & 3) | 1, 2);
    }

    fn finish(self) -> BitWriter {
        self.w
    }
}

/// CABAC slice data for an all-`P_Skip` picture of `mbs` macroblocks, starting
/// byte aligned and ending with the stop bit then zero alignment.
fn cabac_skip_slice_data(mbs: u64, slice_qp_y: i32) -> Vec<u8> {
    let mut ctx = CabacCtx::init(MB_SKIP_CTX11_IDC0, slice_qp_y);
    let mut enc = CabacEncoder::new(BitWriter::default());
    for i in 0..mbs {
        enc.decision(&mut ctx, 1);
        enc.terminate(i + 1 == mbs);
    }
    let mut w = enc.finish();
    // rbsp_alignment_zero_bits (the stop bit was written by the flush).
    while !w.is_aligned() {
        w.bit(0);
    }
    w.into_bytes()
}

// ---------------------------------------------------------------------------
// The builder.
// ---------------------------------------------------------------------------

/// Builds repeat access units for one parameter-set generation.
#[derive(Debug, Clone)]
pub struct RepeatAuBuilder {
    sps: H264Sps,
    pps: H264Pps,
    /// Precomputed CABAC slice data (byte aligned), when the PPS is CABAC.
    cabac_data: Option<Vec<u8>>,
}

impl RepeatAuBuilder {
    /// Build from the active SPS and PPS NAL units (bare NALs, header byte
    /// included). `None` when unsupported (see the [module docs](self)).
    #[must_use]
    pub fn new(sps: &[u8], pps: &[u8]) -> Option<Self> {
        let sps = parse_sps(sps)?;
        let pps = parse_pps(pps)?;
        if pps.sps_id != sps.sps_id {
            return None;
        }
        let intra_only =
            sps.constraint_flags & 0x10 != 0 && matches!(sps.profile_idc, 110 | 122 | 244);
        if !matches!(sps.profile_idc, 66 | 77 | 100 | 110 | 122 | 244) || intra_only {
            return None;
        }
        if sps.separate_colour_plane
            || sps.mb_adaptive_frame_field
            || sps.pic_order_cnt_type == 1
            || sps.max_num_ref_frames == 0
        {
            return None;
        }
        let mbs = sps.frame_size_in_mbs()?;
        if mbs == 0 || mbs > MAX_PIC_SIZE_IN_MBS {
            return None;
        }
        let cabac_data = pps
            .entropy_coding_mode
            .then(|| cabac_skip_slice_data(mbs, pps.pic_init_qp));
        Some(Self {
            sps,
            pps,
            cabac_data,
        })
    }

    /// The parsed SPS.
    #[must_use]
    pub fn sps(&self) -> &H264Sps {
        &self.sps
    }

    /// The parsed PPS.
    #[must_use]
    pub fn pps(&self) -> &H264Pps {
        &self.pps
    }

    /// Parse the previous access unit's first slice header.
    #[must_use]
    pub fn prev_slice(&self, prev_au: &[u8], framing: NalFraming) -> Option<PrevSlice> {
        let vcl = first_vcl_prefix(prev_au, framing)?;
        parse_slice_header(vcl, &self.sps, &self.pps)
    }

    /// Whether the picture after `prev` may be a repeat of it: with a
    /// default reference list (one active entry, no modification) the repeat
    /// copies `RefPicList0[0]`, the short-term reference with the highest
    /// `PicNum`. That is `prev` only when `prev` is a short-term reference
    /// marked by the sliding window: refused after a non-reference picture,
    /// adaptive marking (any MMCO, including 3 and 6) or an IDR marked long
    /// term (R1-M3).
    fn repeat_allowed(prev: &PrevSlice) -> bool {
        prev.nal_ref_idc != 0 && !prev.adaptive_marking && !prev.mmco5 && !prev.long_term
    }

    /// The repeat AU to place right after `prev_au` (the previously emitted
    /// access unit), framed as `framing`. `None` when unsupported.
    #[must_use]
    pub fn repeat_after(&self, prev_au: &[u8], framing: NalFraming) -> Option<Vec<u8>> {
        let prev = self.prev_slice(prev_au, framing)?;
        if !Self::repeat_allowed(&prev) {
            return None;
        }
        let slice = self.slice_rbsp(&prev)?;
        // AUD: primary_pic_type 1 (I, P), then rbsp_trailing_bits.
        let aud: [u8; 2] = [0x09, 0x30];
        let mut slice_nal = Vec::with_capacity(slice.len() + 8);
        slice_nal.push(0x01); // forbidden 0, nal_ref_idc 0, type 1
        slice_nal.extend_from_slice(&escape(&slice));
        frame_nals(&[&aud, &slice_nal], framing)
    }

    /// `frame_num` and POC lsb of the repeat picture after `prev`.
    fn next_numbers(&self, prev: &PrevSlice) -> Option<(u32, Option<u32>)> {
        if prev.field_pic || prev.mmco5 {
            return None;
        }
        let reference = prev.nal_ref_idc != 0;
        if self.sps.pic_order_cnt_type == 2 && !reference {
            return None;
        }
        let max_frame_num = 1_u64 << self.sps.log2_max_frame_num;
        let frame_num = (u64::from(prev.frame_num) + u64::from(reference)) % max_frame_num;
        let lsb = match self.sps.pic_order_cnt_type {
            0 => {
                let max_lsb = 1_u64 << self.sps.log2_max_poc_lsb;
                Some(u32::try_from((u64::from(prev.poc_lsb?) + 2) % max_lsb).ok()?)
            }
            _ => None,
        };
        Some((u32::try_from(frame_num).ok()?, lsb))
    }

    /// The repeat slice's RBSP (header + slice data + trailing bits).
    fn slice_rbsp(&self, prev: &PrevSlice) -> Option<Vec<u8>> {
        let (frame_num, lsb) = self.next_numbers(prev)?;
        let sps = &self.sps;
        let pps = &self.pps;
        let mut w = BitWriter::default();
        w.ue(0); // first_mb_in_slice
        w.ue(5); // slice_type: P (all slices of the picture)
        w.ue(pps.pps_id);
        w.bits(frame_num, sps.log2_max_frame_num);
        if !sps.frame_mbs_only {
            w.bit(0); // field_pic_flag
        }
        if sps.pic_order_cnt_type == 0 {
            w.bits(lsb?, sps.log2_max_poc_lsb);
            if pps.bottom_field_pic_order_in_frame_present {
                w.se(0); // delta_pic_order_cnt_bottom
            }
        }
        if pps.redundant_pic_cnt_present {
            w.ue(0);
        }
        w.bit(1); // num_ref_idx_active_override_flag
        w.ue(0); // num_ref_idx_l0_active_minus1: one reference
        w.bit(0); // ref_pic_list_modification_flag_l0
        if pps.weighted_pred {
            w.ue(0); // luma_log2_weight_denom
            if sps.chroma_array_type() != 0 {
                w.ue(0); // chroma_log2_weight_denom
            }
            w.bit(0); // luma_weight_l0_flag[0]
            if sps.chroma_array_type() != 0 {
                w.bit(0); // chroma_weight_l0_flag[0]
            }
        }
        // nal_ref_idc == 0: no dec_ref_pic_marking().
        if pps.entropy_coding_mode {
            w.ue(0); // cabac_init_idc
        }
        w.se(0); // slice_qp_delta
        if pps.deblocking_filter_control_present {
            w.ue(1); // disable_deblocking_filter_idc
        }
        if let Some(data) = &self.cabac_data {
            while !w.is_aligned() {
                w.bit(1); // cabac_alignment_one_bit
            }
            w.bytes(data);
        } else {
            let mbs = u32::try_from(sps.frame_size_in_mbs()?).ok()?;
            w.ue(mbs); // mb_skip_run
            w.trailing_bits();
        }
        Some(w.into_bytes())
    }
}

/// Frame bare NALs as `framing`.
fn frame_nals(nals: &[&[u8]], framing: NalFraming) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for nal in nals {
        match framing {
            NalFraming::AnnexB => out.extend_from_slice(&[0, 0, 0, 1]),
            NalFraming::LengthPrefixed { nal_length_size } => {
                let size = u32::from(nal_length_size);
                if !(1..=4).contains(&size) {
                    return None;
                }
                let len = u64::try_from(nal.len()).ok()?;
                if len >= 1_u64 << (8 * size) {
                    return None;
                }
                for i in (0..size).rev() {
                    out.push(u8::try_from((len >> (8 * i)) & 0xFF).ok()?);
                }
            }
            NalFraming::Obu => return None,
        }
        out.extend_from_slice(nal);
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::expect_used)]
mod tests {
    use super::*;

    // Fixtures generated once with the ffmpeg CLI + libx264 (testsrc2, aud=1,
    // bframes=0 unless noted). x264 picks POC type 2 without B-frames and POC
    // type 0 with them.
    /// Baseline 320x240, CAVLC.
    const BASE_SPS: &[u8] = &[
        0x67, 0x42, 0xc0, 0x0d, 0xda, 0x05, 0x07, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40,
        0x00, 0x00, 0x0f, 0x03, 0xc5, 0x0a, 0xa8,
    ];
    const BASE_PPS: &[u8] = &[0x68, 0xce, 0x0f, 0xc8];
    /// First P slice of the baseline clip (header bytes).
    const BASE_P1: &[u8] = &[
        0x41, 0x9a, 0x21, 0x2f, 0x0f, 0x43, 0x87, 0xb4, 0xcd, 0x3f, 0xc5, 0x68,
    ];
    /// IDR slice of the baseline clip (header bytes).
    const BASE_IDR: &[u8] = &[
        0x65, 0x88, 0x84, 0x3a, 0x0c, 0x60, 0x01, 0x42, 0x03, 0x22, 0x10, 0x83,
    ];
    /// High 1280x720, CABAC, no B-frames.
    const HIGH_SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xb2, 0x00, 0xa0, 0x0b, 0x76, 0x02, 0x20, 0x00, 0x00, 0x03,
        0x00, 0x20, 0x00, 0x00, 0x07, 0x81, 0xe3, 0x06, 0x49,
    ];
    const HIGH_PPS: &[u8] = &[0x68, 0xeb, 0xc3, 0xcb, 0x22, 0xc0];
    const HIGH_P3: &[u8] = &[
        0x41, 0x9a, 0x72, 0x78, 0x43, 0xc9, 0x94, 0xc0, 0x85, 0xbf, 0x61, 0x16,
    ];
    /// High 1280x720, CABAC, one B-frame (POC type 0).
    const HB_SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xe4, 0x01, 0x40, 0x16, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03,
        0x00, 0x40, 0x00, 0x00, 0x0f, 0x03, 0xc6, 0x0c, 0x44, 0x80,
    ];
    const HB_PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];
    /// Last P (reference) slice of the B-frame clip's GOP, and a B (non-ref).
    const HB_P_LAST: &[u8] = &[
        0x41, 0x9a, 0x9d, 0x27, 0x84, 0x3c, 0x99, 0x4c, 0x08, 0x5b, 0xff, 0x4f,
    ];
    const HB_B: &[u8] = &[
        0x01, 0x9e, 0x45, 0xe4, 0x2d, 0xff, 0xf1, 0x09, 0x0e, 0x33, 0x9f, 0xf9,
    ];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        frame_nals(nals, NalFraming::AnnexB).unwrap()
    }

    #[test]
    fn parses_baseline_cavlc_sps_pps() {
        let sps = parse_sps(BASE_SPS).unwrap();
        assert_eq!(sps.profile_idc, 66);
        assert_eq!(sps.level_idc, 13);
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!(
            (sps.pic_width_in_mbs, sps.pic_height_in_map_units),
            (20, 15)
        );
        assert!(sps.frame_mbs_only);
        assert_eq!(sps.pic_order_cnt_type, 2);
        assert_eq!(sps.log2_max_frame_num, 4);
        let pps = parse_pps(BASE_PPS).unwrap();
        assert!(!pps.entropy_coding_mode);
        assert_eq!((pps.pps_id, pps.sps_id), (0, 0));
        assert_eq!(pps.num_slice_groups, 1);
        assert!(!pps.weighted_pred);
        assert!(pps.deblocking_filter_control_present);
    }

    #[test]
    fn parses_high_cabac_sps_pps() {
        let sps = parse_sps(HIGH_SPS).unwrap();
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.level_idc, 31);
        assert_eq!(
            (sps.pic_width_in_mbs, sps.pic_height_in_map_units),
            (80, 45)
        );
        assert_eq!(sps.bit_depth_luma, 8);
        assert!(!sps.scaling_matrix_present);
        assert_eq!(sps.pic_order_cnt_type, 2);
        let pps = parse_pps(HIGH_PPS).unwrap();
        assert!(pps.entropy_coding_mode);
        assert!(
            pps.weighted_pred,
            "x264 default weightp=2 sets weighted_pred_flag"
        );
        assert!(pps.deblocking_filter_control_present);
    }

    #[test]
    fn parses_poc_type_0_sps() {
        let sps = parse_sps(HB_SPS).unwrap();
        assert_eq!(sps.pic_order_cnt_type, 0);
        assert!(sps.log2_max_poc_lsb >= 4);
        assert!(parse_pps(HB_PPS).unwrap().entropy_coding_mode);
    }

    #[test]
    fn frame_num_rules() {
        let b = RepeatAuBuilder::new(BASE_SPS, BASE_PPS).unwrap();
        let idr = b
            .prev_slice(&annexb(&[BASE_IDR]), NalFraming::AnnexB)
            .unwrap();
        assert!(idr.idr);
        assert_eq!(idr.frame_num, 0);
        assert_eq!(
            b.next_numbers(&idr),
            Some((1, None)),
            "after a reference: +1"
        );
        let p1 = b
            .prev_slice(&annexb(&[BASE_P1]), NalFraming::AnnexB)
            .unwrap();
        assert_eq!(p1.frame_num, 1);
        assert_eq!(p1.nal_ref_idc, 2);
        assert_eq!(b.next_numbers(&p1), Some((2, None)));
        // frame_num wraps at MaxFrameNum (2^4 here).
        let wrap = PrevSlice {
            frame_num: 15,
            ..p1
        };
        assert_eq!(b.next_numbers(&wrap), Some((0, None)));
        // POC type 2: refused after a non-reference picture.
        let nonref = PrevSlice {
            nal_ref_idc: 0,
            ..p1
        };
        assert_eq!(b.next_numbers(&nonref), None);
        // MMCO 5 or a field picture before: refused.
        assert_eq!(b.next_numbers(&PrevSlice { mmco5: true, ..p1 }), None);
        assert_eq!(
            b.next_numbers(&PrevSlice {
                field_pic: true,
                ..p1
            }),
            None
        );
    }

    #[test]
    fn poc_type_0_rules() {
        let b = RepeatAuBuilder::new(HB_SPS, HB_PPS).unwrap();
        let p = b
            .prev_slice(&annexb(&[HB_P_LAST]), NalFraming::AnnexB)
            .unwrap();
        let lsb = p.poc_lsb.unwrap();
        let max = 1_u32 << b.sps().log2_max_poc_lsb;
        let (fnum, next_lsb) = b.next_numbers(&p).unwrap();
        assert_eq!(fnum, (p.frame_num + 1) % (1 << b.sps().log2_max_frame_num));
        assert_eq!(next_lsb, Some((lsb + 2) % max));
        // After a non-reference B: frame_num unchanged, lsb + 2.
        let bslice = b.prev_slice(&annexb(&[HB_B]), NalFraming::AnnexB).unwrap();
        assert_eq!(bslice.nal_ref_idc, 0);
        let (fnum_b, lsb_b) = b.next_numbers(&bslice).unwrap();
        assert_eq!(fnum_b, bslice.frame_num);
        assert_eq!(lsb_b, Some((bslice.poc_lsb.unwrap() + 2) % max));
        // The lsb wraps at MaxPicOrderCntLsb.
        let w = PrevSlice {
            poc_lsb: Some(max - 1),
            ..p
        };
        assert_eq!(b.next_numbers(&w).unwrap().1, Some(1));
    }

    /// A tiny spec decoder (§9.3.3.2) for the CABAC round trip.
    struct CabacDecoder<'a> {
        r: BitReader<'a>,
        range: u32,
        offset: u32,
    }

    impl<'a> CabacDecoder<'a> {
        fn new(data: &'a [u8]) -> Self {
            let mut r = BitReader::new(data);
            let offset = r.bits(9).unwrap();
            Self {
                r,
                range: 510,
                offset,
            }
        }
        fn renorm(&mut self) {
            while self.range < 256 {
                self.range <<= 1;
                self.offset = (self.offset << 1) | self.r.bit().unwrap_or(0);
            }
        }
        fn decision(&mut self, ctx: &mut CabacCtx) -> u8 {
            let q = usize::try_from((self.range >> 6) & 3).unwrap();
            let p = usize::from(ctx.p_state_idx);
            let lps = u32::from(RANGE_TAB_LPS[p][q]);
            self.range -= lps;
            let bin;
            if self.offset >= self.range {
                bin = 1 - ctx.val_mps;
                self.offset -= self.range;
                self.range = lps;
                if ctx.p_state_idx == 0 {
                    ctx.val_mps = 1 - ctx.val_mps;
                }
                ctx.p_state_idx = TRANS_IDX_LPS[p];
            } else {
                bin = ctx.val_mps;
                if p < 62 {
                    ctx.p_state_idx += 1;
                }
            }
            self.renorm();
            bin
        }
        fn terminate(&mut self) -> bool {
            self.range -= 2;
            if self.offset >= self.range {
                true
            } else {
                self.renorm();
                false
            }
        }
    }

    #[test]
    fn cabac_slice_data_round_trips_through_a_spec_decoder() {
        for (mbs, qp) in [
            (1_u64, 26),
            (2, 26),
            (300, 22),
            (3600, 51),
            (8160, 0),
            (99, -12),
        ] {
            let data = cabac_skip_slice_data(mbs, qp);
            let mut dec = CabacDecoder::new(&data);
            let mut ctx = CabacCtx::init(MB_SKIP_CTX11_IDC0, qp);
            for i in 0..mbs {
                assert_eq!(dec.decision(&mut ctx), 1, "mb {i}: mb_skip_flag");
                assert_eq!(dec.terminate(), i + 1 == mbs, "mb {i}: end_of_slice_flag");
            }
            // §9.3.4.5: after the terminate bin the decoder has read up to
            // and including the stop bit; the rest of the data is zero
            // alignment within the last byte.
            let consumed = dec.r.pos;
            let total = data.len() * 8;
            assert!(
                consumed <= total + 7,
                "mbs {mbs}: bits consumed {consumed} of {total}"
            );
            assert_ne!(*data.last().unwrap(), 0, "the stop bit is in the last byte");
        }
    }

    #[test]
    fn context_init_matches_9_3_1_1() {
        // SliceQPY 26: pre = ((23*26)>>4)+33 = 37+33 = 70 -> state 6, MPS 1.
        assert_eq!(
            CabacCtx::init(MB_SKIP_CTX11_IDC0, 26),
            CabacCtx {
                p_state_idx: 6,
                val_mps: 1
            }
        );
        // SliceQPY 0: pre = 33 -> state 30, MPS 0.
        assert_eq!(
            CabacCtx::init(MB_SKIP_CTX11_IDC0, 0),
            CabacCtx {
                p_state_idx: 30,
                val_mps: 0
            }
        );
        // Negative QP (high bit depth) clips to 0.
        assert_eq!(
            CabacCtx::init(MB_SKIP_CTX11_IDC0, -12),
            CabacCtx::init(MB_SKIP_CTX11_IDC0, 0)
        );
    }

    #[test]
    fn repeat_au_is_well_formed_cavlc() {
        let b = RepeatAuBuilder::new(BASE_SPS, BASE_PPS).unwrap();
        let au = b
            .repeat_after(&annexb(&[BASE_P1]), NalFraming::AnnexB)
            .unwrap();
        let nals = split_nals(&au, NalFraming::AnnexB).unwrap();
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x09, 0x30]);
        assert_eq!(nals[1][0], 0x01, "nal_ref_idc 0, type 1");
        // Read the slice header back.
        let rbsp = unescape(&nals[1][1..]);
        let mut r = BitReader::new(&rbsp);
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.ue(), Some(5));
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.bits(4), Some(2), "frame_num after P(1)");
        assert_eq!(r.flag(), Some(true), "override");
        assert_eq!(r.ue(), Some(0));
        assert_eq!(r.flag(), Some(false), "no list modification");
        assert_eq!(r.se(), Some(0), "slice_qp_delta");
        assert_eq!(r.ue(), Some(1), "deblocking disabled");
        assert_eq!(r.ue(), Some(300), "mb_skip_run = PicSizeInMbs");
        assert_eq!(r.bit(), Some(1), "stop bit");
        while let Some(bit) = r.bit() {
            assert_eq!(bit, 0);
        }
        // The prev slice of the repeat itself parses (it can follow itself:
        // a non-ref after a non-ref is refused for POC type 2).
        let me = b.prev_slice(&au, NalFraming::AnnexB).unwrap();
        assert_eq!((me.nal_ref_idc, me.frame_num), (0, 2));
        assert!(b.repeat_after(&au, NalFraming::AnnexB).is_none());
    }

    #[test]
    fn length_prefixed_framing_round_trips() {
        let b = RepeatAuBuilder::new(HIGH_SPS, HIGH_PPS).unwrap();
        for size in [1_u8, 2, 4] {
            let f = NalFraming::LengthPrefixed {
                nal_length_size: size,
            };
            let prev = frame_nals(&[HIGH_P3], f);
            let Some(prev) = prev else {
                continue;
            };
            let au = b.repeat_after(&prev, f).unwrap();
            let annex = b
                .repeat_after(&annexb(&[HIGH_P3]), NalFraming::AnnexB)
                .unwrap();
            let a: Vec<&[u8]> = split_nals(&au, f).unwrap();
            let c: Vec<&[u8]> = split_nals(&annex, NalFraming::AnnexB).unwrap();
            assert_eq!(a, c, "same NALs whatever the framing");
        }
        assert!(b
            .repeat_after(&annexb(&[HIGH_P3]), NalFraming::Obu)
            .is_none());
    }

    #[test]
    fn oversized_nal_does_not_fit_a_short_length_prefix() {
        let big = vec![0x41_u8; 300];
        assert!(frame_nals(&[&big], NalFraming::LengthPrefixed { nal_length_size: 1 }).is_none());
        assert!(frame_nals(&[&big], NalFraming::LengthPrefixed { nal_length_size: 2 }).is_some());
    }

    #[test]
    fn escape_inserts_emulation_prevention() {
        assert_eq!(
            escape(&[0, 0, 0, 0, 1, 0, 0, 3, 0, 0, 4]),
            vec![0, 0, 3, 0, 0, 3, 1, 0, 0, 3, 3, 0, 0, 4]
        );
        assert_eq!(
            unescape(&escape(&[0, 0, 1, 0, 0, 2, 0, 0])),
            vec![0, 0, 1, 0, 0, 2, 0, 0]
        );
    }

    #[test]
    fn hostile_and_truncated_inputs_return_none() {
        // Truncated SPS / PPS at every length.
        for n in 0..BASE_SPS.len() - 1 {
            let _ = parse_sps(&BASE_SPS[..n]);
        }
        assert!(parse_sps(&BASE_SPS[..5]).is_none());
        assert!(parse_pps(&HIGH_PPS[..2]).is_none());
        assert!(parse_sps(&[]).is_none());
        assert!(parse_pps(&[]).is_none());
        // Wrong NAL type / forbidden bit.
        assert!(parse_sps(BASE_PPS).is_none());
        assert!(parse_pps(BASE_SPS).is_none());
        let mut forbidden = BASE_SPS.to_vec();
        forbidden[0] |= 0x80;
        assert!(parse_sps(&forbidden).is_none());
        // Absurd picture size.
        let mut huge = vec![0x67, 0x42, 0xc0, 0x0d];
        // sps_id 0, log2_max_frame_num 0, poc type 2, refs 1, gaps 0,
        // width_minus1 = 2^20 - 1 (ue), height_minus1 0, frame_mbs_only 1.
        let mut w = BitWriter::default();
        w.ue(0);
        w.ue(0);
        w.ue(2);
        w.ue(1);
        w.bit(0);
        w.ue((1 << 20) - 1);
        w.ue(0);
        w.bit(1);
        w.bit(1);
        w.trailing_bits();
        huge.extend(w.into_bytes());
        assert!(
            parse_sps(&huge).is_none(),
            "pictures above Level 6.2 MaxFS are refused"
        );
        // All-zero and all-0xFF garbage.
        assert!(parse_sps(&[0x67, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
        let _ = parse_sps(&[0x67; 64]);
        let _ = parse_pps(&[0x68; 64]);
        // Mismatched ids.
        let mut pps1 = BitWriter::default();
        pps1.ue(0);
        pps1.ue(1); // sps_id 1
        pps1.bits(0, 2);
        pps1.ue(0);
        pps1.ue(0);
        pps1.ue(0);
        pps1.bits(0, 3);
        pps1.se(0);
        pps1.se(0);
        pps1.se(0);
        pps1.bits(0b100, 3);
        pps1.trailing_bits();
        let mut pps1_nal = vec![0x68];
        pps1_nal.extend(pps1.into_bytes());
        assert_eq!(parse_pps(&pps1_nal).unwrap().sps_id, 1);
        assert!(RepeatAuBuilder::new(BASE_SPS, &pps1_nal).is_none());
        // Previous AU: garbage, truncated, no VCL, other PPS id.
        let b = RepeatAuBuilder::new(HIGH_SPS, HIGH_PPS).unwrap();
        assert!(b.repeat_after(&[], NalFraming::AnnexB).is_none());
        assert!(b
            .repeat_after(&[0, 0, 1, 0x09, 0x30], NalFraming::AnnexB)
            .is_none());
        assert!(b
            .repeat_after(&[0, 0, 1, 0x41], NalFraming::AnnexB)
            .is_none());
        assert!(b
            .repeat_after(
                &[0xFF; 300],
                NalFraming::LengthPrefixed { nal_length_size: 4 }
            )
            .is_none());
        assert!(b
            .repeat_after(
                &[0, 0, 0, 9, 1],
                NalFraming::LengthPrefixed { nal_length_size: 4 }
            )
            .is_none());
        assert!(b
            .repeat_after(&[0; 64], NalFraming::LengthPrefixed { nal_length_size: 4 })
            .is_none());
        assert!(b
            .repeat_after(&[1], NalFraming::LengthPrefixed { nal_length_size: 9 })
            .is_none());
        for n in 1..HIGH_P3.len() {
            let _ = b.repeat_after(&annexb(&[&HIGH_P3[..n]]), NalFraming::AnnexB);
        }
        // Unsupported configurations.
        let mut intra = HIGH_SPS.to_vec();
        intra[1] = 110;
        intra[2] = 0x10; // constraint_set3: High 10 Intra
        assert!(RepeatAuBuilder::new(&intra, HIGH_PPS).is_none());
        let mut ext = BASE_SPS.to_vec();
        ext[1] = 88;
        assert!(RepeatAuBuilder::new(&ext, BASE_PPS).is_none());
    }

    #[test]
    fn mmco5_in_previous_picture_is_detected() {
        let b = RepeatAuBuilder::new(BASE_SPS, BASE_PPS).unwrap();
        // A P slice header: first_mb 0, type 5, pps 0, frame_num 3, override 0,
        // no modification, (no weights), adaptive marking: mmco 5, then 0.
        let mut w = BitWriter::default();
        w.ue(0);
        w.ue(5);
        w.ue(0);
        w.bits(3, 4);
        w.bit(0);
        w.bit(0);
        w.bit(1);
        w.ue(5);
        w.ue(0);
        w.se(0);
        w.ue(1);
        w.ue(300);
        w.trailing_bits();
        let mut nal = vec![0x41];
        nal.extend(escape(&w.into_bytes()));
        let p = b.prev_slice(&annexb(&[&nal]), NalFraming::AnnexB).unwrap();
        assert!(p.mmco5);
        assert!(b
            .repeat_after(&annexb(&[&nal]), NalFraming::AnnexB)
            .is_none());
    }

    /// A CAVLC baseline P slice header (frame_num 3) with the given
    /// `nal_ref_idc` and marking: `None` = sliding window, `Some(ops)` =
    /// adaptive marking with `(mmco, args)` then 0.
    fn p_slice(nal_ref_idc: u8, marking: Option<&[(u32, &[u32])]>) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(0);
        w.ue(5);
        w.ue(0);
        w.bits(3, 4);
        w.bit(0); // num_ref_idx_active_override_flag
        w.bit(0); // ref_pic_list_modification_flag_l0
        if nal_ref_idc != 0 {
            match marking {
                None => w.bit(0),
                Some(ops) => {
                    w.bit(1);
                    for (op, args) in ops {
                        w.ue(*op);
                        for a in *args {
                            w.ue(*a);
                        }
                    }
                    w.ue(0);
                }
            }
        }
        w.se(0);
        w.ue(1);
        w.ue(300);
        w.trailing_bits();
        let mut nal = vec![(nal_ref_idc << 5) | 1];
        nal.extend(escape(&w.into_bytes()));
        nal
    }

    #[test]
    fn repeat_refused_unless_prev_is_the_sliding_window_short_term_ref() {
        // R1-M3: RefPicList0[0] is the previous picture only for a short-term
        // reference marked by the sliding window.
        let b = RepeatAuBuilder::new(BASE_SPS, BASE_PPS).unwrap();
        let ok = p_slice(2, None);
        assert!(b
            .repeat_after(&annexb(&[&ok]), NalFraming::AnnexB)
            .is_some());
        let nonref = p_slice(0, None);
        assert!(b
            .repeat_after(&annexb(&[&nonref]), NalFraming::AnnexB)
            .is_none());
        for ops in [
            &[(1_u32, &[0_u32][..])][..],
            &[(3, &[0, 0][..])][..],
            &[(6, &[0][..])][..],
            &[(2, &[0][..]), (4, &[1][..])][..],
        ] {
            let nal = p_slice(2, Some(ops));
            let prev = b.prev_slice(&annexb(&[&nal]), NalFraming::AnnexB).unwrap();
            assert!(prev.adaptive_marking);
            assert!(
                b.repeat_after(&annexb(&[&nal]), NalFraming::AnnexB)
                    .is_none(),
                "adaptive marking {ops:?}"
            );
        }
        // An IDR marked long-term.
        let mut w = BitWriter::default();
        w.ue(0);
        w.ue(7);
        w.ue(0);
        w.bits(0, 4);
        w.ue(0); // idr_pic_id
        w.bit(0); // no_output_of_prior_pics
        w.bit(1); // long_term_reference_flag
        w.se(0);
        w.ue(1);
        w.trailing_bits();
        let mut idr = vec![0x65];
        idr.extend(escape(&w.into_bytes()));
        let prev = b.prev_slice(&annexb(&[&idr]), NalFraming::AnnexB).unwrap();
        assert!(prev.idr && prev.long_term);
        assert!(b
            .repeat_after(&annexb(&[&idr]), NalFraming::AnnexB)
            .is_none());
        // The real x264 IDR (short-term) still repeats.
        assert!(b
            .repeat_after(&annexb(&[BASE_IDR]), NalFraming::AnnexB)
            .is_some());
    }

    #[test]
    fn pic_init_qp_overflow_is_refused() {
        // R1-A1: se() of ue 4294967293 is +2147483647; 26 + that overflowed.
        let mut w = BitWriter::default();
        w.ue(0);
        w.ue(0);
        w.bits(0, 2);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.bits(0, 3);
        w.ue(4_294_967_293); // pic_init_qp_minus26 = +2^31 - 1
        w.se(0);
        w.se(0);
        w.bits(0b100, 3);
        w.trailing_bits();
        let mut nal = vec![0x68];
        nal.extend(w.into_bytes());
        assert!(parse_pps(&nal).is_none());
    }

    #[test]
    fn huge_dimensions_are_refused_without_overflow() {
        // R1-A2: width/height near u32::MAX must never multiply unchecked.
        for (wm1, hm1, fmo) in [
            (u32::MAX - 2, u32::MAX - 2, false),
            (u32::MAX - 2, 0, true),
            (0, u32::MAX - 2, false),
            (70_000, 70_000, true),
        ] {
            let mut w = BitWriter::default();
            w.ue(0);
            w.ue(0);
            w.ue(2);
            w.ue(1);
            w.bit(0);
            w.ue(wm1);
            w.ue(hm1);
            w.bit(u32::from(fmo));
            if !fmo {
                w.bit(0);
            }
            w.bit(1);
            w.trailing_bits();
            let mut nal = vec![0x67, 0x42, 0xc0, 0x0d];
            nal.extend(w.into_bytes());
            assert!(parse_sps(&nal).is_none(), "{wm1}x{hm1}");
        }
        let sps = H264Sps {
            pic_width_in_mbs: u32::MAX,
            pic_height_in_map_units: u32::MAX,
            frame_mbs_only: false,
            ..parse_sps(BASE_SPS).unwrap()
        };
        assert_eq!(
            sps.frame_size_in_mbs(),
            u64::from(u32::MAX).checked_mul(2 * u64::from(u32::MAX))
        );
        let sps = H264Sps {
            pic_width_in_mbs: u32::MAX,
            ..sps
        };
        assert!(sps.frame_height_in_mbs().is_some());
    }

    #[test]
    fn work_is_bounded() {
        // R1-A6: oversize parameter sets are refused before unescaping.
        let mut big_sps = BASE_SPS.to_vec();
        big_sps.resize(MAX_PARAM_SET_BYTES + 1, 0);
        assert!(parse_sps(&big_sps).is_none());
        let mut big_pps = BASE_PPS.to_vec();
        big_pps.resize(MAX_PARAM_SET_BYTES + 1, 0);
        assert!(parse_pps(&big_pps).is_none());
        // A huge previous slice: only the prefix is parsed; still correct.
        let b = RepeatAuBuilder::new(BASE_SPS, BASE_PPS).unwrap();
        let mut big = BASE_P1.to_vec();
        big.resize(4 * 1024 * 1024, 0x55);
        let lp = NalFraming::LengthPrefixed { nal_length_size: 4 };
        let prev = frame_nals(&[&big], lp).unwrap();
        assert!(b.repeat_after(&prev, lp).is_some());
        assert!(b
            .repeat_after(&annexb(&[&big]), NalFraming::AnnexB)
            .is_some());
        assert!(first_vcl_prefix(&prev, lp).unwrap().len() <= MAX_SLICE_HEADER_BYTES + 1);
        // Zero-length NALs in length-prefixed framing are malformed and are
        // never an unbounded loop.
        let zeros = vec![0_u8; 1 << 20];
        assert!(b.repeat_after(&zeros, lp).is_none());
        assert!(split_nals(&zeros, lp).is_none());
        // Many non-VCL NALs before the VCL: capped.
        let aud: &[u8] = &[0x09, 0x30];
        let mut nals: Vec<&[u8]> = vec![aud; MAX_NALS_PER_AU + 1];
        nals.push(BASE_P1);
        assert!(b.repeat_after(&annexb(&nals), NalFraming::AnnexB).is_none());
        assert!(b
            .repeat_after(&frame_nals(&nals, lp).unwrap(), lp)
            .is_none());
        // The emulation counters saturate (no overflow on long zero runs).
        assert_eq!(unescape(&[0; 70_000]).len(), 70_000);
        assert!(escape(&[0; 70_000]).len() > 70_000);
    }

    #[test]
    fn truncated_parameter_sets_and_slices_return_none() {
        // T1: every truncation below the first complete parse is None, and
        // parsing is monotone in the prefix length.
        for full in [BASE_SPS, HIGH_SPS, HB_SPS] {
            let first = (0..=full.len())
                .find(|&n| parse_sps(&full[..n]).is_some())
                .unwrap();
            assert!(first > 5, "a usable SPS needs its fields");
            for n in 0..first {
                assert!(parse_sps(&full[..n]).is_none(), "SPS prefix {n}");
            }
            for n in first..=full.len() {
                assert_eq!(parse_sps(&full[..n]), parse_sps(full), "SPS prefix {n}");
            }
        }
        for full in [BASE_PPS, HIGH_PPS, HB_PPS] {
            let first = (0..=full.len())
                .find(|&n| parse_pps(&full[..n]).is_some())
                .unwrap();
            for n in 0..first {
                assert!(parse_pps(&full[..n]).is_none(), "PPS prefix {n}");
            }
        }
        let b = RepeatAuBuilder::new(HIGH_SPS, HIGH_PPS).unwrap();
        let first = (1..=HIGH_P3.len())
            .find(|&n| {
                b.repeat_after(&annexb(&[&HIGH_P3[..n]]), NalFraming::AnnexB)
                    .is_some()
            })
            .unwrap();
        assert!(first > 3);
        for n in 1..first {
            assert!(
                b.repeat_after(&annexb(&[&HIGH_P3[..n]]), NalFraming::AnnexB)
                    .is_none(),
                "slice prefix {n}"
            );
        }
    }

    #[test]
    fn cabac_tables_match_spec_literals() {
        // T1: independent spot values from ITU-T H.264 Tables 9-44 / 9-45.
        assert_eq!(RANGE_TAB_LPS[0], [128, 176, 208, 240]);
        assert_eq!(RANGE_TAB_LPS[12], [77, 94, 111, 128]);
        assert_eq!(RANGE_TAB_LPS[63], [2, 2, 2, 2]);
        assert_eq!(TRANS_IDX_LPS[..5], [0, 0, 1, 2, 2]);
        assert_eq!(TRANS_IDX_LPS[62], 38);
        assert_eq!(TRANS_IDX_LPS[63], 63);
    }
}
