//! Per-AU parameter-set probe + drift detection (GP-2, ADR-0030 §4).
//!
//! A guarded passthrough (ADR-0030 §4) pre-bakes a slate whose coded parameters
//! are **bit-identical** to the input's — same SPS/PPS/VPS (H.264/HEVC) or
//! `sequence_header` OBU (AV1) — so the slate splices cleanly into the copied
//! elementary stream. That match is only valid while the input's parameters hold.
//! The catch ([`crate::libav`] / `Demuxer::stream_parameters`, ADR-0030 §4 "No
//! clean IDR / mid-stream param change"): the container's parameter snapshot is
//! taken at **open** time and is **not** refreshed mid-stream. A mid-stream
//! resolution / profile / level / chroma / bit-depth change is visible only in
//! the **in-band** parameter-set bytes carried by later access units.
//!
//! GP-2 is the primitive that watches for that drift. [`StreamParamProbe`] takes
//! an owned [`ParamSnapshot`] of the active parameter sets (from the first GOP's
//! in-band sets, or from avcC/hvcC extradata at open time); [`diff`] then walks
//! each later access unit's in-band parameter sets and reports a [`ParamDrift`]
//! whenever an active set's **bytes change**. Bit-identical sets ⇒ no drift; an
//! access unit carrying **no** in-band parameter sets carries the snapshot
//! forward (never a false positive). A later slice consumes [`ParamDrift`] to
//! invalidate + re-bake the cached slate off the failover hot path.
//!
//! ## Purity
//!
//! The whole module is a **pure function** over `(&[u8], CodecKind, NalFraming,
//! &ParamSnapshot)` — **no** libav, **no** async, **no** allocation beyond the
//! owned snapshot bytes. It reuses the GP-1 byte-parser codec/framing types
//! ([`multiview_ffmpeg::idr::CodecKind`] / [`multiview_ffmpeg::idr::NalFraming`],
//! always compiled in the default pure-Rust build) and the same Annex-B /
//! length-prefixed / OBU framing semantics, so it is exhaustively unit-testable
//! without a decoder. Any malformed / truncated input degrades safely (no panic,
//! conservative non-drift or an isolated fresh-set drift — never a silent miss
//! that would leave a stale slate spliced in).

use multiview_ffmpeg::idr::{CodecKind, NalFraming};

/// The class of a coded parameter set, across the codecs GP-2 models.
///
/// A change to any active set of these classes invalidates a pre-baked
/// param-matched slate (ADR-0030 §4). `#[non_exhaustive]`: downstream `match`
/// must carry a wildcard so new classes can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParamSetClass {
    /// H.264 / HEVC **Sequence Parameter Set** (resolution, profile/level,
    /// chroma, bit-depth). H.264 `nal_unit_type == 7`; HEVC `== 33`.
    Sps,
    /// H.264 / HEVC **Picture Parameter Set**. H.264 `nal_unit_type == 8`;
    /// HEVC `== 34`.
    Pps,
    /// HEVC **Video Parameter Set** (`nal_unit_type == 32`). H.264 has none.
    Vps,
    /// AV1 **sequence header OBU** (`obu_type == 1`) — AV1's parameter set.
    SequenceHeader,
}

/// One identified parameter set: its class, its in-stream id (or an ordinal
/// fallback), and an owned snapshot of its bytes (the NAL/OBU payload, header
/// included, exactly as carried in-band).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParamSet {
    class: ParamSetClass,
    /// The parsed in-stream parameter-set id where the codec carries one
    /// (H.264 SPS/PPS, HEVC VPS/PPS), else an ordinal among same-class sets in
    /// the access unit. Single-instance sets (HEVC SPS, AV1 seq header) use `0`.
    id: u32,
    /// Owned bytes of the parameter set as it appears in-band (the de-framed
    /// NAL / OBU unit, header byte(s) included). The drift test is a byte
    /// comparison over exactly these.
    bytes: Vec<u8>,
}

/// An owned snapshot of a stream's active parameter sets at a point in time.
///
/// Built once from the first GOP's in-band sets ([`StreamParamProbe::snapshot_from_au`])
/// or from avcC/hvcC extradata ([`StreamParamProbe::from_extradata`]); later
/// access units are compared against it via [`diff`]. Borrows nothing, so it can
/// be stored / sent across threads while the demuxer keeps reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSnapshot {
    codec: CodecKind,
    sets: Vec<ParamSet>,
}

impl ParamSnapshot {
    /// An empty snapshot for `codec` — no parameter sets captured yet.
    ///
    /// Diffing a later access unit that **does** carry a parameter set against an
    /// empty snapshot reports drift (a set appeared): never a silent miss.
    #[must_use]
    pub fn empty(codec: CodecKind) -> Self {
        Self {
            codec,
            sets: Vec::new(),
        }
    }

    /// The codec this snapshot was taken for.
    #[must_use]
    pub fn codec(&self) -> CodecKind {
        self.codec
    }

    /// Whether the snapshot contains at least one parameter set of `class`.
    #[must_use]
    pub fn has(&self, class: ParamSetClass) -> bool {
        self.sets.iter().any(|s| s.class == class)
    }

    /// The captured bytes of the parameter set of `(class, id)`, if present.
    fn get(&self, class: ParamSetClass, id: u32) -> Option<&[u8]> {
        self.sets
            .iter()
            .find(|s| s.class == class && s.id == id)
            .map(|s| s.bytes.as_slice())
    }
}

/// The result of diffing an access unit against a [`ParamSnapshot`].
///
/// `changed` is the splice-invalidation signal: `true` means an active parameter
/// set's bytes changed (or a new active set appeared) versus the snapshot, so a
/// pre-baked param-matched slate is stale and must be re-baked. `which` names the
/// changed classes (deduplicated) for diagnostics / targeted re-bake.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ParamDrift {
    /// Whether any active parameter set drifted.
    pub changed: bool,
    /// The parameter-set classes that changed (deduplicated, snapshot order).
    pub which: Vec<ParamSetClass>,
}

impl ParamDrift {
    /// No drift detected (the cached slate stays valid).
    fn none() -> Self {
        Self {
            changed: false,
            which: Vec::new(),
        }
    }
}

/// The parameter-set probe: extracts active parameter sets from an access unit
/// or codec extradata, producing a [`ParamSnapshot`] to diff later units against.
///
/// Stateless — every method is a pure function over its bytes; this type is just
/// the namespace for the GP-2 constructors.
#[derive(Debug, Clone, Copy)]
pub struct StreamParamProbe;

impl StreamParamProbe {
    /// Snapshot the active parameter sets carried **in-band** in one access unit.
    ///
    /// Walks `au` under `framing`, collecting H.264 SPS(7)/PPS(8),
    /// HEVC VPS(32)/SPS(33)/PPS(34), or the AV1 sequence-header OBU(1). Returns
    /// [`None`] when the access unit carries no parameter set of `codec`
    /// (nothing to snapshot) or `codec` is unmodelled.
    #[must_use]
    pub fn snapshot_from_au(
        au: &[u8],
        codec: CodecKind,
        framing: NalFraming,
    ) -> Option<ParamSnapshot> {
        let sets = collect_param_sets(au, codec, framing);
        if sets.is_empty() {
            return None;
        }
        Some(ParamSnapshot { codec, sets })
    }

    /// Snapshot the active parameter sets out of avcC / hvcC **extradata**.
    ///
    /// This is the open-time snapshot (ADR-0030 §4 pre-bake): the container's
    /// codec config record carries the initial SPS/PPS (avcC) or VPS/SPS/PPS
    /// (hvcC) before any access unit is read. Returns [`None`] for AV1 (its
    /// sequence header is in-band, snapshot it from the first temporal unit via
    /// [`StreamParamProbe::snapshot_from_au`]), for an unmodelled codec, or when
    /// the extradata carries no parameter set (too short / not a config record).
    #[must_use]
    pub fn from_extradata(codec: CodecKind, extradata: &[u8]) -> Option<ParamSnapshot> {
        let sets = match codec {
            CodecKind::H264 => parse_avcc(extradata),
            CodecKind::Hevc => parse_hvcc(extradata),
            // AV1's sequence header is in-band (snapshot it from the first temporal
            // unit, not extradata); `Other` is unmodelled; `CodecKind` is
            // `#[non_exhaustive]` so the wildcard is mandatory — all yield none.
            _ => Vec::new(),
        };
        if sets.is_empty() {
            return None;
        }
        Some(ParamSnapshot { codec, sets })
    }
}

/// Diff one access unit's in-band parameter sets against a snapshot.
///
/// Reports [`ParamDrift::changed`] when an active set's bytes **changed** versus
/// `prev` (or a parameter set of a class/id absent from `prev` appeared). An
/// access unit carrying **no** in-band parameter set carries `prev` forward (no
/// drift). A set whose bytes are bit-identical to `prev` ⇒ no drift. Codec
/// mismatch between `prev` and the requested `codec`, or an unmodelled `codec`,
/// conservatively reports no drift (the snapshot is for a different stream).
#[must_use]
pub fn diff(
    prev: &ParamSnapshot,
    au_bytes: &[u8],
    codec: CodecKind,
    framing: NalFraming,
) -> ParamDrift {
    if matches!(codec, CodecKind::Other) || prev.codec != codec {
        return ParamDrift::none();
    }

    let incoming = collect_param_sets(au_bytes, codec, framing);
    if incoming.is_empty() {
        // No in-band parameter sets this AU: the previous snapshot still holds.
        return ParamDrift::none();
    }

    let mut which: Vec<ParamSetClass> = Vec::new();
    for set in &incoming {
        let drifted = match prev.get(set.class, set.id) {
            // A set of this (class, id) existed before: drift iff the bytes
            // changed. Bit-identical ⇒ no drift.
            Some(prev_bytes) => prev_bytes != set.bytes.as_slice(),
            // A new active set appeared that the snapshot never had. Against a
            // populated snapshot this is a genuine parameter change; against an
            // empty snapshot (no sets ever captured) it is the first appearance
            // — still a drift the caller must bake against.
            None => true,
        };
        if drifted && !which.contains(&set.class) {
            which.push(set.class);
        }
    }

    ParamDrift {
        changed: !which.is_empty(),
        which,
    }
}

// ---- parameter-set collection -------------------------------------------

/// Collect every parameter set of `codec` carried in-band in `au` under `framing`.
fn collect_param_sets(au: &[u8], codec: CodecKind, framing: NalFraming) -> Vec<ParamSet> {
    match codec {
        CodecKind::H264 => collect_nal_param_sets(au, framing, h264_class),
        CodecKind::Hevc => collect_nal_param_sets(au, framing, hevc_class),
        CodecKind::Av1 => collect_av1_seq_headers(au),
        // `Other` (unmodelled) and any future `#[non_exhaustive]` codec carry no
        // parameter set GP-2 understands — conservatively none (hence no drift).
        _ => Vec::new(),
    }
}

/// Map an H.264 NAL header byte to a parameter-set class (or `None`).
fn h264_class(nal: &[u8]) -> Option<ParamSetClass> {
    match nal.first().map(|&b| b & 0x1F) {
        Some(7) => Some(ParamSetClass::Sps),
        Some(8) => Some(ParamSetClass::Pps),
        _ => None,
    }
}

/// Map an HEVC NAL header (first byte) to a parameter-set class (or `None`).
fn hevc_class(nal: &[u8]) -> Option<ParamSetClass> {
    match nal.first().map(|&b| (b >> 1) & 0x3F) {
        Some(32) => Some(ParamSetClass::Vps),
        Some(33) => Some(ParamSetClass::Sps),
        Some(34) => Some(ParamSetClass::Pps),
        _ => None,
    }
}

/// Walk the NAL units of `au` (Annex-B or length-prefixed), keeping those the
/// `classify` predicate maps to a parameter-set class. Each kept NAL is keyed by
/// its parsed in-stream id (where the codec carries one) or an ordinal fallback.
fn collect_nal_param_sets(
    au: &[u8],
    framing: NalFraming,
    classify: fn(&[u8]) -> Option<ParamSetClass>,
) -> Vec<ParamSet> {
    let mut out: Vec<ParamSet> = Vec::new();
    let mut ordinals: Vec<(ParamSetClass, u32)> = Vec::new();
    for_each_nal(au, framing, |nal| {
        if let Some(class) = classify(nal) {
            let ordinal = ordinals.iter().filter(|(c, _)| *c == class).count();
            let ordinal = u32::try_from(ordinal).unwrap_or(u32::MAX);
            ordinals.push((class, ordinal));
            let id = param_set_id(class, nal).unwrap_or(ordinal);
            out.push(ParamSet {
                class,
                id,
                bytes: nal.to_vec(),
            });
        }
    });
    out
}

/// Parse the in-stream parameter-set id for the classes that carry a small,
/// fixed-position id at the front of the RBSP. Returns `None` when no stable id
/// is parseable (the caller then keys by ordinal).
///
/// * H.264 **SPS**: `profile_idc u(8)`, `constraint_set+reserved u(8)`,
///   `level_idc u(8)`, then `seq_parameter_set_id ue(v)`.
/// * H.264 **PPS**: `pic_parameter_set_id ue(v)` first.
/// * HEVC **VPS**: `vps_video_parameter_set_id u(4)` first.
/// * HEVC **PPS**: `pps_pic_parameter_set_id ue(v)` first.
/// * HEVC **SPS** / **everything else**: keyed single-instance (`0`) — the SPS id
///   sits behind a variable-length `profile_tier_level`, not worth bit-parsing for
///   a drift signal; one active SPS is the overwhelming norm and a genuine change
///   is still caught by the byte compare at the single key.
fn param_set_id(class: ParamSetClass, nal: &[u8]) -> Option<u32> {
    // RBSP begins after the NAL header (1 byte for H.264, 2 for HEVC). We pass
    // the header width via the class (HEVC classes have a 2-byte header).
    match class {
        ParamSetClass::Sps => {
            // Distinguish H.264 SPS (1-byte header, id after 3 RBSP bytes) from
            // HEVC SPS (single-instance) by header width heuristic: an H.264 SPS
            // NAL header low 5 bits == 7; HEVC SPS first byte (>>1)&0x3F == 33.
            if nal.first().is_some_and(|&b| (b & 0x1F) == 7) {
                let rbsp = nal.get(1..)?;
                let de = remove_emulation_prevention(rbsp);
                // Skip profile_idc, constraint flags, level_idc (3 bytes).
                let body = de.get(3..)?;
                let mut reader = BitReader::new(body);
                reader.read_ue()
            } else {
                Some(0)
            }
        }
        ParamSetClass::Pps => {
            // Header width: H.264 PPS header low5==8 (1 byte); HEVC PPS (2 bytes).
            let header_len = if nal.first().is_some_and(|&b| (b & 0x1F) == 8) {
                1
            } else {
                2
            };
            let rbsp = nal.get(header_len..)?;
            let de = remove_emulation_prevention(rbsp);
            let mut reader = BitReader::new(&de);
            reader.read_ue()
        }
        ParamSetClass::Vps => {
            // HEVC VPS: 2-byte NAL header, then vps_video_parameter_set_id u(4).
            let rbsp = nal.get(2..)?;
            let de = remove_emulation_prevention(rbsp);
            let mut reader = BitReader::new(&de);
            reader.read_bits(4)
        }
        ParamSetClass::SequenceHeader => Some(0),
    }
}

/// Collect AV1 sequence-header OBUs (the AV1 parameter set) from a temporal unit.
fn collect_av1_seq_headers(au: &[u8]) -> Vec<ParamSet> {
    const OBU_SEQUENCE_HEADER: u8 = 1;
    let mut out: Vec<ParamSet> = Vec::new();
    for_each_obu(au, |obu_type, payload| {
        if obu_type == OBU_SEQUENCE_HEADER {
            out.push(ParamSet {
                class: ParamSetClass::SequenceHeader,
                id: 0,
                bytes: payload.to_vec(),
            });
        }
    });
    out
}

// ---- NAL / OBU framing ---------------------------------------------------
//
// These reproduce the GP-1 byte-framing semantics (see
// `multiview_ffmpeg::idr`: Annex-B `00 00 01` start codes, length-prefixed
// avcC/hvcC, low-overhead OBU with `obu_has_size_field`) as *yielding* walkers
// — GP-1's primitives are private predicate-only short-circuiting walkers that
// cannot hand back NAL/OBU byte slices, which collecting parameter-set bytes
// requires. The framing rules are kept identical so the two stay consistent.

/// Run `visit` over each NAL unit of `au` under `framing`.
fn for_each_nal(au: &[u8], framing: NalFraming, mut visit: impl FnMut(&[u8])) {
    match framing {
        NalFraming::AnnexB => annexb_for_each_nal(au, &mut visit),
        NalFraming::LengthPrefixed { nal_length_size } => {
            length_prefixed_for_each_nal(au, nal_length_size, &mut visit);
        }
        // OBU framing never carries an H.264/HEVC NAL; any future
        // `#[non_exhaustive]` framing is likewise treated as carrying no NAL unit.
        _ => {}
    }
}

/// Iterate Annex-B NAL units (split on `00 00 01`, tolerating the 4-byte
/// `00 00 00 01` variant). The body of each NAL runs to the byte before the
/// **next** start code; a 4-byte start code has an extra leading `00` that is
/// **not** part of the preceding NAL, and any `rbsp_trailing` zero padding is
/// likewise trimmed — so the captured bytes are the meaningful NAL payload
/// exactly (start-code framing must not leak into the parameter-set snapshot).
fn annexb_for_each_nal(au: &[u8], visit: &mut impl FnMut(&[u8])) {
    // `starts[n]` is the index of the NAL body just after a `00 00 01` prefix.
    let mut starts: Vec<usize> = Vec::new();
    let mut i = 0_usize;
    while let Some(window) = au.get(i..i + 3) {
        if window == [0x00, 0x00, 0x01] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (n, &body_start) in starts.iter().enumerate() {
        // The NAL ends just before the next start code's `00 00 01` prefix (3
        // bytes back from the next body start), or at end-of-buffer for the last.
        let raw_end = starts
            .get(n + 1)
            .map_or(au.len(), |&next_start| next_start.saturating_sub(3));
        if let Some(nal) = au.get(body_start..raw_end) {
            // Trim trailing `0x00` bytes: these are either the extra leading `00`
            // of a 4-byte `00 00 00 01` start code or RBSP trailing-zero padding.
            // A valid NAL never ends on `0x00` (RBSP ends with a `1` stop bit),
            // so trimming only removes framing/padding, never payload.
            let trimmed_end = nal
                .iter()
                .rposition(|&b| b != 0x00)
                .map_or(0, |last| last + 1);
            let nal = nal.get(..trimmed_end).unwrap_or(nal);
            if !nal.is_empty() {
                visit(nal);
            }
        }
    }
}

/// Iterate length-prefixed NAL units (avcC / hvcC). A `nal_length_size` outside
/// `1..=4`, or a length running past the buffer, ends iteration safely.
fn length_prefixed_for_each_nal(au: &[u8], nal_length_size: u8, visit: &mut impl FnMut(&[u8])) {
    let size = usize::from(nal_length_size);
    if !(1..=4).contains(&size) {
        return;
    }
    let mut offset = 0_usize;
    while let Some(len_bytes) = au.get(offset..offset + size) {
        let mut nal_len = 0_usize;
        for &b in len_bytes {
            nal_len = (nal_len << 8) | usize::from(b);
        }
        let body_start = offset + size;
        let Some(body_end) = body_start.checked_add(nal_len) else {
            return;
        };
        let Some(nal) = au.get(body_start..body_end) else {
            return;
        };
        if !nal.is_empty() {
            visit(nal);
        }
        offset = body_end;
        if nal_len == 0 {
            // A zero length would loop forever; a real stream never has one.
            return;
        }
    }
}

/// Run `visit(obu_type, payload)` over each OBU of a low-overhead AV1 temporal
/// unit. An OBU without a size field (cannot advance safely) ends iteration.
fn for_each_obu(au: &[u8], mut visit: impl FnMut(u8, &[u8])) {
    let mut offset = 0_usize;
    while let Some(&header) = au.get(offset) {
        // OBU header: forbidden(1) | type(4) | extension_flag(1) | has_size(1) | reserved(1).
        let obu_type = (header >> 3) & 0x0F;
        let extension_flag = (header & 0b0000_0100) != 0;
        let has_size_field = (header & 0b0000_0010) != 0;
        let mut cursor = offset + 1;
        if extension_flag {
            // Skip the 1-byte extension header (temporal_id / spatial_id).
            if au.get(cursor).is_none() {
                return;
            }
            cursor += 1;
        }
        if !has_size_field {
            return;
        }
        let Some((obu_size, after_leb)) = read_leb128(au, cursor) else {
            return;
        };
        let payload_start = after_leb;
        let Some(payload_end) = payload_start.checked_add(obu_size) else {
            return;
        };
        let Some(payload) = au.get(payload_start..payload_end) else {
            return;
        };
        visit(obu_type, payload);
        offset = payload_end;
    }
}

/// Decode an unsigned LEB128 value at `offset` (AV1 OBU size). At most 8 bytes;
/// an overlong value or a buffer that ends mid-value yields `None`.
fn read_leb128(buf: &[u8], offset: usize) -> Option<(usize, usize)> {
    let mut value = 0_u64;
    let mut i = offset;
    for shift in 0..8_u32 {
        let &byte = buf.get(i)?;
        value |= u64::from(byte & 0x7F).checked_shl(shift * 7)?;
        i += 1;
        if byte & 0x80 == 0 {
            let value = usize::try_from(value).ok()?;
            return Some((value, i));
        }
    }
    None
}

// ---- avcC / hvcC extradata -----------------------------------------------

/// Parse an avcC (ISO/IEC 14496-15) config record into its SPS + PPS sets.
///
/// Layout: `configurationVersion(0x01)`, profile/compat/level (3 B), byte 4 low
/// 2 bits = `lengthSizeMinusOne`, byte 5 low 5 bits = `numOfSequenceParameterSets`,
/// then each SPS as `u16 length + NAL`, then a `numOfPictureParameterSets` byte,
/// then each PPS as `u16 length + NAL`. Anything malformed/short yields the sets
/// parsed so far (possibly empty).
fn parse_avcc(extradata: &[u8]) -> Vec<ParamSet> {
    let mut out: Vec<ParamSet> = Vec::new();
    // Require the 6-byte fixed header + a `0x01` version byte.
    if extradata.first() != Some(&0x01) {
        return out;
    }
    let Some(&sps_count_byte) = extradata.get(5) else {
        return out;
    };
    let sequence_param_count = usize::from(sps_count_byte & 0x1F);
    let mut offset = 6_usize;
    for _ in 0..sequence_param_count {
        let Some((nal, next)) = read_u16_prefixed(extradata, offset) else {
            return out;
        };
        push_nal_set(&mut out, nal, h264_class);
        offset = next;
    }
    let Some(&picture_param_count) = extradata.get(offset) else {
        return out;
    };
    offset += 1;
    for _ in 0..usize::from(picture_param_count) {
        let Some((nal, next)) = read_u16_prefixed(extradata, offset) else {
            return out;
        };
        push_nal_set(&mut out, nal, h264_class);
        offset = next;
    }
    out
}

/// Parse an hvcC (ISO/IEC 14496-15) config record into its VPS + SPS + PPS sets.
///
/// Layout: a 22-byte fixed header, then `numOfArrays(u8)` at offset 22; each
/// array is `array_completeness|reserved|NAL_unit_type(u8)`, `numNalus(u16)`,
/// then that many `u16 length + NAL` entries. Malformed/short yields what parsed.
fn parse_hvcc(extradata: &[u8]) -> Vec<ParamSet> {
    let mut out: Vec<ParamSet> = Vec::new();
    if extradata.first() != Some(&0x01) {
        return out;
    }
    let Some(&num_arrays) = extradata.get(22) else {
        return out;
    };
    let mut offset = 23_usize;
    for _ in 0..usize::from(num_arrays) {
        // array header byte: low 6 bits = NAL_unit_type (informational; the class
        // is derived from the NAL header itself by `hevc_class`).
        let Some(_array_header) = extradata.get(offset) else {
            return out;
        };
        let Some(count_bytes) = extradata.get(offset + 1..offset + 3) else {
            return out;
        };
        let num_nalus = usize::from(u16::from_be_bytes([
            *count_bytes.first().unwrap_or(&0),
            *count_bytes.get(1).unwrap_or(&0),
        ]));
        offset += 3;
        for _ in 0..num_nalus {
            let Some((nal, next)) = read_u16_prefixed(extradata, offset) else {
                return out;
            };
            push_nal_set(&mut out, nal, hevc_class);
            offset = next;
        }
    }
    out
}

/// Read a `u16` big-endian length-prefixed NAL at `offset`, returning the NAL
/// bytes and the offset just past it. `None` if it runs past the buffer.
fn read_u16_prefixed(buf: &[u8], offset: usize) -> Option<(&[u8], usize)> {
    let len_bytes = buf.get(offset..offset + 2)?;
    let len = usize::from(u16::from_be_bytes([
        *len_bytes.first()?,
        *len_bytes.get(1)?,
    ]));
    let body_start = offset + 2;
    let body_end = body_start.checked_add(len)?;
    let nal = buf.get(body_start..body_end)?;
    Some((nal, body_end))
}

/// Classify a NAL from extradata and, if it is a parameter set, push it keyed by
/// its parsed id (or an ordinal fallback among same-class sets already pushed).
fn push_nal_set(out: &mut Vec<ParamSet>, nal: &[u8], classify: fn(&[u8]) -> Option<ParamSetClass>) {
    if nal.is_empty() {
        return;
    }
    if let Some(class) = classify(nal) {
        let ordinal = out.iter().filter(|s| s.class == class).count();
        let ordinal = u32::try_from(ordinal).unwrap_or(u32::MAX);
        let id = param_set_id(class, nal).unwrap_or(ordinal);
        out.push(ParamSet {
            class,
            id,
            bytes: nal.to_vec(),
        });
    }
}

// ---- bitstream Exp-Golomb reader -----------------------------------------

/// Strip H.264/HEVC emulation-prevention bytes (`00 00 03` → `00 00`) from an
/// RBSP so the bit reader sees the raw syntax. Bounded; allocates one `Vec`.
fn remove_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len());
    let mut zeros = 0_usize;
    for &b in rbsp {
        if zeros >= 2 && b == 0x03 {
            // Drop the emulation-prevention byte; reset the run.
            zeros = 0;
            continue;
        }
        if b == 0x00 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// A minimal big-endian bit reader for Exp-Golomb parsing of parameter-set ids.
/// Reads never panic: past the end of the buffer they yield `None`.
struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    /// Read one bit (MSB-first), or `None` past the end.
    fn read_bit(&mut self) -> Option<u32> {
        let byte_index = self.bit_pos / 8;
        let bit_index = 7 - (self.bit_pos % 8);
        let &byte = self.bytes.get(byte_index)?;
        self.bit_pos += 1;
        Some(u32::from((byte >> bit_index) & 1))
    }

    /// Read `n` bits (`n <= 32`) as an unsigned big-endian value, or `None`.
    fn read_bits(&mut self, n: u32) -> Option<u32> {
        let mut value = 0_u32;
        for _ in 0..n {
            let bit = self.read_bit()?;
            value = (value << 1) | bit;
        }
        Some(value)
    }

    /// Read an unsigned Exp-Golomb (`ue(v)`) code. Caps the leading-zero run at
    /// 32 to stay bounded on malformed input (yields `None` past that).
    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0_u32;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 32 {
                // Bound the run on malformed/all-zero input rather than looping.
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        // value = 2^leading_zeros - 1 + suffix.
        let base = 1_u32.checked_shl(leading_zeros)?.checked_sub(1)?;
        base.checked_add(suffix)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::{remove_emulation_prevention, BitReader};

    #[test]
    fn bitreader_reads_fixed_bits() {
        // 0b1010_0110 -> read 4 bits = 0b1010 = 10.
        let mut r = BitReader::new(&[0b1010_0110]);
        assert_eq!(r.read_bits(4), Some(0b1010));
        assert_eq!(r.read_bits(4), Some(0b0110));
        assert_eq!(
            r.read_bits(1),
            None,
            "past the end yields None, never panics"
        );
    }

    #[test]
    fn ue_decodes_exp_golomb() {
        // ue: '1' -> 0; '010' -> 1; '011' -> 2; '00100' -> 3.
        assert_eq!(BitReader::new(&[0b1000_0000]).read_ue(), Some(0));
        assert_eq!(BitReader::new(&[0b0100_0000]).read_ue(), Some(1));
        assert_eq!(BitReader::new(&[0b0110_0000]).read_ue(), Some(2));
        assert_eq!(BitReader::new(&[0b0010_0000]).read_ue(), Some(3));
    }

    #[test]
    fn ue_all_zeros_is_bounded_none() {
        // A long zero run must terminate (cap), never loop forever.
        let zeros = [0_u8; 8];
        assert_eq!(BitReader::new(&zeros).read_ue(), None);
    }

    #[test]
    fn emulation_prevention_bytes_are_stripped() {
        // 00 00 03 01 -> 00 00 01 (the 03 is dropped); a 03 not after 00 00 stays.
        assert_eq!(
            remove_emulation_prevention(&[0x00, 0x00, 0x03, 0x01]),
            vec![0x00, 0x00, 0x01]
        );
        assert_eq!(
            remove_emulation_prevention(&[0x01, 0x03, 0x02]),
            vec![0x01, 0x03, 0x02]
        );
    }
}
